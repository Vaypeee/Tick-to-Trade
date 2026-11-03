//! Infrastructure for the tick-to-trade experiment.
//!
//! Nothing here sends an order, and that is the point. Both hypotheses the
//! project is built around are testable from recordings alone, so the
//! measurement apparatus came first and the fast system waits for a reason to
//! exist.
//!
//! - [`recorder`] captures a feed to disk, verbatim, stamped on two clocks.
//! - [`analysis`] replays those files: rebuilding the order book, extracting
//!   trades and bursts of taking, and comparing the market against a data
//!   feed, against a spectator, and against itself.
//! - [`strategy`] holds the trading rules. They are pure functions, tested
//!   against recordings, because rules can be checked for nothing while an
//!   execution path can only be checked against money.
//! - [`report`] draws a recording as a timeline, in replay only.
//! - [`config`] and [`tls`] are the plumbing the rest needs.

pub mod analysis;
pub mod config;
pub mod recorder;
pub mod report;
pub mod strategy;
pub mod tls;
