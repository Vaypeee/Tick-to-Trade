//! Offline analysis of recordings.
//!
//! Nothing here touches the network. Everything is derived from files, so a
//! result can be reproduced, disputed and re-derived after the model changes —
//! which is the only reason the recorders were built first.

pub mod align;
pub mod book;
pub mod diff;
pub mod edge;
pub mod flow;
pub mod replay;

use align::Observation;
use replay::{Kind, Record};

/// Prices on this venue are discrete cents, so every comparison between them
/// is made on integer thousandths rather than on the floats they arrive as.
///
/// This is not pedantry. `0.60 - 0.50` is `0.09999999999999998` in binary
/// floating point, so a two-cent threshold silently rejects a two-cent move.
pub fn ticks(price: f64) -> i64 {
    (price * 1000.0).round() as i64
}

/// Turns a polled sports recording into observations, by diffing each stored
/// response against the one before it.
///
/// Only stored responses are diffed, which is exactly right: the recorder
/// stored a response precisely because it differed from its predecessor, so
/// consecutive stored responses bracket every change the feed ever showed.
///
/// `needles` narrows the diff to paths worth treating as events. With none,
/// every change counts — the honest default when it is not yet known which
/// field of a feed moves when something happens on the pitch.
pub fn observations(
    records: &[Record],
    needles: &[String],
    max_changes: usize,
) -> Vec<Observation> {
    let polls: Vec<&Record> = records
        .iter()
        .filter(|record| record.kind == Kind::Poll && record.payload.is_some())
        .collect();

    polls
        .windows(2)
        .filter_map(|pair| {
            let (previous, current) = (pair[0], pair[1]);
            let changes = diff::diff(
                previous.payload.as_ref()?,
                current.payload.as_ref()?,
                max_changes,
            );
            let matching = diff::filter(&changes, needles);
            if matching.is_empty() {
                return None;
            }

            let (from, at) = current.observed_window_ns();
            Some(Observation {
                from_unix_ns: from,
                at_unix_ns: at,
                label: label(&matching),
            })
        })
        .collect()
}

fn label(changes: &[&diff::Change]) -> String {
    const SHOWN: usize = 3;
    let mut parts: Vec<String> = changes
        .iter()
        .take(SHOWN)
        .map(|change| change.path.clone())
        .collect();
    if changes.len() > SHOWN {
        parts.push(format!("+{} more", changes.len() - SHOWN));
    }
    parts.join(", ")
}

/// How often each path changed across the recording, most frequent first.
///
/// A polled document churns: bookmaker odds, clocks, advertising slots. This
/// is what tells you which paths are noise and which might mean something
/// happened, so that `needles` can be chosen from evidence rather than from
/// a guess about the provider.
pub fn change_frequency(records: &[Record], max_changes: usize) -> Vec<(String, usize)> {
    let polls: Vec<&Record> = records
        .iter()
        .filter(|record| record.kind == Kind::Poll && record.payload.is_some())
        .collect();

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for pair in polls.windows(2) {
        let (Some(before), Some(after)) = (pair[0].payload.as_ref(), pair[1].payload.as_ref())
        else {
            continue;
        };
        for change in diff::diff(before, after, max_changes) {
            *counts.entry(change.path).or_default() += 1;
        }
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
}

/// Presses from a labels recording, as `(pressed, label, direction)`.
///
/// The press is timed from the browser clock at the key, not from the moment
/// the server wrote it: the loopback trip is the recorder, not the spectator.
pub fn presses(records: &[Record]) -> Vec<(u64, String, String)> {
    records
        .iter()
        .filter(|record| record.kind == Kind::Label)
        .map(|record| {
            let (pressed, _) = record.observed_window_ns();
            (
                pressed,
                record.label.clone().unwrap_or_default(),
                record.direction.clone().unwrap_or_else(|| "neutral".into()),
            )
        })
        .collect()
}

/// The outcome a market recording lists first, read from its sidecar.
///
/// A label says which way an event should push *the first* outcome, so the
/// comparison needs to know which token that was. The sidecar is written for
/// exactly this: a recording that cannot say what it recorded is not evidence.
pub fn reference_asset(recording: &std::path::Path) -> Option<String> {
    let sidecar = recording.with_extension("meta.json");
    let raw = std::fs::read_to_string(sidecar).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed
        .get("asset_ids")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_owned)
}

/// Wall-clock span covered by a recording, in nanoseconds.
pub fn span_ns(records: &[Record]) -> Option<(u64, u64)> {
    let first = records.first()?.t_unix_ns;
    let last = records.last()?.t_unix_ns;
    Some((first, last))
}

/// The window both recordings cover. Anything outside it cannot be analysed,
/// and quietly including it would compare a feed against a silent market.
pub fn overlap_ns(left: (u64, u64), right: (u64, u64)) -> Option<(u64, u64)> {
    let from = left.0.max(right.0);
    let to = left.1.min(right.1);
    (from < to).then_some((from, to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn poll(seq: u64, requested: u64, received: u64, payload: serde_json::Value) -> Record {
        serde_json::from_value(json!({
            "seq": seq,
            "t_unix_ns": received,
            "t_mono_ns": received,
            "kind": "poll",
            "status": 200,
            "t_request_unix_ns": requested,
            "t_request_mono_ns": requested,
            "unchanged_polls": 0,
            "payload": payload,
        }))
        .unwrap()
    }

    #[test]
    fn consecutive_stored_responses_bracket_each_change() {
        let records = vec![
            poll(1, 10, 20, json!({"score": 0, "noise": "a"})),
            poll(2, 100, 120, json!({"score": 1, "noise": "a"})),
        ];
        let found = observations(&records, &[], 50);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].from_unix_ns, 100);
        assert_eq!(found[0].at_unix_ns, 120);
        assert_eq!(found[0].label, "/score");
    }

    #[test]
    fn a_single_response_yields_nothing_to_compare() {
        assert!(observations(&[poll(1, 1, 2, json!({"a": 1}))], &[], 50).is_empty());
    }

    #[test]
    fn filtering_drops_changes_that_are_not_events() {
        let records = vec![
            poll(1, 10, 20, json!({"score": 0, "ads": 1})),
            poll(2, 30, 40, json!({"score": 0, "ads": 2})),
        ];
        assert!(observations(&records, &["score".to_string()], 50).is_empty());
        assert_eq!(observations(&records, &[], 50).len(), 1);
    }

    #[test]
    fn labels_summarise_rather_than_dump_the_diff() {
        let records = vec![
            poll(1, 1, 2, json!({"a": 0, "b": 0, "c": 0, "d": 0})),
            poll(2, 3, 4, json!({"a": 1, "b": 1, "c": 1, "d": 1})),
        ];
        assert_eq!(
            observations(&records, &[], 50)[0].label,
            "/a, /b, /c, +1 more"
        );
    }

    #[test]
    fn change_frequency_ranks_the_noisiest_paths_first() {
        let records = vec![
            poll(1, 1, 2, json!({"clock": 1, "score": 0})),
            poll(2, 3, 4, json!({"clock": 2, "score": 0})),
            poll(3, 5, 6, json!({"clock": 3, "score": 1})),
        ];
        let ranked = change_frequency(&records, 50);
        assert_eq!(ranked[0], ("/clock".to_string(), 2));
        assert_eq!(ranked[1], ("/score".to_string(), 1));
    }

    fn label(seq: u64, clicked: u64, name: &str, direction: &str) -> Record {
        serde_json::from_value(json!({
            "seq": seq,
            "t_unix_ns": clicked + 3_000_000,
            "t_mono_ns": clicked,
            "kind": "label",
            "label": name,
            "direction": direction,
            "t_click_unix_ns": clicked,
        }))
        .unwrap()
    }

    #[test]
    fn presses_are_timed_from_the_key_not_from_the_write() {
        let records = vec![label(1, 1_000, "blue_fight", "up")];
        assert_eq!(
            presses(&records),
            [(1_000, "blue_fight".to_string(), "up".to_string())]
        );
    }

    #[test]
    fn control_lines_in_a_labels_recording_are_not_presses() {
        let records: Vec<Record> = vec![
            serde_json::from_value(json!({
                "seq": 1, "t_unix_ns": 1, "t_mono_ns": 1,
                "kind": "control", "control": "serving",
            }))
            .unwrap(),
            label(2, 500, "red_pick", "down"),
        ];
        assert_eq!(presses(&records).len(), 1);
    }

    #[test]
    fn overlap_is_the_window_both_recordings_cover() {
        assert_eq!(overlap_ns((0, 100), (50, 150)), Some((50, 100)));
        assert_eq!(overlap_ns((0, 40), (50, 150)), None);
    }
}
