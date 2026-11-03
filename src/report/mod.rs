//! Turning a recording into something you can look at.
//!
//! The analysis prints numbers; this draws the same recording on one axis, so
//! a lead that rests on a single outlier stops looking like a percentile and
//! starts looking like what it is.
//!
//! Deliberately a replay, never a live view. The spectator baseline is only
//! worth anything while the person pressing the keys cannot see the market:
//! a chart of the book next to the labelling page would turn an independent
//! observation into an echo of the thing it is supposed to be measured
//! against, and nothing in the data would reveal that it had happened.

pub mod scale;
pub mod timeline;
