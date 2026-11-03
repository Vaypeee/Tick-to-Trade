//! Replays a recording through the decision layer and keeps the books.
//!
//! No orders, no network: it reads what the market actually showed and asks
//! what the rules would have done with it. Positions are sequential — one at
//! a time, opened on a signal and closed after a fixed hold — because a
//! backtest that can hold several overlapping positions hides which signal
//! earned what.
//!
//! The expected move is an input, not a belief this code holds. Sweeping it
//! answers the question worth asking: how right would a model have to be for
//! these rules to break even?

use std::collections::BTreeMap;

use super::decision::{Decision, Limits, Refusal, Side, Touch};

/// One completed round trip.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub entered_unix_ns: u64,
    pub exited_unix_ns: u64,
    pub side: Side,
    pub entry: f64,
    pub exit: f64,
    pub size: f64,
    /// Signed result after fees, in probability points times size.
    pub net: f64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Run {
    pub trades: Vec<Trade>,
    /// How many signals each rule turned away.
    pub refusals: BTreeMap<String, usize>,
    /// Signals that passed the rules but had no book to close against.
    pub abandoned: usize,
}

impl Run {
    pub fn net(&self) -> f64 {
        self.trades.iter().map(|trade| trade.net).sum()
    }

    pub fn wins(&self) -> usize {
        self.trades.iter().filter(|trade| trade.net > 0.0).count()
    }

    /// Largest peak-to-trough fall of the realised equity curve.
    ///
    /// Reported because a total that looks healthy can be the far side of a
    /// drawdown nobody would have sat through.
    pub fn max_drawdown(&self) -> f64 {
        let mut equity = 0.0;
        let mut peak: f64 = 0.0;
        let mut worst: f64 = 0.0;
        for trade in &self.trades {
            equity += trade.net;
            peak = peak.max(equity);
            worst = worst.max(peak - equity);
        }
        worst
    }

    /// Realised equity after each trade, for plotting or inspection.
    pub fn equity(&self) -> Vec<(u64, f64)> {
        let mut equity = 0.0;
        self.trades
            .iter()
            .map(|trade| {
                equity += trade.net;
                (trade.exited_unix_ns, equity)
            })
            .collect()
    }

    fn refuse(&mut self, refusal: Refusal) {
        *self.refusals.entry(refusal.to_string()).or_default() += 1;
    }
}

/// A signal to be acted on: when it fired and which way it pointed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signal {
    pub at_unix_ns: u64,
    pub rose: bool,
}

/// Replays `signals` against a touch series.
///
/// `touch_at` supplies the book as it stood at an instant; returning `None`
/// means there was nothing to trade against, which is refused rather than
/// filled at an invented price.
pub fn run<F>(
    signals: &[Signal],
    touch_at: F,
    expected_move: f64,
    react_ns: u64,
    hold_ns: u64,
    limits: &Limits,
) -> Run
where
    F: Fn(u64) -> Option<Touch>,
{
    let mut result = Run::default();
    let mut open: Option<(u64, Side, f64, f64)> = None;

    let mut ordered: Vec<&Signal> = signals.iter().collect();
    ordered.sort_by_key(|signal| signal.at_unix_ns);

    for signal in ordered {
        let entry_at = signal.at_unix_ns.saturating_add(react_ns);

        // Anything already open and past its hold is closed first, so trades
        // never overlap and each one is attributable to a single signal.
        if let Some((opened_at, side, entry, size)) = open {
            let exit_at = opened_at.saturating_add(hold_ns);
            if exit_at <= entry_at {
                close(
                    &mut result,
                    touch_at(exit_at),
                    opened_at,
                    exit_at,
                    side,
                    entry,
                    size,
                    limits,
                );
                open = None;
            }
        }

        // Still holding, so this signal is not acted on. The rules would
        // allow scaling in whenever `size` fits inside `max_position`, but
        // this replay carries one position at a time: taking the entry would
        // overwrite the open one and lose it from the books entirely.
        if open.is_some() {
            result.refuse(Refusal::PositionFull);
            continue;
        }

        let signed_move = if signal.rose {
            expected_move.abs()
        } else {
            -expected_move.abs()
        };

        match super::decision::evaluate(
            signed_move,
            react_ns,
            touch_at(entry_at).as_ref(),
            0.0,
            limits,
        ) {
            Decision::Enter {
                side, price, size, ..
            } => {
                open = Some((entry_at, side, price, size));
            }
            Decision::Refuse(refusal) => result.refuse(refusal),
        }
    }

    // Close whatever is still open at its scheduled exit.
    if let Some((opened_at, side, entry, size)) = open {
        let exit_at = opened_at.saturating_add(hold_ns);
        close(
            &mut result,
            touch_at(exit_at),
            opened_at,
            exit_at,
            side,
            entry,
            size,
            limits,
        );
    }

    result
}

/// Books a position out, or reports that it could not be.
///
/// Returns nothing: an earlier version handed back whether the close
/// succeeded and both callers ignored it, which made a dropped position look
/// like a deliberate one.
#[allow(clippy::too_many_arguments)]
fn close(
    result: &mut Run,
    touch: Option<Touch>,
    opened_at: u64,
    exit_at: u64,
    side: Side,
    entry: f64,
    size: f64,
    limits: &Limits,
) {
    let Some(touch) = touch else {
        // The position cannot be valued, so it is reported rather than
        // closed at a price the market never showed.
        result.abandoned += 1;
        return;
    };

    // Closing crosses back: a long sells the bid, a short buys the ask.
    let exit = match side {
        Side::Buy => touch.bid,
        Side::Sell => touch.ask,
    };
    let gross = match side {
        Side::Buy => exit - entry,
        Side::Sell => entry - exit,
    };
    let net = (gross - limits.fee_fraction * (entry + exit)) * size;

    result.trades.push(Trade {
        entered_unix_ns: opened_at,
        exited_unix_ns: exit_at,
        side,
        entry,
        exit,
        size,
        net,
    });
}

/// Runs the same replay across a ladder of expected moves.
///
/// The answer being looked for is the crossing point: the smallest move a
/// model would have to predict, correctly and on average, for the rules to
/// stop losing money.
pub fn sweep<F>(
    signals: &[Signal],
    touch_at: F,
    moves: &[f64],
    react_ns: u64,
    hold_ns: u64,
    limits: &Limits,
) -> Vec<(f64, Run)>
where
    F: Fn(u64) -> Option<Touch> + Copy,
{
    moves
        .iter()
        .map(|expected| {
            (
                *expected,
                run(signals, touch_at, *expected, react_ns, hold_ns, limits),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            min_edge: 0.0,
            max_position: 10.0,
            size: 10.0,
            max_spread: 0.10,
            require_touch_depth: false,
            fee_fraction: 0.0,
            max_signal_age_ns: 1_000_000_000,
        }
    }

    fn touch(bid: f64, ask: f64) -> Touch {
        Touch {
            bid,
            ask,
            bid_size: 1_000.0,
            ask_size: 1_000.0,
        }
    }

    /// Flat at 0.40/0.42 until t=500, then 0.50/0.52.
    fn rising(at: u64) -> Option<Touch> {
        Some(if at < 500 {
            touch(0.40, 0.42)
        } else {
            touch(0.50, 0.52)
        })
    }

    #[test]
    fn a_profitable_signal_is_taken_and_closed() {
        let signals = [Signal {
            at_unix_ns: 100,
            rose: true,
        }];
        let result = run(&signals, rising, 0.10, 0, 1_000, &limits());

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.entry, 0.42);
        assert_eq!(trade.exit, 0.50);
        assert!((trade.net - 0.8).abs() < 1e-9);
        assert_eq!(result.wins(), 1);
    }

    #[test]
    fn a_signal_whose_move_cannot_pay_the_spread_is_refused() {
        let signals = [Signal {
            at_unix_ns: 100,
            rose: true,
        }];
        let result = run(&signals, rising, 0.01, 0, 1_000, &limits());
        assert!(result.trades.is_empty());
        assert_eq!(result.refusals.get("edge below cost"), Some(&1));
    }

    #[test]
    fn a_signal_arriving_mid_hold_cannot_replace_the_open_position() {
        // Limits that allow scaling in: the rules would say yes, but this
        // replay holds one position, so taking the entry would drop the
        // first one without ever booking it.
        let mut roomy = limits();
        roomy.max_position = 100.0;

        let signals = [
            Signal {
                at_unix_ns: 100,
                rose: true,
            },
            Signal {
                at_unix_ns: 200,
                rose: true,
            },
        ];
        let result = run(&signals, rising, 0.10, 0, 1_000, &roomy);

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].entered_unix_ns, 100);
        assert_eq!(result.refusals.get("position full"), Some(&1));
    }

    #[test]
    fn trades_never_overlap() {
        // Three signals inside one hold: only the first can open.
        let signals = [
            Signal {
                at_unix_ns: 100,
                rose: true,
            },
            Signal {
                at_unix_ns: 200,
                rose: true,
            },
            Signal {
                at_unix_ns: 300,
                rose: true,
            },
        ];
        let result = run(&signals, rising, 0.10, 0, 1_000, &limits());
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.refusals.get("position full"), Some(&2));
    }

    #[test]
    fn a_position_with_no_book_to_close_against_is_reported() {
        let signals = [Signal {
            at_unix_ns: 100,
            rose: true,
        }];
        let only_early = |at: u64| (at < 400).then(|| touch(0.40, 0.42));
        let result = run(&signals, only_early, 0.10, 0, 1_000, &limits());
        assert!(result.trades.is_empty());
        assert_eq!(result.abandoned, 1);
    }

    #[test]
    fn reacting_late_can_turn_a_winner_into_a_loser() {
        let signals = [Signal {
            at_unix_ns: 100,
            rose: true,
        }];
        let prompt = run(&signals, rising, 0.10, 0, 1_000, &limits());
        // Entering after the move has happened buys the new ask and closes
        // at the same bid, giving back the spread.
        let late = run(&signals, rising, 0.10, 600, 1_000, &limits());
        assert!(prompt.net() > 0.0);
        assert!(late.net() < 0.0);
    }

    #[test]
    fn drawdown_is_measured_from_the_peak() {
        let signals = [
            Signal {
                at_unix_ns: 100,
                rose: true,
            },
            Signal {
                at_unix_ns: 5_000,
                rose: true,
            },
        ];
        // The second trade enters after the rise and loses the spread.
        let result = run(&signals, rising, 0.10, 0, 1_000, &limits());
        assert_eq!(result.trades.len(), 2);
        assert!(result.max_drawdown() > 0.0);
        assert_eq!(result.equity().len(), 2);
    }

    #[test]
    fn the_sweep_shows_where_the_rules_stop_losing() {
        let signals = [Signal {
            at_unix_ns: 100,
            rose: true,
        }];
        let ladder = [0.01, 0.05, 0.20];
        let results = sweep(&signals, rising, &ladder, 0, 1_000, &limits());

        assert_eq!(results.len(), 3);
        // Too small to clear the spread, so nothing is taken at all.
        assert!(results[0].1.trades.is_empty());
        // Large enough to act on, and the market obliged.
        assert!(results[2].1.net() > 0.0);
    }

    #[test]
    fn an_empty_run_reports_nothing_rather_than_zero_profit() {
        let result = run(&[], rising, 0.1, 0, 1_000, &limits());
        assert!(result.trades.is_empty());
        assert_eq!(result.net(), 0.0);
        assert_eq!(result.max_drawdown(), 0.0);
        assert!(result.equity().is_empty());
    }
}
