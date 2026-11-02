//! Records the Polymarket CLOB market channel to a timestamped JSON Lines file.
//!
//! Run it alongside a recording of a live sports feed, then compare the two
//! offline: that comparison is what tells you whether the repricing window the
//! project assumes actually exists, and how long it lasts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use tick_to_trade::{config::Config, recorder};
use tokio::sync::watch;

const DEFAULT_CONFIG: &str = "config.toml";
const USAGE: &str = "\
usage: record [--config <path>] [--duration-secs <n>]

    --config <path>        configuration file (default: config.toml)
    --duration-secs <n>    stop after n seconds (default: run until Ctrl-C)
    -h, --help             print this message

Records the Polymarket CLOB market channel. The channel is public, so no
credentials are needed; `polymarket.asset_ids` decides what is recorded.
";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    config: PathBuf,
    duration: Option<Duration>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
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

async fn record(config_path: &Path, duration: Option<Duration>) -> Result<()> {
    let config = Config::load(config_path)?;
    let (tx, rx) = watch::channel(false);
    tokio::spawn(watch_for_stop(tx, duration));

    let summary = recorder::run(&config, rx)
        .await
        .context("recording failed")?;

    tracing::info!(
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
    if summary.lines == 0 {
        tracing::warn!("no frames were recorded — check `polymarket.asset_ids`");
    }
    Ok(())
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

    match record(&args.config, args.duration).await {
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
    fn defaults_to_config_toml_and_no_time_limit() {
        let parsed = parse_args(args(&[])).unwrap().unwrap();
        assert_eq!(parsed, Args::default());
        assert_eq!(parsed.config, PathBuf::from("config.toml"));
        assert!(parsed.duration.is_none());
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
