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
    book, change_frequency, edge, flow, observations, overlap_ns, presses, reference_asset,
    replay::{self, Record},
    span_ns,
};

const USAGE: &str = "\
usage: analyse --market <file> [--sports <file> | --labels <file>] [options]
       ... repeated, one pair per recording

    --market <file>        a polymarket-*.jsonl recording; repeat it to pool
                           several games into one distribution
    --sports <file>        a sports-*.jsonl recording over the same window
    --labels <file>        a labels-*.jsonl recording over the same window

    --min-move <p>         mid move counting as a signal (default: 0.02)
    --window-secs <n>      how far either side of a press to look for the
                           nearest signal (default: 30)
    --horizon-secs <n>     how long to wait for the market to follow a feed
                           change (default: 120)
    --path <substring>     only treat changes under matching paths as events;
                           repeatable, default: every change counts

    --surge-secs <n>       window over which taking is netted (default: 5)
    --surge-size <n>       net size that counts as a surge; the default is
                           5x the median trade size in the recording
    --hold-secs <n>        how long a trial position is held (default: 30)
    --fee-bps <n>          taker fee charged on each leg (default: 0)

    -h, --help             print this message

With --labels: how far the order book ran ahead of an ordinary spectator.
With --sports: how long after the feed showed something did the market move.
With neither: what following the flow would have paid, against reaction delay.

One game is not a sample. Give several pairs at once and the presses are
pooled into a single distribution:

    analyse --market m1.jsonl --labels l1.jsonl \
            --market m2.jsonl --labels l2.jsonl
";

/// What a market recording is being compared against.
#[derive(Debug, PartialEq)]
enum Against {
    /// A polled data feed: how long until the market followed it.
    Sports(PathBuf),
    /// A spectator on the public broadcast: how far the book ran ahead.
    Labels(PathBuf),
}

/// One recording, and whatever it is being compared with.
#[derive(Debug, PartialEq)]
struct Pair {
    market: PathBuf,
    against: Option<Against>,
}

#[derive(Debug, PartialEq)]
struct Args {
    pairs: Vec<Pair>,
    min_move: f64,
    horizon_secs: u64,
    window_secs: u64,
    surge_secs: u64,
    surge_size: Option<f64>,
    hold_secs: u64,
    fee_bps: f64,
    paths: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            pairs: Vec::new(),
            min_move: 0.02,
            horizon_secs: 120,
            window_secs: 30,
            surge_secs: 5,
            surge_size: None,
            hold_secs: 30,
            fee_bps: 0.0,
            paths: Vec::new(),
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
            // Each market opens a pair; the comparison that follows attaches
            // to it, so several games read left to right as they were run.
            "--market" => parsed.pairs.push(Pair {
                market: value()?.into(),
                against: None,
            }),
            "--sports" | "--labels" => {
                let path: PathBuf = value()?.into();
                let against = if arg == "--sports" {
                    Against::Sports(path)
                } else {
                    Against::Labels(path)
                };
                match parsed.pairs.last_mut() {
                    Some(pair) if pair.against.is_none() => pair.against = Some(against),
                    Some(_) => {
                        return Err(format!(
                            "`{arg}` follows a recording that already has one; give each \
                             comparison its own `--market`"
                        ));
                    }
                    None => return Err(format!("`{arg}` must follow a `--market`")),
                }
            }
            "--path" => parsed.paths.push(value()?),
            "--min-move" => {
                let raw = value()?;
                parsed.min_move = raw
                    .parse()
                    .map_err(|_| format!("`--min-move` expects a number, got `{raw}`"))?;
                if !(parsed.min_move > 0.0 && parsed.min_move < 1.0) {
                    return Err("`--min-move` is a probability move, so 0 < p < 1".into());
                }
            }
            "--window-secs" => {
                parsed.window_secs = positive(&value()?, "--window-secs")?;
            }
            "--horizon-secs" => {
                parsed.horizon_secs = positive(&value()?, "--horizon-secs")?;
            }
            "--surge-secs" => {
                parsed.surge_secs = positive(&value()?, "--surge-secs")?;
            }
            "--hold-secs" => {
                parsed.hold_secs = positive(&value()?, "--hold-secs")?;
            }
            "--surge-size" => {
                let raw = value()?;
                let size: f64 = raw
                    .parse()
                    .map_err(|_| format!("`--surge-size` expects a number, got `{raw}`"))?;
                if size <= 0.0 {
                    return Err("`--surge-size` must be greater than zero".into());
                }
                parsed.surge_size = Some(size);
            }
            "--fee-bps" => {
                let raw = value()?;
                parsed.fee_bps = raw
                    .parse()
                    .map_err(|_| format!("`--fee-bps` expects a number, got `{raw}`"))?;
                if parsed.fee_bps < 0.0 {
                    return Err("`--fee-bps` cannot be negative".into());
                }
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    if parsed.pairs.is_empty() {
        return Err("at least one `--market` is required".into());
    }
    Ok(Some(parsed))
}

fn positive(raw: &str, flag: &str) -> Result<u64, String> {
    let parsed: u64 = raw
        .parse()
        .map_err(|_| format!("`{flag}` expects a number, got `{raw}`"))?;
    if parsed == 0 {
        return Err(format!("`{flag}` must be greater than zero"));
    }
    Ok(parsed)
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

/// Prints what the trading looked like and returns the bursts found in it.
fn describe_flow(
    market: &[Record],
    quotes: &[align::Quote],
    asset: &str,
    args: &Args,
) -> Vec<flow::Surge> {
    let (executions, sides) = flow::trades(market, quotes);
    let mine: Vec<&flow::Trade> = executions
        .iter()
        .filter(|trade| trade.asset == asset)
        .collect();
    let unclear = mine
        .iter()
        .filter(|trade| trade.taker == flow::Taker::Unclear)
        .count();

    println!(
        "  {} trade(s) in that outcome, {unclear} of them inside the spread",
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
            let value = median(&mut sizes)
                .map(|value| value * 5.0)
                .filter(|value| *value > 0.0)
                .unwrap_or(1.0);
            println!(
                "  surge threshold {value:.0} (5x the median trade; set --surge-size to override)"
            );
            value
        }
    };

    let window_ns = args.surge_secs.saturating_mul(1_000_000_000);
    let bursts = flow::surges(&executions, asset, window_ns, threshold);
    println!(
        "  {} surge(s) of {threshold:.0} net within {}s",
        bursts.len(),
        args.surge_secs
    );
    bursts
}

/// Reports what following each surge would have paid, against how late you
/// were to it.
fn report_edge(surges: &[flow::Surge], quotes: &[align::Quote], asset: &str, args: &Args) {
    println!("\n== What following the flow would have paid ==");
    if surges.is_empty() {
        println!("  No surge to trade. Nothing to say about the edge.");
        return;
    }

    let fee = args.fee_bps / 10_000.0;
    println!(
        "  Holding {}s, crossing the spread both ways, {} bps per leg.",
        args.hold_secs, args.fee_bps
    );
    println!("  Every fill is at the touch, so this is an upper bound: a thin");
    println!("  book will not absorb size at those prices.");
    println!(
        "\n  {:>9}  {:>7}  {:>10}  {:>8}  {:>10}",
        "react", "trials", "median", "won", "worst"
    );

    let hold_ns = args.hold_secs.saturating_mul(1_000_000_000);
    for delay_ms in edge::REACTION_LADDER_MS {
        let result = edge::study(
            surges,
            quotes,
            asset,
            delay_ms.saturating_mul(1_000_000),
            hold_ns,
            fee,
        );
        let trials = result.trials.len();
        if trials == 0 {
            println!("  {delay_ms:>7}ms  {:>7}  {:>10}", 0, "-");
            continue;
        }
        println!(
            "  {delay_ms:>7}ms  {trials:>7}  {:>+10.4}  {:>7.0}%  {:>+10.4}",
            result.median_net().unwrap_or(0.0),
            100.0 * result.wins() as f64 / trials as f64,
            result.worst().unwrap_or(0.0),
        );
    }
    println!("\n  Median is in probability points per unit of size, after costs.");
    println!("  A median at or below zero means the move is already in the price");
    println!("  by the time the surge is visible, and there is nothing to chase.");
}

/// What one recording contributed, for pooling with the rest.
#[derive(Default)]
struct Contribution {
    price: Leads,
    flow: Leads,
    presses: usize,
}

fn merge(into: &mut Leads, from: Leads) {
    into.leads.extend(from.leads);
    into.unmatched += from.unmatched;
}

/// Works through one recording and whatever it is compared against.
fn analyse_pair(pair: &Pair, args: &Args) -> Result<Contribution> {
    let market = replay::read(&pair.market)?;
    println!("\n== {} ==", describe(&pair.market, &market));

    let other = match pair.against.as_ref() {
        Some(Against::Sports(path) | Against::Labels(path)) => {
            let records = replay::read(path)?;
            println!("   {}", describe(path, &records));
            Some(records)
        }
        None => None,
    };

    let Some(market_span) = span_ns(&market) else {
        bail!("`{}` is empty", pair.market.display());
    };
    if let Some(other) = &other {
        let Some(other_span) = span_ns(other) else {
            bail!("the comparison recording is empty");
        };
        let Some((from, to)) = overlap_ns(market_span, other_span) else {
            bail!(
                "`{}` does not overlap its comparison in wall-clock time, so there is \
                 nothing to compare; record both at once",
                pair.market.display()
            );
        };
        println!("   overlap: {:.1} s", seconds(to - from));
    }

    let (quotes, agreement) = book::replay(&market);
    print!("   {} mid-price moves", quotes.len());
    match agreement.rate() {
        Some(rate) => println!(", book agrees with the exchange on {:.1}%", rate * 100.0),
        None => println!(", no exchange touch to check against"),
    }
    if agreement.skipped > 0 {
        println!(
            "   {} delta(s) dropped for outcomes with no snapshot — subscribe to every \
             outcome of a market",
            agreement.skipped
        );
    }

    let Some(asset) =
        reference_asset(&pair.market).or_else(|| quotes.first().map(|quote| quote.asset.clone()))
    else {
        bail!("`{}` has no prices to work from", pair.market.display());
    };

    let mut contribution = Contribution::default();

    match (pair.against.as_ref(), other) {
        (Some(Against::Sports(_)), Some(other)) => {
            let found = observations(&other, &args.paths, 200);
            println!("   {} feed change(s)", found.len());
            if args.paths.is_empty() {
                println!("   busiest paths (use --path to keep only the ones that are events):");
                for (path, count) in change_frequency(&other, 200).iter().take(5) {
                    println!(
                        "     {count:>4} x {}",
                        path.chars().take(70).collect::<String>()
                    );
                }
            }
            let horizon_ns = args.horizon_secs.saturating_mul(1_000_000_000);
            let matched = align::match_observations(&found, &quotes, args.min_move, horizon_ns);
            report(&matched, args.horizon_secs);
        }
        (Some(Against::Labels(_)), Some(other)) => {
            let found = presses(&other);
            contribution.presses = found.len();
            println!("   {} press(es)", found.len());

            let window_ns = args.window_secs.saturating_mul(1_000_000_000);
            let moves = align::significant_moves(&quotes, &asset, args.min_move);
            contribution.price = align::leads(&found, &align::move_signals(&moves), window_ns);
            report_leads(
                &format!("price moves of at least {:.3}", args.min_move),
                &contribution.price,
                args.window_secs,
                moves.len(),
            );

            println!();
            let bursts = describe_flow(&market, &quotes, &asset, args);
            contribution.flow = align::leads(&found, &flow::surge_signals(&bursts), window_ns);
            report_leads(
                &format!("net taking within {}s", args.surge_secs),
                &contribution.flow,
                args.window_secs,
                bursts.len(),
            );
            report_edge(&bursts, &quotes, &asset, args);
        }
        _ => {
            println!();
            let bursts = describe_flow(&market, &quotes, &asset, args);
            report_edge(&bursts, &quotes, &asset, args);
        }
    }

    Ok(contribution)
}

fn analyse(args: &Args) -> Result<()> {
    let mut pooled = Contribution::default();
    let mut recordings = 0;

    for pair in &args.pairs {
        let contribution = analyse_pair(pair, args)?;
        if contribution.presses > 0 {
            recordings += 1;
        }
        pooled.presses += contribution.presses;
        merge(&mut pooled.price, contribution.price);
        merge(&mut pooled.flow, contribution.flow);
    }

    // One game is never a sample. Pooling is the whole reason several
    // recordings can be given at once, so it is reported separately rather
    // than left for the reader to add up by eye.
    if recordings > 1 {
        println!(
            "\n\n== Pooled over {recordings} recording(s), {} press(es) ==",
            pooled.presses
        );
        report_leads(
            "price moves, pooled",
            &pooled.price,
            args.window_secs,
            pooled.price.leads.len(),
        );
        report_leads(
            "net taking, pooled",
            &pooled.flow,
            args.window_secs,
            pooled.flow.leads.len(),
        );
    } else if args.pairs.len() > 1 {
        println!("\n\nNothing to pool: only one recording contributed presses.");
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
    fn a_market_recording_stands_on_its_own() {
        // With nothing to compare against, the edge study still runs.
        let parsed = parse_args(args(&["--market", "a.jsonl"])).unwrap().unwrap();
        assert_eq!(parsed.pairs.len(), 1);
        assert!(parsed.pairs[0].against.is_none());
        assert_eq!(parsed.hold_secs, 30);
        assert_eq!(parsed.fee_bps, 0.0);
    }

    #[test]
    fn a_comparison_without_a_market_is_refused() {
        assert!(parse_args(args(&["--labels", "l.jsonl"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    #[test]
    fn trial_settings_are_validated() {
        assert!(parse_args(args(&["--market", "m", "--hold-secs", "0"])).is_err());
        assert!(parse_args(args(&["--market", "m", "--fee-bps", "-1"])).is_err());
        let parsed = parse_args(args(&["--market", "m", "--fee-bps", "25"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.fee_bps, 25.0);
    }

    #[test]
    fn parses_one_pair_and_the_defaults() {
        let parsed = parse_args(args(&["--market", "m.jsonl", "--sports", "s.jsonl"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.pairs[0].market, PathBuf::from("m.jsonl"));
        assert_eq!(
            parsed.pairs[0].against,
            Some(Against::Sports(PathBuf::from("s.jsonl")))
        );
        assert_eq!(parsed.min_move, 0.02);
        assert_eq!(parsed.horizon_secs, 120);
        assert_eq!(parsed.window_secs, 30);
        assert!(parsed.paths.is_empty());
    }

    #[test]
    fn several_games_pair_up_in_the_order_they_are_given() {
        let parsed = parse_args(args(&[
            "--market", "m1", "--labels", "l1", "--market", "m2", "--labels", "l2",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.pairs.len(), 2);
        assert_eq!(parsed.pairs[0].market, PathBuf::from("m1"));
        assert_eq!(
            parsed.pairs[1].against,
            Some(Against::Labels(PathBuf::from("l2")))
        );
    }

    #[test]
    fn a_recording_can_only_be_compared_with_one_thing() {
        // Two comparisons for one market is a typo, not a request: silently
        // keeping the first would analyse something other than what was asked.
        assert!(parse_args(args(&["--market", "m", "--labels", "l", "--sports", "s"])).is_err());
    }

    #[test]
    fn a_market_may_stand_alone_among_pairs() {
        let parsed = parse_args(args(&[
            "--market", "m1", "--market", "m2", "--labels", "l2",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.pairs.len(), 2);
        assert!(parsed.pairs[0].against.is_none());
        assert!(parsed.pairs[1].against.is_some());
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
