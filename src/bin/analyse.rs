//! Measures how long after a sports feed shows something the market moves.
//!
//! Reads two recordings made over the same wall-clock window and reports the
//! lag between them. This is the number the project exists to find: if it is
//! small, there is nothing here worth building a fast system for.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tick_to_trade::analysis::{
    align::{self, Leads, Matching},
    book, change_frequency, flow, observations, overlap_ns, presses, reference_asset,
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

/// What the market recording is being compared against.
#[derive(Debug, PartialEq)]
enum Against {
    /// A polled data feed: how long until the market followed it.
    Sports(PathBuf),
    /// A spectator on the public broadcast: how far the book ran ahead.
    Labels(PathBuf),
}

#[derive(Debug, PartialEq)]
struct Args {
    market: PathBuf,
    against: Option<Against>,
    min_move: f64,
    horizon_secs: u64,
    window_secs: u64,
    surge_secs: u64,
    surge_size: Option<f64>,
    paths: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            market: PathBuf::new(),
            against: None,
            min_move: 0.02,
            horizon_secs: 120,
            window_secs: 30,
            surge_secs: 5,
            surge_size: None,
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
            "--sports" | "--labels" => {
                let path: PathBuf = args
                    .next()
                    .ok_or_else(|| format!("`{arg}` expects a path"))?
                    .into();
                if parsed.against.is_some() {
                    return Err("pass either `--sports` or `--labels`, not both".into());
                }
                parsed.against = Some(if arg == "--sports" {
                    Against::Sports(path)
                } else {
                    Against::Labels(path)
                });
            }
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
            "--surge-secs" => {
                let raw = args.next().ok_or("`--surge-secs` expects a value")?;
                parsed.surge_secs = raw
                    .parse()
                    .map_err(|_| format!("`--surge-secs` expects a number, got `{raw}`"))?;
                if parsed.surge_secs == 0 {
                    return Err("`--surge-secs` must be greater than zero".into());
                }
            }
            "--surge-size" => {
                let raw = args.next().ok_or("`--surge-size` expects a value")?;
                let size: f64 = raw
                    .parse()
                    .map_err(|_| format!("`--surge-size` expects a number, got `{raw}`"))?;
                if size <= 0.0 {
                    return Err("`--surge-size` must be greater than zero".into());
                }
                parsed.surge_size = Some(size);
            }
            "--window-secs" => {
                let raw = args.next().ok_or("`--window-secs` expects a value")?;
                parsed.window_secs = raw
                    .parse()
                    .map_err(|_| format!("`--window-secs` expects a number, got `{raw}`"))?;
                if parsed.window_secs == 0 {
                    return Err("`--window-secs` must be greater than zero".into());
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

    if parsed.market.as_os_str().is_empty() || parsed.against.is_none() {
        return Err("`--market` and one of `--sports` or `--labels` are required".into());
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

/// Reports how one kind of market signal sat relative to the spectator.
fn report_leads(kind: &str, report: &Leads, window_secs: u64, signals: usize) {
    println!("\n-- {kind}: {signals} signal(s) --");

    if report.leads.is_empty() {
        println!("  No press had one within {window_secs} s of it.");
        return;
    }

    println!(
        "  {:>9}  {:>6}  {:<24}  label",
        "offset", "agrees", "signal"
    );
    for lead in &report.leads {
        println!(
            "  {:>+8.2}s  {:>6}  {:<24}  {}",
            seconds_signed(lead.offset_ns),
            if lead.agrees { "yes" } else { "NO" },
            lead.detail,
            lead.label,
        );
    }

    // A negative offset means the market moved before the key was pressed.
    let first = report
        .leads
        .iter()
        .filter(|lead| lead.offset_ns < 0)
        .count();
    let mut confirming: Vec<u64> = report
        .leads
        .iter()
        .filter(|lead| lead.offset_ns < 0 && lead.agrees)
        .map(|lead| lead.offset_ns.unsigned_abs())
        .collect();
    confirming.sort_unstable();

    println!(
        "  Market moved first on {} of {} matched press(es); {} of those agreed.",
        first,
        report.leads.len(),
        confirming.len()
    );
    if confirming.is_empty() {
        println!("  Nothing both preceded a press and pointed the right way.");
    } else {
        println!("  Lead, over the presses where it led and agreed:");
        for (name, percent) in [("p50", 50.0), ("p90", 90.0), ("max", 100.0)] {
            if let Some(value) = align::percentile(&confirming, percent) {
                println!("  {name:<6} {:>8.2}s", seconds(value));
            }
        }
    }
    println!(
        "  {} press(es) had none within {window_secs} s.",
        report.unmatched
    );
    if report.leads.len() < 5 {
        println!("  Too few matched presses to read anything into the percentiles.");
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn seconds_signed(nanos: i64) -> f64 {
    nanos as f64 / 1e9
}

fn analyse(args: &Args) -> Result<()> {
    let market = replay::read(&args.market)?;
    let against = args.against.as_ref().expect("parsing requires one");
    let other_path = match against {
        Against::Sports(path) | Against::Labels(path) => path,
    };
    let other = replay::read(other_path)?;

    println!("== Recordings ==");
    println!("  {}", describe(&args.market, &market));
    println!("  {}", describe(other_path, &other));

    let (Some(market_span), Some(other_span)) = (span_ns(&market), span_ns(&other)) else {
        bail!("one of the recordings is empty");
    };
    let Some((from, to)) = overlap_ns(market_span, other_span) else {
        bail!(
            "the recordings do not overlap in wall-clock time, so there is \
             nothing to compare; record both at once"
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
    if agreement.skipped > 0 {
        println!(
            "  {} delta(s) dropped for outcomes with no snapshot — subscribe to",
            agreement.skipped
        );
        println!("  every outcome of a market, or its book cannot be rebuilt");
    }

    match against {
        Against::Sports(_) => {
            let found = observations(&other, &args.paths, 200);
            println!("\n== Sports feed ==");
            println!("  {} change(s) observed", found.len());
            if args.paths.is_empty() {
                println!("  busiest paths (use --path to keep only the ones that are events):");
                for (path, count) in change_frequency(&other, 200).iter().take(5) {
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
        }
        Against::Labels(_) => {
            let found = presses(&other);
            println!("\n== Spectator baseline ==");
            println!("  {} press(es)", found.len());

            let Some(asset) = reference_asset(&args.market)
                .or_else(|| quotes.first().map(|quote| quote.asset.clone()))
            else {
                bail!("the market recording has no prices to compare against");
            };
            println!("  reference outcome {}", &asset[..asset.len().min(16)]);

            let window_ns = args.window_secs.saturating_mul(1_000_000_000);

            println!("\n== What the market did, and when ==");
            let moves = align::significant_moves(&quotes, &asset, args.min_move);
            let matched = align::leads(&found, &align::move_signals(&moves), window_ns);
            report_leads(
                &format!("price moves of at least {:.3}", args.min_move),
                &matched,
                args.window_secs,
                moves.len(),
            );

            let (executions, sides) = flow::trades(&market, &quotes);
            let mine: Vec<&flow::Trade> = executions
                .iter()
                .filter(|trade| trade.asset == asset)
                .collect();
            let unclear = mine
                .iter()
                .filter(|trade| trade.taker == flow::Taker::Unclear)
                .count();
            println!(
                "\n  {} trade(s) in that outcome, {unclear} of them inside the spread",
                mine.len()
            );
            match sides.rate() {
                Some(rate) => println!(
                    "  the `side` field agreed with where the trade printed on {:.1}% of {} classifiable trades",
                    rate * 100.0,
                    sides.checked
                ),
                None => println!("  no classifiable trade to check the `side` field against"),
            }

            let threshold = match args.surge_size {
                Some(size) => size,
                None => {
                    let mut sizes: Vec<f64> = mine.iter().map(|trade| trade.size).collect();
                    let scaled = median(&mut sizes).map(|value| value * 5.0);
                    let value = scaled.filter(|value| *value > 0.0).unwrap_or(1.0);
                    println!(
                        "  surge threshold {value:.0} (5x the median trade; set --surge-size to override)"
                    );
                    value
                }
            };
            let surge_window_ns = args.surge_secs.saturating_mul(1_000_000_000);
            let bursts = flow::surges(&executions, &asset, surge_window_ns, threshold);
            let flow_matched = align::leads(&found, &flow::surge_signals(&bursts), window_ns);
            report_leads(
                &format!("net taking of {threshold:.0} within {}s", args.surge_secs),
                &flow_matched,
                args.window_secs,
                bursts.len(),
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
    fn a_market_and_something_to_compare_it_with_are_required() {
        assert!(parse_args(args(&["--market", "a.jsonl"])).is_err());
        assert!(parse_args(args(&["--labels", "l.jsonl"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    #[test]
    fn parses_the_two_recordings_and_the_defaults() {
        let parsed = parse_args(args(&["--market", "m.jsonl", "--sports", "s.jsonl"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.market, PathBuf::from("m.jsonl"));
        assert_eq!(
            parsed.against,
            Some(Against::Sports(PathBuf::from("s.jsonl")))
        );
        assert_eq!(parsed.min_move, 0.02);
        assert_eq!(parsed.horizon_secs, 120);
        assert_eq!(parsed.window_secs, 30);
        assert!(parsed.paths.is_empty());
    }

    #[test]
    fn labels_are_an_alternative_comparison_not_an_extra_one() {
        let parsed = parse_args(args(&["--market", "m", "--labels", "l"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.against, Some(Against::Labels(PathBuf::from("l"))));
        assert!(parse_args(args(&["--market", "m", "--labels", "l", "--sports", "s"])).is_err());
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
