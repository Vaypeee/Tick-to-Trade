//! Reads a recording back.
//!
//! The on-disk format is deliberately flat, so this stays a plain struct with
//! optional fields rather than a tagged enum: a recording written by a newer
//! recorder should still load in an older analysis, minus the fields it does
//! not know about. Analysis code that refuses to read last month's data is
//! analysis code that stops being run.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A websocket frame, stored verbatim.
    Data,
    /// A frame that was not JSON.
    Text,
    /// A marker written by the recorder.
    Control,
    /// An HTTP response that differed from the previous one.
    Poll,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Record {
    pub seq: u64,
    /// Wall clock at receipt.
    ///
    /// This is the only clock that can be compared **between** recordings.
    /// Each recorder starts its own monotonic origin, so `t_mono_ns` from two
    /// files are unrelated numbers; aligning on them would silently produce a
    /// nonsense offset.
    pub t_unix_ns: u64,
    /// Monotonic nanoseconds since that recorder started. Use within one file.
    pub t_mono_ns: u64,
    pub kind: Kind,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub control: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub t_request_unix_ns: Option<u64>,
    #[serde(default)]
    pub unchanged_polls: Option<u64>,
}

impl Record {
    /// The window inside which this record's change actually happened.
    ///
    /// For a pushed frame it is a point: the feed told us as it happened. For
    /// a poll it spans from the request that found the change back to nothing
    /// earlier than the previous response, because the change could have
    /// landed at any moment in between.
    pub fn observed_window_ns(&self) -> (u64, u64) {
        match self.t_request_unix_ns {
            Some(requested) => (requested, self.t_unix_ns),
            None => (self.t_unix_ns, self.t_unix_ns),
        }
    }

    /// Width of that window. Zero for a pushed frame.
    pub fn uncertainty_ns(&self) -> u64 {
        let (from, to) = self.observed_window_ns();
        to.saturating_sub(from)
    }
}

/// Loads a recording, failing with the line number that could not be parsed.
pub fn read(path: &Path) -> Result<Vec<Record>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read `{}`", path.display()))?;
    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<Record>(line)
                .with_context(|| format!("`{}` line {} is not a record", path.display(), index + 1))
        })
        .collect()
}

/// Reports gaps in the sequence numbers.
///
/// A recording is only evidence if nothing went missing from it, and the
/// recorder numbers every line precisely so this check is possible.
pub fn missing_sequences(records: &[Record]) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    for pair in records.windows(2) {
        let (previous, next) = (pair[0].seq, pair[1].seq);
        if next > previous + 1 {
            gaps.push((previous, next));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: &str) -> Record {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn reads_a_websocket_frame() {
        let parsed = record(
            r#"{"seq":1,"t_unix_ns":10,"t_mono_ns":2,"kind":"data","payload":{"event_type":"book"}}"#,
        );
        assert_eq!(parsed.kind, Kind::Data);
        assert_eq!(parsed.uncertainty_ns(), 0);
        assert_eq!(parsed.observed_window_ns(), (10, 10));
    }

    #[test]
    fn reads_a_poll_and_keeps_its_detection_window() {
        let parsed = record(
            r#"{"seq":4,"t_unix_ns":500,"t_mono_ns":9,"kind":"poll","status":200,
                "t_request_unix_ns":200,"t_request_mono_ns":3,"unchanged_polls":2,"payload":{}}"#,
        );
        assert_eq!(parsed.kind, Kind::Poll);
        assert_eq!(parsed.observed_window_ns(), (200, 500));
        assert_eq!(parsed.uncertainty_ns(), 300);
        assert_eq!(parsed.unchanged_polls, Some(2));
    }

    #[test]
    fn reads_a_control_marker() {
        let parsed = record(
            r#"{"seq":2,"t_unix_ns":1,"t_mono_ns":1,"kind":"control","control":"connected"}"#,
        );
        assert_eq!(parsed.control.as_deref(), Some("connected"));
        assert!(parsed.detail.is_none());
    }

    #[test]
    fn unknown_fields_do_not_break_an_older_analysis() {
        let parsed = record(
            r#"{"seq":1,"t_unix_ns":1,"t_mono_ns":1,"kind":"data","payload":1,"future":"x"}"#,
        );
        assert_eq!(parsed.seq, 1);
    }

    #[test]
    fn sequence_gaps_are_reported() {
        let records: Vec<Record> = [1u64, 2, 5, 6, 9]
            .iter()
            .map(|seq| {
                record(&format!(
                    r#"{{"seq":{seq},"t_unix_ns":1,"t_mono_ns":1,"kind":"control","control":"x"}}"#
                ))
            })
            .collect();
        assert_eq!(missing_sequences(&records), vec![(2, 5), (6, 9)]);
    }

    #[test]
    fn a_dense_recording_has_no_gaps() {
        let records: Vec<Record> = (1u64..=4)
            .map(|seq| {
                record(&format!(
                    r#"{{"seq":{seq},"t_unix_ns":1,"t_mono_ns":1,"kind":"control","control":"x"}}"#
                ))
            })
            .collect();
        assert!(missing_sequences(&records).is_empty());
    }
}
