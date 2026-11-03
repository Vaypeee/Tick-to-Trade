//! Infrastructure for the tick-to-trade experiment.
//!
//! Two halves exist today, and neither touches the live pipeline. [`recorder`]
//! captures raw feeds to disk with timestamps; [`analysis`] replays those
//! recordings offline and measures the lag between a real-world event and the
//! market repricing. The hypothesis behind this project is testable from
//! recordings alone, without a fast system, so that is what was built first.
//! The pipeline follows once the measurement says what latency budget is
//! actually worth chasing.

pub mod analysis;
pub mod config;
pub mod recorder;
pub mod report;
pub mod strategy;
pub mod tls;
