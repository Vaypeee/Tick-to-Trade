//! Replays a market recording through the trading rules.
//!
//! No network, no orders: it reads what the market showed and reports what
//! the rules in `[decision]` would have done with it.
//!
//! The expected move is the one thing this cannot know, so it is swept rather
//! than guessed. The table it prints answers a better question than any
//! single number would: how large a move would a model have to predict,
//! correctly and on average, before these rules stop losing money?

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tick_to_trade::{
    analysis::{
        align::Quote,
        book, flow, reference_asset,
        replay::{self},
    },
    config::Config,
    strategy::{
        decision::{Limits, Touch},
        paper::{self, Signal},
    },
};

const USAGE: &str = "\
usage: paper --market <file> [options]

    --market <file>        a polymarket-*.jsonl recording
    --config <path>        rules to apply (default: config.toml)
    --react-ms <n>         how late the entry is (default: 250)
    --hold-secs <n>        how long a position is held (default: 30)
    --surge-secs <n>       window over which taking is netted (default: 5)
    --surge-size <n>       net size that counts as a surge; the default is
                           5x the median trade size in the recording
    -h, --help             print this message

Signals come from aggressive order flow. Every entry crosses the spread and
every exit crosses it back, so the cost is a full spread plus fees on both
legs, taken from the book as it stood.
";

#[derive(Debug, PartialEq)]
struct Args {
    market: PathBuf,
    config: PathBuf,
    react_ms: u64,
    hold_secs: u64,
    surge_secs: u64,
    surge_size: Option<f64>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            market: PathBuf::new(),
            config: PathBuf::from("config.toml"),
            react_ms: 250,
            hold_secs: 30,
            surge_secs: 5,
            surge_size: None,
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
            "--market" => parsed.market = value()?.into(),
            "--config" => parsed.config = value()?.into(),
            "--react-ms" => {
                parsed.react_ms = value()?
                    .parse()
                    .map_err(|_| "`--react-ms` expects a number")?
            }
            "--hold-secs" => {
                let secs: u64 = value()?
                    .parse()
                    .map_err(|_| "`--hold-secs` expects a number")?;
                if secs == 0 {
                    return Err("`--hold-secs` must be greater than zero".into());
                }
                parsed.hold_secs = secs;
            }
            "--surge-secs" => {
                let secs: u64 = value()?
                    .parse()
                    .map_err(|_| "`--surge-secs` expects a number")?;
                if secs == 0 {
                    return Err("`--surge-secs` must be greater than zero".into());
                }
                parsed.surge_secs = secs;
            }
            "--surge-size" => {
                let size: f64 = value()?
                    .parse()
                    .map_err(|_| "`--surge-size` expects a number")?;
                if size <= 0.0 {
                    return Err("`--surge-size` must be greater than zero".into());
                }
                parsed.surge_size = Some(size);
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    if parsed.market.as_os_str().is_empty() {
        return Err("`--market` is required".into());
    }
    Ok(Some(parsed))
}

/// The book as it stood, from the replayed quote series.
fn touch_at(quotes: &[Quote], asset: &str, at_unix_ns: u64) -> Option<Touch> {
    quotes
        .iter()
        .rev()
        .find(|quote| quote.asset == asset && quote.at_unix_ns <= at_unix_ns)
        .map(|quote| Touch {
            bid: quote.bid,
            ask: quote.ask,
            bid_size: quote.bid_size,
            ask_size: quote.ask_size,
        })
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

/// The moves the sweep tries, in probability points.
const LADDER: [f64; 7] = [0.005, 0.01, 0.02, 0.03, 0.05, 0.08, 0.12];

fn run(args: &Args) -> Result<()> {
    let config = Config::load(&args.config)?;
    let limits = Limits {
        min_edge: config.decision.min_edge,
        max_position: config.decision.max_position,
        size: config.decision.size,
        max_spread: config.decision.max_spread,
        require_touch_depth: config.decision.require_touch_depth,
        fee_fraction: config.execution.fee_bps / 10_000.0,
        max_signal_age_ns: config.decision.max_signal_age_ms.saturating_mul(1_000_000),
    };

    let records = replay::read(&args.market)?;
    let (quotes, agreement) = book::replay(&records);
    let Some(asset) =
        reference_asset(&args.market).or_else(|| quotes.first().map(|quote| quote.asset.clone()))
    else {
        bail!("the market recording has no prices to work from");
    };

    println!("== Market ==");
    println!(
        "  {} mid-price moves, reference outcome {}",
        quotes.len(),
        &asset[..asset.len().min(16)]
    );
    if let Some(rate) = agreement.rate() {
        println!(
            "  book reconstruction agrees with the exchange on {:.1}% of {} updates",
            rate * 100.0,
            agreement.checked
        );
    }

    let (trades, _) = flow::trades(&records, &quotes);
    let threshold = match args.surge_size {
        Some(size) => size,
        None => {
            let mut sizes: Vec<f64> = trades
                .iter()
                .filter(|trade| trade.asset == asset)
                .map(|trade| trade.size)
                .collect();
            median(&mut sizes)
                .map(|value| value * 5.0)
                .filter(|value| *value > 0.0)
                .unwrap_or(1.0)
        }
    };
    let surges = flow::surges(
        &trades,
        &asset,
        args.surge_secs.saturating_mul(1_000_000_000),
        threshold,
    );
    let signals: Vec<Signal> = surges
        .iter()
        .map(|surge| Signal {
            at_unix_ns: surge.at_unix_ns,
            rose: surge.signed_size > 0.0,
        })
        .collect();

    println!("\n== Signals ==");
    println!(
        "  {} surge(s) of {threshold:.0} net within {}s",
        signals.len(),
        args.surge_secs
    );

    println!("\n== Rules ==");
    println!(
        "  size {:.0}, max position {:.0}, min edge {:.3} on top of cost",
        limits.size, limits.max_position, limits.min_edge
    );
    println!(
        "  refuse a spread over {:.3}, {} depth at the touch, {:.1} bps per leg",
        limits.max_spread,
        if limits.require_touch_depth {
            "require"
        } else {
            "ignore"
        },
        config.execution.fee_bps
    );
    println!(
        "  react {} ms, hold {} s, signals stale after {} ms",
        args.react_ms, args.hold_secs, config.decision.max_signal_age_ms
    );

    if signals.is_empty() {
        println!("\nNo signal to act on, so the rules were never exercised.");
        return Ok(());
    }

    let react_ns = args.react_ms.saturating_mul(1_000_000);
    let hold_ns = args.hold_secs.saturating_mul(1_000_000_000);
    let touch = |at: u64| touch_at(&quotes, &asset, at);
    let results = paper::sweep(&signals, touch, &LADDER, react_ns, hold_ns, &limits);

    println!("\n== What a model would have to be right about ==");
    println!(
        "  {:>10} {:>7} {:>11} {:>7} {:>11}",
        "move", "trades", "net", "won", "drawdown"
    );
    for (expected, result) in &results {
        let trades = result.trades.len();
        if trades == 0 {
            println!("  {expected:>10.3} {:>7} {:>11}", 0, "-");
            continue;
        }
        println!(
            "  {expected:>10.3} {trades:>7} {:>+11.3} {:>6.0}% {:>11.3}",
            result.net(),
            100.0 * result.wins() as f64 / trades as f64,
            result.max_drawdown(),
        );
    }

    match results
        .iter()
        .find(|(_, result)| !result.trades.is_empty() && result.net() > 0.0)
    {
        Some((expected, _)) => println!(
            "\n  Break-even sits between {expected:.3} and the row above it: a model\n  \
             predicting less than that, on average, loses money under these rules."
        ),
        None => println!(
            "\n  No move on the ladder turned a profit. On this recording the rules\n  \
             lose at every assumption tried, which is a result."
        ),
    }

    // Refusals are taken from the smallest move that traded at all: that is
    // where the rules bite hardest and the binding one is most visible.
    if let Some((_, result)) = results.iter().find(|(_, each)| !each.refusals.is_empty()) {
        println!("\n  Signals turned away:");
        for (reason, count) in &result.refusals {
            println!("    {count:>5}  {reason}");
        }
        if result.abandoned > 0 {
            println!("    {:>5}  no book to close against", result.abandoned);
        }
    }

    println!("\n  Every fill is at the touch, so this is an upper bound.");
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

    match run(&args).context("paper run failed") {
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
    fn a_market_recording_is_required() {
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--react-ms", "0"])).is_err());
    }

    #[test]
    fn defaults_are_the_cautious_ones() {
        let parsed = parse_args(args(&["--market", "m.jsonl"])).unwrap().unwrap();
        assert_eq!(parsed.config, PathBuf::from("config.toml"));
        assert_eq!(parsed.react_ms, 250);
        assert_eq!(parsed.hold_secs, 30);
        assert_eq!(parsed.surge_secs, 5);
        assert!(parsed.surge_size.is_none());
    }

    #[test]
    fn zero_durations_are_refused() {
        assert!(parse_args(args(&["--market", "m", "--hold-secs", "0"])).is_err());
        assert!(parse_args(args(&["--market", "m", "--surge-secs", "0"])).is_err());
        assert!(parse_args(args(&["--market", "m", "--surge-size", "0"])).is_err());
    }

    #[test]
    fn reacting_instantly_is_allowed_even_though_it_is_not_realistic() {
        let parsed = parse_args(args(&["--market", "m", "--react-ms", "0"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.react_ms, 0);
    }

    #[test]
    fn the_touch_is_the_last_one_at_or_before_the_instant() {
        let quotes = vec![
            Quote {
                at_unix_ns: 100,
                asset: "A".into(),
                bid: 0.40,
                ask: 0.42,
                bid_size: 5.0,
                ask_size: 7.0,
                mid: 0.41,
            },
            Quote {
                at_unix_ns: 300,
                asset: "A".into(),
                bid: 0.50,
                ask: 0.52,
                bid_size: 1.0,
                ask_size: 2.0,
                mid: 0.51,
            },
        ];
        let found = touch_at(&quotes, "A", 200).unwrap();
        assert_eq!(found.bid, 0.40);
        assert_eq!(found.ask_size, 7.0);
        assert!(touch_at(&quotes, "A", 50).is_none());
        assert!(touch_at(&quotes, "B", 400).is_none());
    }
}
