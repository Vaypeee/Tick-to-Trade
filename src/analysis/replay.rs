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
    /// A key press from a spectator watching the public broadcast.
    Label,
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
    /// On a label: which button was pressed.
    #[serde(default)]
    pub label: Option<String>,
    /// On a label: which way the event should push the first outcome.
    #[serde(default)]
    pub direction: Option<String>,
    /// On a label: the browser clock at the press, before the loopback trip.
    #[serde(default)]
    pub t_click_unix_ns: Option<u64>,
}

impl Record {
    /// The window inside which this record's change actually happened.
    ///
    /// For a pushed frame it is a point: the feed told us as it happened. For
    /// a poll it spans from the request that found the change back to nothing
    /// earlier than the previous response, because the change could have
    /// landed at any moment in between.
    pub fn observed_window_ns(&self) -> (u64, u64) {
        match (self.t_request_unix_ns, self.t_click_unix_ns) {
            // A poll found the change somewhere between asking and answering.
            (Some(requested), _) => (requested, self.t_unix_ns),
            // A press is a point in time; the loopback write is not part of it.
            (_, Some(clicked)) => (clicked, clicked),
            _ => (self.t_unix_ns, self.t_unix_ns),
        }
    }

    /// Width of that window. Zero for a pushed frame.
    pub fn uncertainty_ns(&self) -> u64 {
        let (from, to) = self.observed_window_ns();
        to.saturating_sub(from)
    }
}

/// Names the recordings sitting next to a path that could not be read.
///
/// A recording is identified by a unix timestamp nobody memorises, so the
/// usual mistake is a stale or mistyped name — including pasting the
/// `<unix>` placeholder straight out of the documentation. An error that
/// lists what is actually there turns that into one more keystroke instead
/// of a directory listing.
fn nearby(path: &Path) -> String {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort();
    names.truncate(8);
    format!("; `{}` holds: {}", dir.display(), names.join(", "))
}

/// Loads a recording, failing with the line number that could not be parsed.
pub fn read(path: &Path) -> Result<Vec<Record>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read `{}`{}", path.display(), nearby(path)))?;
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
    fn a_label_is_timed_from_the_press_not_from_the_write() {
        let parsed = record(
            r#"{"seq":9,"t_unix_ns":5000,"t_mono_ns":50,"kind":"label","label":"blue_fight",
                "direction":"up","t_click_unix_ns":4000,"t_click_mono_ns":40}"#,
        );
        assert_eq!(parsed.kind, Kind::Label);
        assert_eq!(parsed.label.as_deref(), Some("blue_fight"));
        assert_eq!(parsed.direction.as_deref(), Some("up"));
        assert_eq!(parsed.observed_window_ns(), (4000, 4000));
        assert_eq!(parsed.uncertainty_ns(), 0);
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
    fn a_missing_file_points_at_the_ones_that_are_there() {
        let dir = std::env::temp_dir().join("tick-to-trade-replay-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("polymarket-1.jsonl"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();

        let error = read(&dir.join("polymarket-<unix>.jsonl")).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("polymarket-1.jsonl"), "{text}");
        assert!(!text.contains("notes.txt"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_directory_adds_nothing_to_the_error() {
        let dir = std::env::temp_dir().join("tick-to-trade-replay-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let error = read(&dir.join("missing.jsonl")).unwrap_err();
        assert!(!format!("{error:#}").contains("holds:"));
        let _ = std::fs::remove_dir_all(&dir);
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
