//! Compares sports feeds recorded over the same window.
//!
//! Vendors publish latency figures that are not comparable with each other
//! and that none of them measures the same way. The only number worth having
//! is one measured here: record two candidates side by side through the same
//! match and see which one showed a change first.
//!
//! The measurement makes no attempt to understand either document. It asks
//! only when each feed changed, and how those instants line up. That is what
//! keeps it usable on a feed whose schema nobody has read — and it is also
//! its weakness, so the report says as much: two feeds changing near each
//! other may be changing for unrelated reasons, and `--path` is how that is
//! narrowed down.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tick_to_trade::analysis::{
    align::{self, Signal},
    change_frequency, observations,
    replay::{self},
};

const USAGE: &str = "\
usage: compare --feed <file> --feed <file> [options]

    --feed <file>          a sports-*.jsonl recording; give two or more, the
                           first is the baseline the others are measured
                           against
    --path <substring>     only count changes under matching paths;
                           repeatable, default: every change counts
    --window-secs <n>      how far either side to look for the same change
                           (default: 20)
    -h, --help             print this message

Record the candidates side by side through one match, then compare them:

    record --feed sports --source espn &
    record --feed sports --source vendor &
";

#[derive(Debug, PartialEq)]
struct Args {
    feeds: Vec<PathBuf>,
    paths: Vec<String>,
    window_secs: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            feeds: Vec::new(),
            paths: Vec::new(),
            window_secs: 20,
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;

    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("`{arg}` expects a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--feed" => parsed.feeds.push(value()?.into()),
            "--path" => parsed.paths.push(value()?),
            "--window-secs" => {
                let raw = value()?;
                parsed.window_secs = raw
                    .parse()
                    .map_err(|_| format!("`--window-secs` expects a number, got `{raw}`"))?;
                if parsed.window_secs == 0 {
                    return Err("`--window-secs` must be greater than zero".into());
                }
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    if parsed.feeds.len() < 2 {
        return Err("two or more `--feed` recordings are needed to compare anything".into());
    }
    Ok(Some(parsed))
}

fn seconds(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

fn signed_seconds(nanos: i64) -> f64 {
    nanos as f64 / 1e9
}

/// Median of an already sorted slice.
fn median(sorted: &[u64]) -> Option<u64> {
    align::percentile(sorted, 50.0)
}

struct Feed {
    label: String,
    /// When the feed was seen to change, in wall clock.
    changes: Vec<u64>,
    /// How wide the detection window was for each change.
    uncertainty: Vec<u64>,
    span_ns: u64,
}

fn load(path: &Path, paths: &[String]) -> Result<Feed> {
    let records = replay::read(path)?;
    let found = observations(&records, paths, 200);

    let mut changes: Vec<u64> = found.iter().map(|each| each.at_unix_ns).collect();
    changes.sort_unstable();
    let mut uncertainty: Vec<u64> = found
        .iter()
        .map(|each| each.at_unix_ns.saturating_sub(each.from_unix_ns))
        .collect();
    uncertainty.sort_unstable();

    let span_ns = records
        .last()
        .zip(records.first())
        .map(|(last, first)| last.t_unix_ns.saturating_sub(first.t_unix_ns))
        .unwrap_or(0);

    Ok(Feed {
        label: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        changes,
        uncertainty,
        span_ns,
    })
}

/// Gaps between consecutive changes: how often the feed says anything at all.
fn cadence(changes: &[u64]) -> Vec<u64> {
    let mut gaps: Vec<u64> = changes
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .collect();
    gaps.sort_unstable();
    gaps
}

fn run(args: &Args) -> Result<()> {
    let feeds: Vec<Feed> = args
        .feeds
        .iter()
        .map(|path| load(path, &args.paths))
        .collect::<Result<_>>()?;

    println!("== Each feed on its own ==");
    println!(
        "  {:<30} {:>9} {:>8} {:>11} {:>13}",
        "recording", "window", "changes", "cadence", "uncertainty"
    );
    for feed in &feeds {
        let gaps = cadence(&feed.changes);
        let cadence_text = median(&gaps)
            .map(|value| format!("{:.1} s", seconds(value)))
            .unwrap_or_else(|| "-".into());
        let uncertainty_text = median(&feed.uncertainty)
            .map(|value| format!("{:.2} s", seconds(value)))
            .unwrap_or_else(|| "-".into());
        println!(
            "  {:<30} {:>8.0}s {:>8} {:>11} {:>13}",
            feed.label.chars().take(30).collect::<String>(),
            seconds(feed.span_ns),
            feed.changes.len(),
            cadence_text,
            uncertainty_text
        );
    }
    println!("\n  Cadence is the median gap between changes: a feed that only says");
    println!("  something every ten seconds cannot beat a market that reprices in");
    println!("  seconds, however fast the request is. Uncertainty is the width of");
    println!("  the window each change was caught in — the price of polling.");

    let (baseline, others) = feeds.split_first().expect("two or more were required");
    if baseline.changes.is_empty() {
        bail!(
            "`{}` showed no change at all, so there is nothing to measure the others \
             against",
            baseline.label
        );
    }

    let presses: Vec<(u64, String, String)> = baseline
        .changes
        .iter()
        .map(|at| (*at, "change".to_string(), "neutral".to_string()))
        .collect();
    let window_ns = args.window_secs.saturating_mul(1_000_000_000);

    println!("\n== Against {} ==", baseline.label);
    for feed in others {
        let signals: Vec<Signal> = feed
            .changes
            .iter()
            .map(|at| Signal {
                at_unix_ns: *at,
                rose: true,
                detail: String::new(),
            })
            .collect();
        let matched = align::leads(&presses, &signals, window_ns);

        println!("\n  {}", feed.label);
        if matched.leads.is_empty() {
            println!(
                "    nothing within {} s of any change in the baseline",
                args.window_secs
            );
            continue;
        }

        let earlier = matched
            .leads
            .iter()
            .filter(|lead| lead.offset_ns < 0)
            .count();
        let mut ahead: Vec<u64> = matched
            .leads
            .iter()
            .filter(|lead| lead.offset_ns < 0)
            .map(|lead| lead.offset_ns.unsigned_abs())
            .collect();
        ahead.sort_unstable();

        println!(
            "    matched {} of {} baseline changes, {} unmatched",
            matched.leads.len(),
            presses.len(),
            matched.unmatched
        );
        println!(
            "    showed the change first on {earlier} of {}",
            matched.leads.len()
        );
        if let Some(value) = median(&ahead) {
            println!("    median lead when it was first: {:.2} s", seconds(value));
        }

        let mut offsets: Vec<i64> = matched.leads.iter().map(|lead| lead.offset_ns).collect();
        offsets.sort_unstable();
        let middle = offsets[offsets.len() / 2];
        println!(
            "    median offset overall: {:+.2} s ({})",
            signed_seconds(middle),
            if middle < 0 { "ahead" } else { "behind" }
        );
    }

    println!("\n  This counts coincidence in time, not sameness of event. Two feeds");
    println!("  changing close together may be changing for unrelated reasons, so");
    println!("  narrow it with --path before reading a winner into these numbers.");

    if args.paths.is_empty() {
        println!("\n  Busiest paths in the baseline, to choose from:");
        let records = replay::read(&args.feeds[0]).context("re-reading the baseline")?;
        for (path, count) in change_frequency(&records, 200).iter().take(5) {
            println!(
                "    {count:>4} x {}",
                path.chars().take(68).collect::<String>()
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Some(args)) => args,
        Err(error) => {
            eprintln!("error: {error}\n");
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match run(&args).context("comparison failed") {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn one_feed_is_not_a_comparison() {
        assert!(parse_args(args(&["--feed", "a.jsonl"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    #[test]
    fn two_or_more_feeds_are_accepted_in_order() {
        let parsed = parse_args(args(&["--feed", "a", "--feed", "b", "--feed", "c"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.feeds.len(), 3);
        assert_eq!(parsed.feeds[0], PathBuf::from("a"));
        assert_eq!(parsed.window_secs, 20);
    }

    #[test]
    fn path_filters_accumulate() {
        let parsed = parse_args(args(&[
            "--feed", "a", "--feed", "b", "--path", "score", "--path", "clock",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.paths, ["score", "clock"]);
    }

    #[test]
    fn a_zero_window_matches_nothing_and_is_refused() {
        assert!(parse_args(args(&["--feed", "a", "--feed", "b", "--window-secs", "0"])).is_err());
    }

    #[test]
    fn cadence_is_the_gaps_between_changes_sorted() {
        assert_eq!(cadence(&[0, 10, 30, 35]), vec![5, 10, 20]);
        assert!(cadence(&[7]).is_empty());
        assert!(cadence(&[]).is_empty());
    }

    #[test]
    fn a_median_of_nothing_is_nothing() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5, 10, 15]), Some(10));
    }
}
