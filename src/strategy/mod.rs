//! The trading rules, written offline so they can be tested before they cost
//! anything.
//!
//! [`decision`] is a pure function from a signal and a book to an action.
//! [`paper`] replays recordings through it and keeps the books. Neither
//! touches the network, and the live pipeline is meant to call the same
//! `decision::evaluate` rather than a second copy of the rules that drifts
//! from the one the backtest validated.

pub mod decision;
pub mod paper;
