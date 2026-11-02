//! Latency of the recorder itself.
//!
//! A project about latency that does not measure its own is taking its word
//! for it. These histograms cover the only stretch the recorder actually
//! controls: from a frame coming off the socket to that frame being handed
//! to the operating system. The network before it and the disk after it are
//! not ours to claim.
//!
//! Each task owns its own histograms and merges them once at the end. A
//! shared, locked histogram on the hot path would measure the contention it
//! introduced, which is exactly the kind of self-inflicted number this file
//! exists to avoid.

use std::time::Duration;

use hdrhistogram::Histogram;

/// Longest value the histograms can record, in nanoseconds.
///
/// Ten seconds is far beyond anything healthy here; a value that clips is a
/// stall, and it is better for one to show up at the ceiling than to be
/// dropped and leave the percentiles looking tidy.
const MAX_NS: u64 = 10_000_000_000;
const SIGNIFICANT_DIGITS: u8 = 3;

fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, MAX_NS, SIGNIFICANT_DIGITS).expect("fixed bounds are valid")
}

/// What a stage of the recorder cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Validating the frame as JSON, before it is queued.
    Parse,
    /// Handing the record to the writer, including any wait for a full queue.
    Enqueue,
    /// Formatting one record as a line.
    Format,
    /// Writing that line out.
    Write,
    /// Frame off the socket to line written. The one that matters.
    Total,
    /// How far apart the exchange timestamp and our receipt are.
    ///
    /// Not a latency. It spans two machines, so it is the network delay plus
    /// however far the two clocks disagree, and the two cannot be separated
    /// without synchronised time. The magnitude is recorded unsigned because
    /// the sign only tells you which clock is ahead, not which part is which.
    FeedOffset,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Enqueue => "enqueue",
            Self::Format => "format",
            Self::Write => "write",
            Self::Total => "total",
            Self::FeedOffset => "feed offset*",
        }
    }
}

/// Histograms for a set of stages, owned by one task.
#[derive(Debug)]
pub struct Timings {
    stages: Vec<(Stage, Histogram<u64>)>,
}

impl Timings {
    pub fn new(stages: &[Stage]) -> Self {
        Self {
            stages: stages.iter().map(|stage| (*stage, histogram())).collect(),
        }
    }

    /// Records one observation. Values past the ceiling are clamped so a
    /// stall still lands in the histogram rather than being discarded.
    pub fn record(&mut self, stage: Stage, nanos: u64) {
        if let Some((_, histogram)) = self.stages.iter_mut().find(|(each, _)| *each == stage) {
            let _ = histogram.record(nanos.clamp(1, MAX_NS));
        }
    }

    pub fn record_duration(&mut self, stage: Stage, elapsed: Duration) {
        self.record(stage, elapsed.as_nanos().min(u128::from(MAX_NS)) as u64);
    }

    /// Folds another task's histograms into these.
    pub fn merge(&mut self, other: Timings) {
        for (stage, histogram) in other.stages {
            match self.stages.iter_mut().find(|(each, _)| *each == stage) {
                Some((_, mine)) => {
                    let _ = mine.add(&histogram);
                }
                None => self.stages.push((stage, histogram)),
            }
        }
        self.stages.sort_by_key(|(stage, _)| *stage);
    }

    /// One line per stage that saw at least one observation.
    pub fn report(&self) -> Vec<StageReport> {
        self.stages
            .iter()
            .filter(|(_, histogram)| !histogram.is_empty())
            .map(|(stage, histogram)| StageReport {
                stage: *stage,
                count: histogram.len(),
                p50_ns: histogram.value_at_quantile(0.50),
                p90_ns: histogram.value_at_quantile(0.90),
                p99_ns: histogram.value_at_quantile(0.99),
                max_ns: histogram.max(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.iter().all(|(_, each)| each.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageReport {
    pub stage: Stage,
    pub count: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// Formats a nanosecond figure in whichever unit keeps it readable.
pub fn humanise(nanos: u64) -> String {
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.1}us", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.1}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", nanos as f64 / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_follow_the_observations() {
        let mut timings = Timings::new(&[Stage::Parse]);
        for value in 1..=100 {
            timings.record(Stage::Parse, value * 1_000);
        }
        let report = timings.report();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].count, 100);
        assert!(report[0].p50_ns >= 49_000 && report[0].p50_ns <= 51_000);
        assert!(report[0].max_ns >= 99_000);
    }

    #[test]
    fn a_stall_lands_at_the_ceiling_rather_than_vanishing() {
        let mut timings = Timings::new(&[Stage::Enqueue]);
        timings.record(Stage::Enqueue, u64::MAX);
        let report = timings.report();
        assert_eq!(report[0].count, 1);
        assert!(report[0].max_ns >= MAX_NS - MAX_NS / 1000);
    }

    #[test]
    fn a_zero_reading_is_still_recorded() {
        let mut timings = Timings::new(&[Stage::Parse]);
        timings.record(Stage::Parse, 0);
        assert_eq!(timings.report()[0].count, 1);
    }

    #[test]
    fn stages_never_observed_are_left_out_of_the_report() {
        let mut timings = Timings::new(&[Stage::Parse, Stage::Write]);
        timings.record(Stage::Parse, 500);
        let report = timings.report();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].stage, Stage::Parse);
    }

    #[test]
    fn merging_sums_the_counts_and_keeps_both_sets_of_stages() {
        let mut producer = Timings::new(&[Stage::Parse]);
        producer.record(Stage::Parse, 1_000);
        producer.record(Stage::Parse, 2_000);

        let mut writer = Timings::new(&[Stage::Parse, Stage::Write]);
        writer.record(Stage::Parse, 3_000);
        writer.record(Stage::Write, 4_000);

        producer.merge(writer);
        let report = producer.report();
        assert_eq!(report.len(), 2);
        assert_eq!(report[0].stage, Stage::Parse);
        assert_eq!(report[0].count, 3);
        assert_eq!(report[1].stage, Stage::Write);
        assert_eq!(report[1].count, 1);
    }

    #[test]
    fn an_untouched_set_reports_nothing() {
        let timings = Timings::new(&[Stage::Parse, Stage::Total]);
        assert!(timings.is_empty());
        assert!(timings.report().is_empty());
    }

    #[test]
    fn durations_are_readable_at_every_scale() {
        assert_eq!(humanise(900), "900ns");
        assert_eq!(humanise(1_500), "1.5us");
        assert_eq!(humanise(2_500_000), "2.5ms");
        assert_eq!(humanise(3_000_000_000), "3.00s");
    }
}
