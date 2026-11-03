//! Records one feed to a timestamped JSON Lines file.
//!
//! Run one process per feed, at the same time, on the same machine. Comparing
//! the two recordings afterwards is what tells you whether the repricing
//! window this project assumes actually exists, and how long it lasts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tick_to_trade::{
    config::Config,
    recorder::{
        self, Feed,
        metrics::{Timings, humanise},
    },
};
use tokio::sync::watch;

const DEFAULT_CONFIG: &str = "config.toml";
const USAGE: &str = "\
usage: record [--feed <polymarket|sports|labels>] [--config <path>]
              [--duration-secs <n>]

    --feed <name>          polymarket (default), sports or labels
    --source <name>        which sports feed, when several are configured
    --config <path>        configuration file (default: config.toml)
    --duration-secs <n>    stop after n seconds (default: run until Ctrl-C)
    -h, --help             print this message

The Polymarket market channel is public, so no credentials are needed;
`polymarket.asset_ids` decides what is recorded. The sports feed is whatever
`sports_feed.url` points at, polled on `sports_feed.poll_interval_ms`. The
labels feed serves a local page: press its keys as you watch the broadcast,
and each press records when an ordinary spectator could have known.

Record together, one process each, so the recordings can be aligned:

    record --feed polymarket &
    record --feed labels &
";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    feed: Feed,
    source: Option<String>,
    config: PathBuf,
    duration: Option<Duration>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            feed: Feed::Polymarket,
            source: None,
            config: PathBuf::from(DEFAULT_CONFIG),
            duration: None,
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--feed" => {
                let value = args.next().ok_or("`--feed` expects a name")?;
                parsed.feed = Feed::parse(&value)?;
            }
            "--source" => {
                parsed.source = Some(args.next().ok_or("`--source` expects a name")?);
            }
            "--config" => {
                let value = args.next().ok_or("`--config` expects a path")?;
                parsed.config = PathBuf::from(value);
            }
            "--duration-secs" => {
                let value = args.next().ok_or("`--duration-secs` expects a value")?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("`--duration-secs` expects a number, got `{value}`"))?;
                if secs == 0 {
                    return Err("`--duration-secs` must be greater than zero".into());
                }
                parsed.duration = Some(Duration::from_secs(secs));
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    Ok(Some(parsed))
}

/// Flips the shutdown flag on Ctrl-C, or when the requested duration elapses.
///
/// Stopping is a normal part of a recording rather than a failure, so it goes
/// through the same path either way: the writer drains and flushes before the
/// process exits.
async fn watch_for_stop(tx: watch::Sender<bool>, duration: Option<Duration>) {
    match duration {
        Some(duration) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => tracing::info!("duration reached, stopping"),
                result = tokio::signal::ctrl_c() => {
                    if result.is_ok() {
                        tracing::info!("interrupted, stopping");
                    }
                }
            }
        }
        None => {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("interrupted, stopping");
            }
        }
    }
    let _ = tx.send(true);
}

async fn record(
    feed: Feed,
    source: Option<&str>,
    config_path: &Path,
    duration: Option<Duration>,
) -> Result<()> {
    let config = Config::load(config_path)?;
    let (tx, rx) = watch::channel(false);
    tokio::spawn(watch_for_stop(tx, duration));

    let outcome = recorder::run(feed, source, &config, rx)
        .await
        .context("recording failed")?;
    let summary = outcome.summary;

    tracing::info!(
        feed = feed.name(),
        lines = summary.lines,
        bytes = summary.bytes,
        stalls = summary.stalls,
        reconnects = summary.reconnects,
        "recording finished"
    );
    if summary.stalls > 0 {
        tracing::warn!(
            stalls = summary.stalls,
            "the writer fell behind; timestamps after a stall include recorder delay"
        );
    }
    report_latency(&outcome.timings);
    if !outcome.durable {
        // Everything above is true and none of it survived. Saying "finished"
        // and stopping there would leave the operator believing in a file
        // that is not on disk.
        tracing::error!(
            path = %outcome.path.display(),
            "the recording is not on disk: it was removed or emptied while it was being written, and everything reported above went nowhere"
        );
        bail!("the recording did not survive the run");
    }
    if summary.lines == 0 {
        let hint = match feed {
            Feed::Polymarket => "check `polymarket.asset_ids`",
            Feed::Sports => "check the feed url",
            Feed::Labels => "nothing was pressed on the label page",
        };
        tracing::warn!("nothing was recorded — {hint}");
    }
    Ok(())
}

/// Prints what the recorder cost itself.
///
/// This is in-process latency only: from a frame leaving the socket to the
/// line reaching the operating system. The network before it and the disk
/// after it are not measured here and are not claimed.
fn report_latency(timings: &Timings) {
    let report = timings.report();
    if report.is_empty() {
        return;
    }

    println!("\nin-process latency, socket to write");
    println!(
        "  {:<12} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "stage", "p50", "p90", "p99", "max", "n"
    );
    for line in &report {
        println!(
            "  {:<12} {:>9} {:>9} {:>9} {:>9} {:>8}",
            line.stage.label(),
            humanise(line.p50_ns),
            humanise(line.p90_ns),
            humanise(line.p99_ns),
            humanise(line.max_ns),
            line.count,
        );
    }
    println!("  * feed offset is the gap between the exchange timestamp and our");
    println!("    receipt: network delay plus clock disagreement, in milliseconds of");
    println!("    resolution. It is not a latency and cannot be split into one.");
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

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

    match record(
        args.feed,
        args.source.as_deref(),
        &args.config,
        args.duration,
    )
    .await
    {
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
    fn defaults_to_the_market_feed_and_no_time_limit() {
        let parsed = parse_args(args(&[])).unwrap().unwrap();
        assert_eq!(parsed, Args::default());
        assert_eq!(parsed.feed, Feed::Polymarket);
        assert_eq!(parsed.config, PathBuf::from("config.toml"));
        assert!(parsed.duration.is_none());
    }

    #[test]
    fn parses_the_feed_to_record() {
        let parsed = parse_args(args(&["--feed", "sports"])).unwrap().unwrap();
        assert_eq!(parsed.feed, Feed::Sports);
        assert!(parse_args(args(&["--feed", "cricket"])).is_err());
        assert!(parse_args(args(&["--feed"])).is_err());
    }

    #[test]
    fn parses_a_config_path_and_a_duration() {
        let parsed = parse_args(args(&["--config", "other.toml", "--duration-secs", "90"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.config, PathBuf::from("other.toml"));
        assert_eq!(parsed.duration, Some(Duration::from_secs(90)));
    }

    #[test]
    fn help_requests_no_recording() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
    }

    #[test]
    fn rejects_bad_durations_and_unknown_flags() {
        assert!(parse_args(args(&["--duration-secs", "soon"])).is_err());
        assert!(parse_args(args(&["--duration-secs", "0"])).is_err());
        assert!(parse_args(args(&["--duration-secs"])).is_err());
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--quiet"])).is_err());
    }
}
