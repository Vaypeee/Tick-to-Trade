//! Aggressive order flow: who was taking liquidity, when, and how hard.
//!
//! The second hypothesis says informed traders move the book before the
//! public broadcast shows the event. A mid-price move is the *consequence* of
//! that; the trading itself is the cause, and it is visible earlier and more
//! sharply. A burst of one-sided taking is the signature worth looking for.
//!
//! Trades are classified by where they printed relative to the touch rather
//! than by trusting the `side` field on the message. Both readings are kept
//! and compared, for the same reason the book is rebuilt rather than taken
//! from the exchange's own `best_bid`: a field whose meaning has been assumed
//! rather than checked is a quiet way to be wrong for months.

use serde_json::Value;

use super::align::{Quote, Signal};
use super::replay::{Kind, Record};

/// Which side crossed the spread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taker {
    /// Printed at or above the ask: somebody bought into resting offers.
    Buy,
    /// Printed at or below the bid: somebody sold into resting bids.
    Sell,
    /// Printed inside the spread, or with no book to compare against.
    Unclear,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    /// When the recorder saw it, not the exchange timestamp on the message.
    /// Everything else in this project is measured on observation, and mixing
    /// the two clocks would put an unmeasured offset inside the result.
    pub at_unix_ns: u64,
    pub asset: String,
    pub price: f64,
    pub size: f64,
    pub taker: Taker,
    /// What the message itself claimed, for comparison.
    pub reported_side: Option<String>,
}

/// How often the classification agreed with the `side` field on the message.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SideAgreement {
    pub checked: u64,
    pub matched: u64,
    /// Trades that printed inside the spread, which the rule cannot classify.
    pub unclear: u64,
}

impl SideAgreement {
    pub fn rate(&self) -> Option<f64> {
        (self.checked > 0).then(|| self.matched as f64 / self.checked as f64)
    }
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

/// The touch as it stood at `at_unix_ns`, from the replayed quote series.
pub fn touch_at(quotes: &[Quote], asset: &str, at_unix_ns: u64) -> Option<(f64, f64)> {
    quotes
        .iter()
        .rev()
        .find(|quote| quote.asset == asset && quote.at_unix_ns <= at_unix_ns)
        .map(|quote| (quote.bid, quote.ask))
}

/// Extracts trades from a market recording and classifies each one.
pub fn trades(records: &[Record], quotes: &[Quote]) -> (Vec<Trade>, SideAgreement) {
    let mut found = Vec::new();
    let mut agreement = SideAgreement::default();

    for record in records.iter().filter(|record| record.kind == Kind::Data) {
        let Some(payload) = &record.payload else {
            continue;
        };
        let events: Vec<&Value> = match payload {
            Value::Array(events) => events.iter().collect(),
            other => vec![other],
        };

        for event in events {
            if event.get("event_type").and_then(Value::as_str) != Some("last_trade_price") {
                continue;
            }
            let (Some(asset), Some(price), Some(size)) = (
                event.get("asset_id").and_then(Value::as_str),
                as_f64(event.get("price")),
                as_f64(event.get("size")),
            ) else {
                continue;
            };

            let taker = match touch_at(quotes, asset, record.t_unix_ns) {
                Some((bid, ask)) if super::ticks(price) >= super::ticks(ask) => Taker::Buy,
                Some((bid, _)) if super::ticks(price) <= super::ticks(bid) => Taker::Sell,
                _ => Taker::Unclear,
            };

            let reported = event
                .get("side")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase);
            match (&taker, reported.as_deref()) {
                (Taker::Unclear, _) => agreement.unclear += 1,
                (_, Some(side)) => {
                    agreement.checked += 1;
                    let same = (taker == Taker::Buy && side == "BUY")
                        || (taker == Taker::Sell && side == "SELL");
                    if same {
                        agreement.matched += 1;
                    }
                }
                (_, None) => {}
            }

            found.push(Trade {
                at_unix_ns: record.t_unix_ns,
                asset: asset.to_owned(),
                price,
                size,
                taker,
                reported_side: reported,
            });
        }
    }

    (found, agreement)
}

/// A burst of one-sided taking.
#[derive(Debug, Clone, PartialEq)]
pub struct Surge {
    /// The trade that carried the net past the threshold, which is the first
    /// instant the burst was visible as one. Dating it from the first trade
    /// of the window would credit the signal with information nobody had
    /// yet; waiting for the burst to finish would do the same.
    pub at_unix_ns: u64,
    pub asset: String,
    /// Positive when buyers were lifting, negative when sellers were hitting.
    pub signed_size: f64,
    pub trades: usize,
}

impl Surge {
    pub fn signal(&self) -> Signal {
        Signal {
            at_unix_ns: self.at_unix_ns,
            rose: self.signed_size > 0.0,
            detail: format!("{:>+11.0} over {} trade(s)", self.signed_size, self.trades),
        }
    }
}

pub fn surge_signals(surges: &[Surge]) -> Vec<Signal> {
    surges.iter().map(Surge::signal).collect()
}

/// Finds each burst whose net taking over the preceding `window_ns` reached
/// `min_size`.
///
/// The window is trailing, not forward-looking. Anchoring it at a trade and
/// summing onwards would let the scan step past an offsetting trade and then
/// report the other side on its own: a buy and a matching sell would come out
/// as a sell surge. Looking backwards from each trade keeps whatever already
/// happened inside the window, which is what nets it out.
///
/// Netting is the point. Two sides trading with each other is not somebody
/// acting on information; only the imbalance is.
///
/// After a burst is reported the scan is quiet for one window, so a single
/// event produces one signal rather than one per trade in its tail.
pub fn surges(trades: &[Trade], asset: &str, window_ns: u64, min_size: f64) -> Vec<Surge> {
    let relevant: Vec<&Trade> = trades
        .iter()
        .filter(|trade| trade.asset == asset && trade.taker != Taker::Unclear)
        .collect();

    let signed = |trade: &Trade| match trade.taker {
        Taker::Buy => trade.size,
        Taker::Sell => -trade.size,
        Taker::Unclear => 0.0,
    };

    let mut found: Vec<Surge> = Vec::new();
    let mut quiet_until: Option<u64> = None;

    for (index, trade) in relevant.iter().enumerate() {
        if quiet_until.is_some_and(|until| trade.at_unix_ns <= until) {
            continue;
        }
        let from = trade.at_unix_ns.saturating_sub(window_ns);

        let mut net = 0.0;
        let mut counted = 0usize;
        for earlier in relevant[..=index].iter().rev() {
            if earlier.at_unix_ns < from {
                break;
            }
            net += signed(earlier);
            counted += 1;
        }

        if net.abs() >= min_size {
            found.push(Surge {
                at_unix_ns: trade.at_unix_ns,
                asset: asset.to_owned(),
                signed_size: net,
                trades: counted,
            });
            quiet_until = Some(trade.at_unix_ns.saturating_add(window_ns));
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn quote(at: u64, bid: f64, ask: f64) -> Quote {
        Quote {
            at_unix_ns: at,
            asset: "A".into(),
            bid,
            ask,
            bid_size: 1_000.0,
            ask_size: 1_000.0,
            mid: (bid + ask) / 2.0,
        }
    }

    fn record(seq: u64, at: u64, price: &str, size: &str, side: &str) -> Record {
        serde_json::from_value(json!({
            "seq": seq,
            "t_unix_ns": at,
            "t_mono_ns": at,
            "kind": "data",
            "payload": {
                "event_type": "last_trade_price",
                "asset_id": "A",
                "price": price,
                "size": size,
                "side": side,
            },
        }))
        .unwrap()
    }

    #[test]
    fn a_trade_at_the_ask_is_a_buyer_crossing() {
        let quotes = [quote(100, 0.40, 0.42)];
        let (found, _) = trades(&[record(1, 200, "0.42", "100", "BUY")], &quotes);
        assert_eq!(found[0].taker, Taker::Buy);
    }

    #[test]
    fn a_trade_at_the_bid_is_a_seller_crossing() {
        let quotes = [quote(100, 0.40, 0.42)];
        let (found, _) = trades(&[record(1, 200, "0.40", "100", "SELL")], &quotes);
        assert_eq!(found[0].taker, Taker::Sell);
    }

    #[test]
    fn a_trade_inside_the_spread_is_not_guessed_at() {
        let quotes = [quote(100, 0.40, 0.44)];
        let (found, agreement) = trades(&[record(1, 200, "0.42", "100", "BUY")], &quotes);
        assert_eq!(found[0].taker, Taker::Unclear);
        assert_eq!(agreement.unclear, 1);
        assert_eq!(agreement.checked, 0);
    }

    #[test]
    fn with_no_book_yet_nothing_is_classified() {
        let (found, _) = trades(&[record(1, 200, "0.42", "100", "BUY")], &[]);
        assert_eq!(found[0].taker, Taker::Unclear);
    }

    #[test]
    fn the_reported_side_is_compared_rather_than_trusted() {
        let quotes = [quote(100, 0.40, 0.42)];
        // Printed at the ask, so a buyer crossed, whatever the field claims.
        let (_, agreement) = trades(&[record(1, 200, "0.42", "100", "SELL")], &quotes);
        assert_eq!(agreement.checked, 1);
        assert_eq!(agreement.matched, 0);
        assert_eq!(agreement.rate(), Some(0.0));
    }

    fn trade(at: u64, size: f64, taker: Taker) -> Trade {
        Trade {
            at_unix_ns: at,
            asset: "A".into(),
            price: 0.41,
            size,
            taker,
            reported_side: None,
        }
    }

    #[test]
    fn one_sided_taking_inside_the_window_is_a_surge() {
        let found = surges(
            &[
                trade(100, 400.0, Taker::Buy),
                trade(200, 400.0, Taker::Buy),
                trade(300, 400.0, Taker::Buy),
            ],
            "A",
            1_000,
            1_000.0,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].signed_size, 1_200.0);
        assert_eq!(found[0].trades, 3);
        // Dated from the last trade: that is when the burst became visible.
        assert_eq!(found[0].at_unix_ns, 300);
    }

    #[test]
    fn two_sides_trading_with_each_other_net_out() {
        // Neither side reaches the threshold alone, and together they cancel,
        // so this is a market trading with itself rather than a signal.
        let found = surges(
            &[
                trade(100, 600.0, Taker::Buy),
                trade(200, 600.0, Taker::Sell),
            ],
            "A",
            1_000,
            1_000.0,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn an_offsetting_trade_cannot_be_stepped_over() {
        // A forward-looking window would move past the buy and report the
        // sell on its own. The trailing window keeps the buy in view.
        let found = surges(
            &[
                trade(100, 900.0, Taker::Buy),
                trade(200, 900.0, Taker::Sell),
                trade(300, 900.0, Taker::Sell),
            ],
            "A",
            1_000,
            1_000.0,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn selling_pressure_surges_negative() {
        let found = surges(
            &[
                trade(100, 800.0, Taker::Sell),
                trade(150, 800.0, Taker::Sell),
            ],
            "A",
            1_000,
            1_000.0,
        );
        assert_eq!(found[0].signed_size, -1_600.0);
        assert!(!found[0].signal().rose);
    }

    #[test]
    fn trading_spread_beyond_the_window_does_not_qualify() {
        let found = surges(
            &[trade(0, 900.0, Taker::Buy), trade(5_000, 900.0, Taker::Buy)],
            "A",
            1_000,
            1_000.0,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn one_burst_is_reported_once_not_per_sub_window() {
        let found = surges(
            &[
                trade(100, 600.0, Taker::Buy),
                trade(200, 600.0, Taker::Buy),
                trade(300, 600.0, Taker::Buy),
                trade(400, 600.0, Taker::Buy),
            ],
            "A",
            1_000,
            1_000.0,
        );
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn another_outcome_is_not_mixed_in() {
        let mut other = trade(100, 5_000.0, Taker::Buy);
        other.asset = "B".into();
        assert!(surges(&[other], "A", 1_000, 1_000.0).is_empty());
    }
}
