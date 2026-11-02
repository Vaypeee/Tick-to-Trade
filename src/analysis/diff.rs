//! Structural diff between two recorded JSON documents.
//!
//! A polled feed hands you a whole document every time, and almost all of it
//! is noise: timestamps, ad slots, standings that nobody scored to change.
//! The interesting question is which *paths* moved, so the diff reports paths
//! rather than documents, and the analysis can then decide which paths mean
//! something happened on the pitch.
//!
//! This is deliberately provider-agnostic. Hard-coding one vendor's field
//! names here would make the tool worthless the moment the feed is replaced,
//! which — on current measurements — it will be.

use serde_json::Value;

/// One path whose value differs between two documents.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// JSON Pointer, for example `/events/0/competitions/0/competitors/1/score`.
    pub path: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// Every path that differs, in document order.
///
/// `max_changes` bounds the work: a feed that reshuffles a large array
/// produces thousands of differences, and a truncated list is more useful
/// than a report nobody can read.
pub fn diff(before: &Value, after: &Value, max_changes: usize) -> Vec<Change> {
    let mut changes = Vec::new();
    walk(
        String::new(),
        Some(before),
        Some(after),
        max_changes,
        &mut changes,
    );
    changes
}

fn walk(
    path: String,
    before: Option<&Value>,
    after: Option<&Value>,
    max_changes: usize,
    changes: &mut Vec<Change>,
) {
    if changes.len() >= max_changes {
        return;
    }
    match (before, after) {
        (Some(before), Some(after)) if before == after => {}
        (Some(Value::Object(before)), Some(Value::Object(after))) => {
            let mut keys: Vec<&String> = before.keys().collect();
            keys.extend(after.keys().filter(|key| !before.contains_key(*key)));
            for key in keys {
                walk(
                    format!("{path}/{}", escape(key)),
                    before.get(key),
                    after.get(key),
                    max_changes,
                    changes,
                );
            }
        }
        (Some(Value::Array(before)), Some(Value::Array(after))) => {
            for index in 0..before.len().max(after.len()) {
                walk(
                    format!("{path}/{index}"),
                    before.get(index),
                    after.get(index),
                    max_changes,
                    changes,
                );
            }
        }
        (before, after) => changes.push(Change {
            path: if path.is_empty() { "/".into() } else { path },
            before: before.cloned(),
            after: after.cloned(),
        }),
    }
}

/// JSON Pointer escaping, so a key containing a slash cannot forge a path.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Keeps the changes whose path contains any of `needles`, case-insensitively.
///
/// Empty `needles` keeps everything: the analysis should start by looking at
/// the whole diff and only then decide what matters.
pub fn filter<'a>(changes: &'a [Change], needles: &[String]) -> Vec<&'a Change> {
    if needles.is_empty() {
        return changes.iter().collect();
    }
    let needles: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
    changes
        .iter()
        .filter(|change| {
            let path = change.path.to_lowercase();
            needles.iter().any(|needle| path.contains(needle))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(changes: &[Change]) -> Vec<&str> {
        changes.iter().map(|change| change.path.as_str()).collect()
    }

    #[test]
    fn identical_documents_have_no_changes() {
        let value = json!({"a": [1, {"b": "c"}]});
        assert!(diff(&value, &value, 100).is_empty());
    }

    #[test]
    fn reports_the_path_of_a_changed_scalar() {
        let changes = diff(
            &json!({"score": {"home": 0, "away": 0}}),
            &json!({"score": {"home": 1, "away": 0}}),
            100,
        );
        assert_eq!(paths(&changes), ["/score/home"]);
        assert_eq!(changes[0].before, Some(json!(0)));
        assert_eq!(changes[0].after, Some(json!(1)));
    }

    #[test]
    fn reports_added_and_removed_keys() {
        let changes = diff(&json!({"a": 1}), &json!({"b": 2}), 100);
        assert_eq!(paths(&changes), ["/a", "/b"]);
        assert_eq!(changes[0].after, None);
        assert_eq!(changes[1].before, None);
    }

    #[test]
    fn walks_into_arrays_by_index() {
        let changes = diff(&json!({"x": [1, 2]}), &json!({"x": [1, 3, 4]}), 100);
        assert_eq!(paths(&changes), ["/x/1", "/x/2"]);
    }

    #[test]
    fn keys_containing_slashes_cannot_forge_a_path() {
        let changes = diff(&json!({"a/b": 1}), &json!({"a/b": 2}), 100);
        assert_eq!(paths(&changes), ["/a~1b"]);
    }

    #[test]
    fn the_change_count_is_bounded() {
        let before = json!({"a": 1, "b": 2, "c": 3, "d": 4});
        let after = json!({"a": 9, "b": 9, "c": 9, "d": 9});
        assert_eq!(diff(&before, &after, 2).len(), 2);
    }

    #[test]
    fn filtering_is_case_insensitive_and_optional() {
        let changes = diff(
            &json!({"Score": 0, "clock": "1"}),
            &json!({"Score": 1, "clock": "2"}),
            100,
        );
        assert_eq!(filter(&changes, &[]).len(), 2);
        let only_score = filter(&changes, &["score".to_string()]);
        assert_eq!(
            paths(&only_score.into_iter().cloned().collect::<Vec<_>>()),
            ["/Score"]
        );
    }
}
