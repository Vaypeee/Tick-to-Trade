//! Infrastructure for the tick-to-trade experiment.
//!
//! The crate is split in two halves. [`recorder`] captures raw market data to
//! disk with timestamps, and is the half that exists today: the hypothesis
//! behind this project is testable from recordings alone, without a fast
//! system. The live pipeline is built on top of it once the recordings say
//! what latency budget is actually worth chasing.

pub mod config;
pub mod recorder;
pub mod tls;
