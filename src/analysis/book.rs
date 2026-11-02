//! Order book reconstruction from a Polymarket recording.
//!
//! The market channel sends a `book` snapshot and then `price_change` deltas.
//! Replaying them gives a mid price at every instant the market moved, which
//! is the series the lag measurement is made against.
//!
//! Each delta also carries the exchange's own `best_bid` and `best_ask`. The
//! replay does not take those at face value: it rebuilds the book from the
//! levels and then checks its answer against them. The live pipeline will
//! depend on exactly this logic to price the spread it is about to cross, so
//! a disagreement here is worth finding now, against a recording, rather than
//! later against money.

use std::collections::BTreeMap;

use serde_json::Value;

use super::align::Quote;
use super::replay::{Kind, Record};

/// Prices are a few discrete cents, so they are kept as integer thousandths.
/// Floating point keys would make two spellings of the same price — `0.41`
/// and `0.410` — into two different levels.
type Ticks = i64;

use super::ticks;

fn price(ticks: Ticks) -> f64 {
    ticks as f64 / 1000.0
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

/// One side of one outcome token.
#[derive(Debug, Default, Clone)]
pub struct Book {
    bids: BTreeMap<Ticks, f64>,
    asks: BTreeMap<Ticks, f64>,
}

impl Book {
    /// Replaces the book wholesale. A snapshot supersedes whatever was there.
    pub fn apply_snapshot(&mut self, bids: &[Value], asks: &[Value]) {
        self.bids.clear();
        self.asks.clear();
        for level in bids {
            self.set(true, level);
        }
        for level in asks {
            self.set(false, level);
        }
    }

    /// Applies one `price_changes` entry.
    pub fn apply_change(&mut self, side: &str, level: &Value) {
        self.set(side.eq_ignore_ascii_case("buy"), level);
    }

    fn set(&mut self, is_bid: bool, level: &Value) {
        let (Some(price), Some(size)) = (as_f64(level.get("price")), as_f64(level.get("size")))
        else {
            return;
        };
        let side = if is_bid {
            &mut self.bids
        } else {
            &mut self.asks
        };
        // A size of zero is a removal, not a level that is merely empty.
        // Keeping it would leave a phantom best price in the book.
        if size <= 0.0 {
            side.remove(&ticks(price));
        } else {
            side.insert(ticks(price), size);
        }
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.keys().next_back().copied().map(price)
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.keys().next().copied().map(price)
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()? + self.best_ask()?) / 2.0)
    }
}

/// How often the rebuilt book agreed with the exchange about the touch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Agreement {
    pub checked: u64,
    pub matched: u64,
}

impl Agreement {
    pub fn rate(&self) -> Option<f64> {
        (self.checked > 0).then(|| self.matched as f64 / self.checked as f64)
    }
}

/// Replays a market recording into a mid-price series.
///
/// Quotes are emitted only when the mid actually moved: a book update that
/// leaves the touch untouched is not a repricing, and counting it as one
/// would make the market look faster than it is.
pub fn replay(records: &[Record]) -> (Vec<Quote>, Agreement) {
    let mut books: BTreeMap<String, Book> = BTreeMap::new();
    let mut last_mid: BTreeMap<String, f64> = BTreeMap::new();
    let mut quotes = Vec::new();
    let mut agreement = Agreement::default();

    for record in records.iter().filter(|record| record.kind == Kind::Data) {
        let Some(payload) = &record.payload else {
            continue;
        };
        // The channel sends either one event or a batch of them.
        let events: Vec<&Value> = match payload {
            Value::Array(events) => events.iter().collect(),
            other => vec![other],
        };

        for event in events {
            let mut touched: Vec<String> = Vec::new();
            match event.get("event_type").and_then(Value::as_str) {
                Some("book") => {
                    let Some(asset) = event.get("asset_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let empty = Vec::new();
                    let bids = event
                        .get("bids")
                        .and_then(Value::as_array)
                        .unwrap_or(&empty);
                    let asks = event
                        .get("asks")
                        .and_then(Value::as_array)
                        .unwrap_or(&empty);
                    books
                        .entry(asset.to_owned())
                        .or_default()
                        .apply_snapshot(bids, asks);
                    touched.push(asset.to_owned());
                }
                Some("price_change") => {
                    let Some(changes) = event.get("price_changes").and_then(Value::as_array) else {
                        continue;
                    };
                    for change in changes {
                        let (Some(asset), Some(side)) = (
                            change.get("asset_id").and_then(Value::as_str),
                            change.get("side").and_then(Value::as_str),
                        ) else {
                            continue;
                        };
                        let book = books.entry(asset.to_owned()).or_default();
                        book.apply_change(side, change);

                        // Cross-check against what the exchange says the touch
                        // is, now that the delta has been applied.
                        if let (Some(reported_bid), Some(reported_ask)) = (
                            as_f64(change.get("best_bid")),
                            as_f64(change.get("best_ask")),
                        ) {
                            agreement.checked += 1;
                            let agrees = book.best_bid().map(ticks) == Some(ticks(reported_bid))
                                && book.best_ask().map(ticks) == Some(ticks(reported_ask));
                            if agrees {
                                agreement.matched += 1;
                            }
                        }
                        touched.push(asset.to_owned());
                    }
                }
                // `last_trade_price` and the rest do not change the book.
                _ => {}
            }

            for asset in touched {
                let Some(mid) = books.get(&asset).and_then(Book::mid) else {
                    continue;
                };
                if last_mid.get(&asset).map(|previous| ticks(*previous)) == Some(ticks(mid)) {
                    continue;
                }
                last_mid.insert(asset.clone(), mid);
                quotes.push(Quote {
                    at_unix_ns: record.t_unix_ns,
                    asset,
                    mid,
                });
            }
        }
    }

    (quotes, agreement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn level(price: &str, size: &str) -> Value {
        json!({"price": price, "size": size})
    }

    #[test]
    fn a_snapshot_sets_the_touch() {
        let mut book = Book::default();
        book.apply_snapshot(
            &[level("0.40", "10"), level("0.41", "5")],
            &[level("0.43", "7"), level("0.42", "9")],
        );
        assert_eq!(book.best_bid(), Some(0.41));
        assert_eq!(book.best_ask(), Some(0.42));
        assert_eq!(book.mid(), Some(0.415));
    }

    #[test]
    fn a_snapshot_replaces_rather_than_merges() {
        let mut book = Book::default();
        book.apply_snapshot(&[level("0.10", "1")], &[level("0.90", "1")]);
        book.apply_snapshot(&[level("0.40", "1")], &[level("0.60", "1")]);
        assert_eq!(book.best_bid(), Some(0.40));
        assert_eq!(book.best_ask(), Some(0.60));
    }

    #[test]
    fn a_zero_size_removes_the_level_instead_of_emptying_it() {
        let mut book = Book::default();
        book.apply_snapshot(&[level("0.40", "10"), level("0.41", "5")], &[]);
        book.apply_change("BUY", &level("0.41", "0"));
        assert_eq!(book.best_bid(), Some(0.40));
    }

    #[test]
    fn trailing_zeros_are_the_same_price_level() {
        let mut book = Book::default();
        book.apply_snapshot(&[level("0.41", "5")], &[]);
        book.apply_change("buy", &level("0.410", "0"));
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn a_one_sided_book_has_no_mid() {
        let mut book = Book::default();
        book.apply_snapshot(&[level("0.40", "1")], &[]);
        assert!(book.mid().is_none());
    }

    fn record(seq: u64, at: u64, payload: Value) -> Record {
        serde_json::from_value(json!({
            "seq": seq,
            "t_unix_ns": at,
            "t_mono_ns": at,
            "kind": "data",
            "payload": payload,
        }))
        .unwrap()
    }

    #[test]
    fn replay_emits_a_quote_only_when_the_mid_moves() {
        let snapshot = json!({
            "event_type": "book",
            "asset_id": "A",
            "bids": [level("0.40", "10")],
            "asks": [level("0.42", "10")],
        });
        // Adds depth behind the touch: the mid is unchanged, so this is not a
        // repricing and must not be counted as one.
        let deep = json!({
            "event_type": "price_change",
            "price_changes": [{
                "asset_id": "A", "side": "BUY", "price": "0.30", "size": "99",
                "best_bid": "0.40", "best_ask": "0.42"
            }],
        });
        let lifts = json!({
            "event_type": "price_change",
            "price_changes": [{
                "asset_id": "A", "side": "BUY", "price": "0.41", "size": "5",
                "best_bid": "0.41", "best_ask": "0.42"
            }],
        });

        let records = vec![
            record(1, 100, snapshot),
            record(2, 200, deep),
            record(3, 300, lifts),
        ];
        let (quotes, agreement) = replay(&records);

        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].at_unix_ns, 100);
        assert_eq!(quotes[1].at_unix_ns, 300);
        assert_eq!(quotes[1].mid, 0.415);
        assert_eq!(
            agreement,
            Agreement {
                checked: 2,
                matched: 2
            }
        );
        assert_eq!(agreement.rate(), Some(1.0));
    }

    #[test]
    fn disagreement_with_the_exchange_is_counted_not_hidden() {
        let records = vec![
            record(
                1,
                100,
                json!({
                    "event_type": "book", "asset_id": "A",
                    "bids": [level("0.40", "10")], "asks": [level("0.42", "10")],
                }),
            ),
            record(
                2,
                200,
                json!({
                    "event_type": "price_change",
                    "price_changes": [{
                        "asset_id": "A", "side": "BUY", "price": "0.41", "size": "5",
                        "best_bid": "0.88", "best_ask": "0.42"
                    }],
                }),
            ),
        ];
        let (_, agreement) = replay(&records);
        assert_eq!(
            agreement,
            Agreement {
                checked: 1,
                matched: 0
            }
        );
    }

    #[test]
    fn batched_events_are_unwrapped() {
        let batch = json!([{
            "event_type": "book", "asset_id": "A",
            "bids": [level("0.40", "1")], "asks": [level("0.60", "1")],
        }]);
        let (quotes, _) = replay(&[record(1, 10, batch)]);
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].mid, 0.5);
    }
}
