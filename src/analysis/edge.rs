//! What following the flow would actually have paid.
//!
//! Detecting a surge is not an edge. The edge, if there is one, is whatever
//! the price does *after* the surge, minus what it costs to get in and out.
//! This replays that: for each detected surge, enter after a reaction delay,
//! hold, and close.
//!
//! Three things here are deliberately pessimistic, because each is a way a
//! backtest flatters itself into a strategy that loses money live:
//!
//! - **Entry and exit cross the spread.** Buying pays the ask and selling
//!   receives the bid. A mid-to-mid study would book half the spread twice as
//!   profit that nobody ever earns.
//! - **Entry is late on purpose.** You cannot trade at the instant of the
//!   surge, so the delay is a parameter and the result is reported as a curve
//!   against it. That curve is the answer to how much speed is worth here.
//! - **Nothing is assumed about size.** Every fill is at the touch, which
//!   overstates what a thin book can absorb. The result is therefore an upper
//!   bound, and is labelled as one.

use super::align::Quote;
use super::flow::{Surge, touch_at};

/// One round trip taken on a surge.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    pub surge_at_unix_ns: u64,
    pub entered_at_unix_ns: u64,
    /// True when the surge was buying pressure and the trial went long.
    pub long: bool,
    /// Price paid to get in, after crossing.
    pub entry: f64,
    /// Price received to get out, after crossing back.
    pub exit: f64,
    /// Exit minus entry in the direction taken, before costs.
    pub gross: f64,
    /// Gross less fees, in probability points per unit of size.
    pub net: f64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Study {
    pub trials: Vec<Trial>,
    /// Surges with no quote to enter or exit against, which are skipped
    /// rather than filled at a price that was never shown.
    pub skipped: usize,
}

impl Study {
    pub fn wins(&self) -> usize {
        self.trials.iter().filter(|trial| trial.net > 0.0).count()
    }

    /// Median net, in probability points.
    pub fn median_net(&self) -> Option<f64> {
        let mut nets: Vec<f64> = self.trials.iter().map(|trial| trial.net).collect();
        if nets.is_empty() {
            return None;
        }
        nets.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        Some(nets[nets.len() / 2])
    }

    pub fn total_net(&self) -> f64 {
        self.trials.iter().map(|trial| trial.net).sum()
    }

    pub fn worst(&self) -> Option<f64> {
        self.trials
            .iter()
            .map(|trial| trial.net)
            .fold(None, |worst, net| {
                Some(match worst {
                    Some(value) if value < net => value,
                    _ => net,
                })
            })
    }
}

/// Replays every surge as a round trip.
///
/// `fee_fraction` is charged on both legs, as a fraction of notional. Leave it
/// at zero only if the venue genuinely takes nothing.
pub fn study(
    surges: &[Surge],
    quotes: &[Quote],
    asset: &str,
    react_ns: u64,
    hold_ns: u64,
    fee_fraction: f64,
) -> Study {
    let mut trials = Vec::new();
    let mut skipped = 0;

    for surge in surges.iter().filter(|surge| surge.asset == asset) {
        let entered_at = surge.at_unix_ns.saturating_add(react_ns);
        let closed_at = entered_at.saturating_add(hold_ns);

        let (Some((entry_bid, entry_ask)), Some((exit_bid, exit_ask))) = (
            touch_at(quotes, asset, entered_at),
            touch_at(quotes, asset, closed_at),
        ) else {
            skipped += 1;
            continue;
        };

        let long = surge.signed_size > 0.0;
        // Crossing both ways: buy the ask, sell the bid, and the reverse for
        // a short. Anything kinder than this is a fill nobody was offered.
        let (entry, exit) = if long {
            (entry_ask, exit_bid)
        } else {
            (entry_bid, exit_ask)
        };
        let gross = if long { exit - entry } else { entry - exit };
        let net = gross - fee_fraction * (entry + exit);

        trials.push(Trial {
            surge_at_unix_ns: surge.at_unix_ns,
            entered_at_unix_ns: entered_at,
            long,
            entry,
            exit,
            gross,
            net,
        });
    }

    Study { trials, skipped }
}

/// The reaction delays the curve is reported over, in milliseconds.
///
/// Spread wide on purpose: the question is not whether 10 ms beats 20 ms, but
/// whether the edge survives being a second late at all. If it does, the
/// original premise of this project — that speed is what matters — is wrong,
/// and the effort belongs in the model instead.
pub const REACTION_LADDER_MS: [u64; 6] = [0, 250, 1_000, 3_000, 10_000, 30_000];

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(at: u64, bid: f64, ask: f64) -> Quote {
        Quote {
            at_unix_ns: at,
            asset: "A".into(),
            bid,
            ask,
            mid: (bid + ask) / 2.0,
        }
    }

    fn surge(at: u64, signed: f64) -> Surge {
        Surge {
            at_unix_ns: at,
            asset: "A".into(),
            signed_size: signed,
            trades: 1,
        }
    }

    fn quotes() -> Vec<Quote> {
        vec![
            quote(0, 0.40, 0.42),
            quote(100, 0.40, 0.42),
            quote(200, 0.45, 0.47),
            quote(300, 0.50, 0.52),
        ]
    }

    #[test]
    fn a_long_pays_the_ask_and_receives_the_bid() {
        let result = study(&[surge(100, 500.0)], &quotes(), "A", 0, 100, 0.0);
        let trial = &result.trials[0];
        assert!(trial.long);
        assert_eq!(trial.entry, 0.42);
        assert_eq!(trial.exit, 0.45);
        // A two-cent spread eats most of a five-cent move.
        assert!((trial.gross - 0.03).abs() < 1e-9);
    }

    #[test]
    fn a_short_pays_the_spread_the_other_way() {
        let falling = vec![
            quote(0, 0.50, 0.52),
            quote(100, 0.50, 0.52),
            quote(200, 0.40, 0.42),
        ];
        let result = study(&[surge(100, -500.0)], &falling, "A", 0, 100, 0.0);
        let trial = &result.trials[0];
        assert!(!trial.long);
        assert_eq!(trial.entry, 0.50);
        assert_eq!(trial.exit, 0.42);
        assert!((trial.gross - 0.08).abs() < 1e-9);
    }

    #[test]
    fn a_move_smaller_than_the_spread_loses() {
        // Touch never moves: crossing in and back out costs the full spread.
        let flat = vec![quote(0, 0.40, 0.42), quote(500, 0.40, 0.42)];
        let result = study(&[surge(0, 500.0)], &flat, "A", 0, 100, 0.0);
        assert!((result.trials[0].gross + 0.02).abs() < 1e-9);
        assert_eq!(result.wins(), 0);
    }

    #[test]
    fn reacting_late_gives_up_the_move() {
        // The move completes between 100 and 200 and then stops, which is the
        // case that matters: arriving after it buys the top.
        let done = vec![
            quote(0, 0.40, 0.42),
            quote(100, 0.40, 0.42),
            quote(200, 0.50, 0.52),
            quote(300, 0.50, 0.52),
        ];
        let prompt = study(&[surge(100, 500.0)], &done, "A", 0, 100, 0.0);
        let late = study(&[surge(100, 500.0)], &done, "A", 100, 100, 0.0);

        assert!(prompt.trials[0].net > 0.0);
        // Entering at the new ask and leaving at the same bid loses the spread.
        assert!(late.trials[0].entry > prompt.trials[0].entry);
        assert!(late.trials[0].net < 0.0);
    }

    #[test]
    fn fees_are_charged_on_both_legs() {
        let free = study(&[surge(100, 500.0)], &quotes(), "A", 0, 100, 0.0);
        let charged = study(&[surge(100, 500.0)], &quotes(), "A", 0, 100, 0.01);
        let expected = free.trials[0].net - 0.01 * (0.42 + 0.45);
        assert!((charged.trials[0].net - expected).abs() < 1e-9);
    }

    #[test]
    fn a_surge_with_no_quote_to_trade_against_is_skipped() {
        let result = study(
            &[surge(0, 500.0)],
            &[quote(500, 0.4, 0.42)],
            "A",
            0,
            100,
            0.0,
        );
        assert!(result.trials.is_empty());
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn another_outcome_is_not_traded() {
        let mut elsewhere = surge(100, 500.0);
        elsewhere.asset = "B".into();
        assert!(
            study(&[elsewhere], &quotes(), "A", 0, 100, 0.0)
                .trials
                .is_empty()
        );
    }

    #[test]
    fn summary_statistics_describe_the_trials() {
        let result = study(
            &[surge(100, 500.0), surge(200, 500.0)],
            &quotes(),
            "A",
            0,
            100,
            0.0,
        );
        assert_eq!(result.trials.len(), 2);
        assert!(result.median_net().is_some());
        assert!(result.worst().is_some());
        assert!(
            (result.total_net() - result.trials.iter().map(|t| t.net).sum::<f64>()).abs() < 1e-9
        );
    }

    #[test]
    fn an_empty_study_reports_nothing_rather_than_zero() {
        let empty = Study::default();
        assert!(empty.median_net().is_none());
        assert!(empty.worst().is_none());
        assert_eq!(empty.wins(), 0);
    }
}
