//! Whether to act on a signal, and why not when not.
//!
//! This is the first piece of the live pipeline, written offline so it can be
//! tested against recordings before it is ever wired to money. It is a pure
//! function: a signal, a touch, a position and a set of limits go in, a
//! decision comes out.
//!
//! It does not pretend to know what a signal is worth. The expected move is
//! an input, supplied by whatever model is being tried — today a flat
//! assumption. That is deliberate: the backtest can then sweep it and report
//! what a model would have to be right about for the strategy to break even,
//! which is a more useful question than what one made-up number produces.

use std::fmt;

/// Which way a decision goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

/// The touch as it stands, and how much is resting on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    pub bid: f64,
    pub ask: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

impl Touch {
    pub fn spread(&self) -> f64 {
        self.ask - self.bid
    }

    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    /// The price a taker pays on this side.
    pub fn taking(&self, side: Side) -> f64 {
        match side {
            Side::Buy => self.ask,
            Side::Sell => self.bid,
        }
    }

    /// What is resting to take on this side.
    pub fn resting(&self, side: Side) -> f64 {
        match side {
            Side::Buy => self.ask_size,
            Side::Sell => self.bid_size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Edge required *beyond* the round trip cost, in probability points.
    pub min_edge: f64,
    /// Largest absolute position, in units.
    pub max_position: f64,
    /// Units per entry.
    pub size: f64,
    /// Refuse a book wider than this; crossing it costs more than it is worth.
    pub max_spread: f64,
    /// Refuse when the touch cannot absorb the whole order. Walking the book
    /// is a different, worse trade than the one being evaluated.
    pub require_touch_depth: bool,
    /// Taker fee as a fraction of notional, charged on each leg.
    pub fee_fraction: f64,
    /// A signal older than this is stale and is not acted on.
    pub max_signal_age_ns: u64,
}

/// Why a signal was not acted on.
///
/// Refusals are named rather than collapsed into a boolean: a backtest where
/// everything is skipped should say which rule did it, or the rule that is
/// wrong never gets found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Stale,
    SpreadTooWide,
    ThinTouch,
    PositionFull,
    NotEnoughEdge,
    NoTouch,
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Stale => "signal too old",
            Self::SpreadTooWide => "spread too wide",
            Self::ThinTouch => "touch too thin",
            Self::PositionFull => "position full",
            Self::NotEnoughEdge => "edge below cost",
            Self::NoTouch => "no book",
        };
        formatter.write_str(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Enter {
        side: Side,
        price: f64,
        size: f64,
        /// Expected move less the full round trip cost.
        edge_after_cost: f64,
    },
    Refuse(Refusal),
}

/// The cost of getting in and back out at this touch.
///
/// A round trip crosses the spread twice — once to take, once to give it
/// back — and pays the fee on both notionals. Charging half a spread, or
/// charging it once, is how a backtest invents an edge it will not see.
pub fn round_trip_cost(touch: &Touch, fee_fraction: f64) -> f64 {
    touch.spread() + fee_fraction * (touch.ask + touch.bid)
}

/// Decides whether to act.
///
/// `expected_move` is the signed move in the first outcome the signal is
/// taken to imply, in probability points. Its sign picks the side.
pub fn evaluate(
    expected_move: f64,
    age_ns: u64,
    touch: Option<&Touch>,
    position: f64,
    limits: &Limits,
) -> Decision {
    let Some(touch) = touch else {
        return Decision::Refuse(Refusal::NoTouch);
    };
    if age_ns > limits.max_signal_age_ns {
        return Decision::Refuse(Refusal::Stale);
    }
    if touch.spread() > limits.max_spread {
        return Decision::Refuse(Refusal::SpreadTooWide);
    }

    let side = if expected_move >= 0.0 {
        Side::Buy
    } else {
        Side::Sell
    };

    // Adding to a position that is already at the limit is refused; reducing
    // one is not, since it moves back inside the limit.
    let after = position + side.sign() * limits.size;
    if after.abs() > limits.max_position && after.abs() > position.abs() {
        return Decision::Refuse(Refusal::PositionFull);
    }

    if limits.require_touch_depth && touch.resting(side) < limits.size {
        return Decision::Refuse(Refusal::ThinTouch);
    }

    let edge_after_cost = expected_move.abs() - round_trip_cost(touch, limits.fee_fraction);
    if edge_after_cost < limits.min_edge {
        return Decision::Refuse(Refusal::NotEnoughEdge);
    }

    Decision::Enter {
        side,
        price: touch.taking(side),
        size: limits.size,
        edge_after_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch() -> Touch {
        Touch {
            bid: 0.40,
            ask: 0.42,
            bid_size: 1_000.0,
            ask_size: 1_000.0,
        }
    }

    fn limits() -> Limits {
        Limits {
            min_edge: 0.0,
            max_position: 100.0,
            size: 10.0,
            max_spread: 0.05,
            require_touch_depth: true,
            fee_fraction: 0.0,
            max_signal_age_ns: 1_000_000_000,
        }
    }

    #[test]
    fn a_round_trip_pays_the_whole_spread_not_half() {
        assert!((round_trip_cost(&touch(), 0.0) - 0.02).abs() < 1e-9);
    }

    #[test]
    fn fees_are_charged_on_both_legs() {
        let cost = round_trip_cost(&touch(), 0.01);
        assert!((cost - (0.02 + 0.01 * 0.82)).abs() < 1e-9);
    }

    #[test]
    fn a_move_bigger_than_the_cost_is_taken_at_the_ask() {
        let decision = evaluate(0.08, 0, Some(&touch()), 0.0, &limits());
        match decision {
            Decision::Enter {
                side,
                price,
                size,
                edge_after_cost,
            } => {
                assert_eq!(side, Side::Buy);
                assert_eq!(price, 0.42);
                assert_eq!(size, 10.0);
                assert!((edge_after_cost - 0.06).abs() < 1e-9);
            }
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn a_downward_move_sells_the_bid() {
        let decision = evaluate(-0.08, 0, Some(&touch()), 0.0, &limits());
        match decision {
            Decision::Enter { side, price, .. } => {
                assert_eq!(side, Side::Sell);
                assert_eq!(price, 0.40);
            }
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn a_move_smaller_than_the_round_trip_is_refused() {
        // A one-cent move cannot pay a two-cent spread.
        assert_eq!(
            evaluate(0.01, 0, Some(&touch()), 0.0, &limits()),
            Decision::Refuse(Refusal::NotEnoughEdge)
        );
    }

    #[test]
    fn the_required_edge_sits_on_top_of_the_cost() {
        let mut limits = limits();
        limits.min_edge = 0.05;
        // Clears the spread by 0.03, which is short of the 0.05 demanded.
        assert_eq!(
            evaluate(0.05, 0, Some(&touch()), 0.0, &limits),
            Decision::Refuse(Refusal::NotEnoughEdge)
        );
    }

    #[test]
    fn a_stale_signal_is_not_acted_on() {
        assert_eq!(
            evaluate(0.5, 2_000_000_000, Some(&touch()), 0.0, &limits()),
            Decision::Refuse(Refusal::Stale)
        );
    }

    #[test]
    fn a_wide_book_is_refused_before_anything_else_is_considered() {
        let wide = Touch {
            bid: 0.20,
            ask: 0.80,
            ..touch()
        };
        assert_eq!(
            evaluate(0.9, 0, Some(&wide), 0.0, &limits()),
            Decision::Refuse(Refusal::SpreadTooWide)
        );
    }

    #[test]
    fn a_touch_that_cannot_absorb_the_order_is_refused() {
        let thin = Touch {
            ask_size: 1.0,
            ..touch()
        };
        assert_eq!(
            evaluate(0.8, 0, Some(&thin), 0.0, &limits()),
            Decision::Refuse(Refusal::ThinTouch)
        );
        // Selling is unaffected: the bid still has depth.
        assert!(matches!(
            evaluate(-0.8, 0, Some(&thin), 0.0, &limits()),
            Decision::Enter { .. }
        ));
    }

    #[test]
    fn a_full_position_cannot_grow_but_can_shrink() {
        let limits = limits();
        assert_eq!(
            evaluate(0.8, 0, Some(&touch()), 100.0, &limits),
            Decision::Refuse(Refusal::PositionFull)
        );
        // The other way reduces exposure, so it is allowed.
        assert!(matches!(
            evaluate(-0.8, 0, Some(&touch()), 100.0, &limits),
            Decision::Enter { .. }
        ));
    }

    #[test]
    fn without_a_book_there_is_nothing_to_trade_against() {
        assert_eq!(
            evaluate(0.9, 0, None, 0.0, &limits()),
            Decision::Refuse(Refusal::NoTouch)
        );
    }

    #[test]
    fn every_refusal_says_which_rule_stopped_it() {
        for refusal in [
            Refusal::Stale,
            Refusal::SpreadTooWide,
            Refusal::ThinTouch,
            Refusal::PositionFull,
            Refusal::NotEnoughEdge,
            Refusal::NoTouch,
        ] {
            assert!(!refusal.to_string().is_empty());
        }
    }
}
