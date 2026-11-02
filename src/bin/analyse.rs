//! Measures how long after a sports feed shows something the market moves.
//!
//! Reads two recordings made over the same wall-clock window and reports the
//! lag between them. This is the number the project exists to find: if it is
//! small, there is nothing here worth building a fast system for.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tick_to_trade::analysis::{
    align::{self, Matching},
    book, change_frequency, observations, overlap_ns,
    replay::{self, Record},
    span_ns,
};

const USAGE: &str = "\
usage: analyse --market <file> --sports <file> [options]

    --market <file>        a polymarket-*.jsonl recording
    --sports <file>        a sports-*.jsonl recording over the same window
    --min-move <p>         mid move counting as a reaction (default: 0.02)
    --horizon-secs <n>     how long to wait for one (default: 120)
    --path <substring>     only treat changes under matching paths as events;
                           repeatable, default: every change counts
    -h, --help             print this message
";

#[derive(Debug, PartialEq)]
struct Args {
    market: PathBuf,
    sports: PathBuf,
    min_move: f64,
    horizon_secs: u64,
    paths: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            market: PathBuf::new(),
            sports: PathBuf::new(),
            min_move: 0.02,
            horizon_secs: 120,
            paths: Vec::new(),
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--market" => parsed.market = args.next().ok_or("`--market` expects a path")?.into(),
            "--sports" => parsed.sports = args.next().ok_or("`--sports` expects a path")?.into(),
            "--path" => parsed
                .paths
                .push(args.next().ok_or("`--path` expects a substring")?),
            "--min-move" => {
                let raw = args.next().ok_or("`--min-move` expects a value")?;
                parsed.min_move = raw
                    .parse()
                    .map_err(|_| format!("`--min-move` expects a number, got `{raw}`"))?;
                if !(parsed.min_move > 0.0 && parsed.min_move < 1.0) {
                    return Err("`--min-move` is a probability move, so 0 < p < 1".into());
                }
            }
            "--horizon-secs" => {
                let raw = args.next().ok_or("`--horizon-secs` expects a value")?;
                parsed.horizon_secs = raw
                    .parse()
                    .map_err(|_| format!("`--horizon-secs` expects a number, got `{raw}`"))?;
                if parsed.horizon_secs == 0 {
                    return Err("`--horizon-secs` must be greater than zero".into());
                }
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    if parsed.market.as_os_str().is_empty() || parsed.sports.as_os_str().is_empty() {
        return Err("both `--market` and `--sports` are required".into());
    }
    Ok(Some(parsed))
}

fn seconds(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

fn describe(path: &Path, records: &[Record]) -> String {
    let gaps = replay::missing_sequences(records);
    let span = span_ns(records)
        .map(|(from, to)| seconds(to.saturating_sub(from)))
        .unwrap_or(0.0);
    format!(
        "{:<28} {:>6} lines  {:>8.1} s  {}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        records.len(),
        span,
        if gaps.is_empty() {
            "no sequence gaps".to_string()
        } else {
            format!("{} SEQUENCE GAPS", gaps.len())
        }
    )
}

fn report(matched: &Matching, horizon_secs: u64) {
    println!(
        "
== Lag from feed change to market move =="
    );
    if matched.lags.is_empty() {
        println!("  No market reaction followed any feed change within {horizon_secs} s.");
        println!("  That is a result, not a failure: on this sample the market did not");
        println!("  visibly respond to anything the feed showed.");
        return;
    }

    println!(
        "  {:>9}  {:>9}  {:>13}  changed paths",
        "lag min", "lag max", "mid move"
    );
    for lag in &matched.lags {
        println!(
            "  {:>8.2}s  {:>8.2}s  {:>5.3} -> {:<5.3}  {}",
            seconds(lag.lag_min_ns),
            seconds(lag.lag_max_ns),
            lag.reaction.from,
            lag.reaction.to,
            lag.observation.label.chars().take(60).collect::<String>(),
        );
    }

    let mut sorted: Vec<u64> = matched.lags.iter().map(|lag| lag.lag_min_ns).collect();
    sorted.sort_unstable();
    println!(
        "
  Lower bound on the lag, over {} market move(s):",
        matched.lags.len()
    );
    for (label, percent) in [("p50", 50.0), ("p90", 90.0), ("max", 100.0)] {
        if let Some(value) = align::percentile(&sorted, percent) {
            println!("  {label:<6} {:>8.2}s", seconds(value));
        }
    }
    println!(
        "  {} feed change(s) drew no move within {horizon_secs} s.",
        matched.ignored
    );
    println!(
        "  {} shared a move with a nearer change and were not counted twice.",
        matched.contested
    );
    if matched.contested > matched.lags.len() {
        println!("  Most changes were noise rather than events. Narrow the feed");
        println!("  with --path before reading anything into these numbers.");
    }
}

fn analyse(args: &Args) -> Result<()> {
    let market = replay::read(&args.market)?;
    let sports = replay::read(&args.sports)?;

    println!("== Recordings ==");
    println!("  {}", describe(&args.market, &market));
    println!("  {}", describe(&args.sports, &sports));

    let (Some(market_span), Some(sports_span)) = (span_ns(&market), span_ns(&sports)) else {
        bail!("one of the recordings is empty");
    };
    let Some((from, to)) = overlap_ns(market_span, sports_span) else {
        bail!(
            "the recordings do not overlap in wall-clock time, so there is \
             nothing to compare; record both feeds at once"
        );
    };
    println!("  overlap: {:.1} s", seconds(to - from));

    let (quotes, agreement) = book::replay(&market);
    println!("\n== Market ==");
    println!("  {} mid-price moves rebuilt from the book", quotes.len());
    match agreement.rate() {
        Some(rate) => println!(
            "  book reconstruction agrees with the exchange touch on {:.1}% of {} updates",
            rate * 100.0,
            agreement.checked
        ),
        None => println!("  no exchange-reported touch to check the reconstruction against"),
    }

    let found = observations(&sports, &args.paths, 200);
    println!("\n== Sports feed ==");
    println!("  {} change(s) observed", found.len());
    if args.paths.is_empty() {
        println!("  busiest paths (use --path to keep only the ones that are events):");
        for (path, count) in change_frequency(&sports, 200).iter().take(5) {
            println!(
                "    {count:>4} x {}",
                path.chars().take(72).collect::<String>()
            );
        }
    }
    if !found.is_empty() {
        let mut uncertainty: Vec<u64> = found
            .iter()
            .map(|observation| observation.at_unix_ns - observation.from_unix_ns)
            .collect();
        uncertainty.sort_unstable();
        if let Some(median) = align::percentile(&uncertainty, 50.0) {
            println!(
                "  median detection uncertainty: {:.2} s (the price of polling)",
                seconds(median)
            );
        }
    }

    let horizon_ns = args.horizon_secs.saturating_mul(1_000_000_000);
    let matched = align::match_observations(&found, &quotes, args.min_move, horizon_ns);
    report(&matched, args.horizon_secs);
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

    match analyse(&args).context("analysis failed") {
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
    fn both_recordings_are_required() {
        assert!(parse_args(args(&["--market", "a.jsonl"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    #[test]
    fn parses_the_two_recordings_and_the_defaults() {
        let parsed = parse_args(args(&["--market", "m.jsonl", "--sports", "s.jsonl"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.market, PathBuf::from("m.jsonl"));
        assert_eq!(parsed.sports, PathBuf::from("s.jsonl"));
        assert_eq!(parsed.min_move, 0.02);
        assert_eq!(parsed.horizon_secs, 120);
        assert!(parsed.paths.is_empty());
    }

    #[test]
    fn path_filters_accumulate() {
        let parsed = parse_args(args(&[
            "--market", "m", "--sports", "s", "--path", "score", "--path", "clock",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.paths, ["score", "clock"]);
    }

    #[test]
    fn a_move_threshold_outside_zero_to_one_is_not_a_probability() {
        let bad = ["--market", "m", "--sports", "s", "--min-move", "1.5"];
        assert!(parse_args(args(&bad)).is_err());
        let zero = ["--market", "m", "--sports", "s", "--min-move", "0"];
        assert!(parse_args(args(&zero)).is_err());
    }

    #[test]
    fn help_requests_no_analysis() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
    }
}
