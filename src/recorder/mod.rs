//! Captures a raw market data feed to disk, with a timestamp on every frame.
//!
//! This is deliberately the first thing the project builds. The hypothesis it
//! exists to test — that the market reprices slowly enough to be beaten — is
//! answerable from recordings alone, and answering it that way costs weeks
//! rather than months. Recordings also give every later benchmark something
//! reproducible to run against, which a live feed never can.

pub mod capture;
pub mod polymarket;
pub mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

use crate::config::Config;

/// Counters worth reporting at the end of a run.
///
/// `stalls` is the one to watch: it counts the times the writer could not keep
/// up and the read loop had to wait, which delays every timestamp taken after
/// it. A recording with stalls is still usable, but its latency figures then
/// describe the recorder as much as the market.
#[derive(Debug, Default)]
pub struct Stats {
    lines: AtomicU64,
    bytes: AtomicU64,
    stalls: AtomicU64,
    reconnects: AtomicU64,
}

impl Stats {
    pub fn on_written(&self, bytes: u64) {
        self.lines.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn on_stall(&self) {
        self.stalls.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Summary {
        Summary {
            lines: self.lines.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            stalls: self.stalls.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub lines: u64,
    pub bytes: u64,
    pub stalls: u64,
    pub reconnects: u64,
}

/// Names a recording after the second it started, so a directory of them sorts
/// chronologically and no run can overwrite another.
pub fn recording_path(dir: &Path, started_unix_secs: u64) -> PathBuf {
    dir.join(format!("polymarket-{started_unix_secs}.jsonl"))
}

pub fn metadata_path(recording: &Path) -> PathBuf {
    recording.with_extension("meta.json")
}

/// Records until `shutdown` flips to `true`, then drains and flushes.
pub async fn run(config: &Config, shutdown: watch::Receiver<bool>) -> Result<Summary> {
    crate::tls::install_crypto_provider();

    let origin = Instant::now();
    let started_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);

    let dir = &config.recording.output_dir;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create `{}`", dir.display()))?;

    let path = recording_path(dir, started_unix_secs);
    write_metadata(&metadata_path(&path), config, started_unix_secs).await?;

    let stats = Arc::new(Stats::default());
    let (tx, rx) = mpsc::channel(config.recording.queue_capacity);

    let writer = {
        let path = path.clone();
        let stats = Arc::clone(&stats);
        tokio::spawn(async move { writer::run(&path, rx, stats).await })
    };

    tracing::info!(path = %path.display(), "recording started");
    let reporter = tokio::spawn(report_periodically(
        Arc::clone(&stats),
        Duration::from_secs(config.recording.stats_interval_secs.max(1)),
        shutdown.clone(),
    ));

    let outcome = polymarket::run(config, tx, origin, shutdown, Arc::clone(&stats)).await;
    reporter.abort();

    // Dropping the sender inside `polymarket::run` ends the loop of the writer
    // once the queue is empty. Joining it is what makes the last frames
    // durable, so it happens even when the feed itself failed.
    writer
        .await
        .context("writer task panicked")?
        .context("writer task failed")?;
    outcome?;

    Ok(stats.snapshot())
}

async fn write_metadata(path: &Path, config: &Config, started_unix_secs: u64) -> Result<()> {
    let metadata = serde_json::json!({
        "recorder_version": env!("CARGO_PKG_VERSION"),
        "started_unix_secs": started_unix_secs,
        "ws_url": config.polymarket.ws_url,
        "asset_ids": config.polymarket.asset_ids,
        "ping_interval_secs": config.polymarket.ping_interval_secs,
        "clocks": {
            "t_mono_ns": "nanoseconds since the recorder started; use this for durations",
            "t_unix_ns": "wall clock at receipt; the only clock comparable across processes, and it can step under NTP",
        },
    });
    let encoded = serde_json::to_string_pretty(&metadata)? + "\n";
    tokio::fs::write(path, encoded)
        .await
        .with_context(|| format!("cannot write `{}`", path.display()))
}

async fn report_periodically(
    stats: Arc<Stats>,
    every: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(every);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let Summary { lines, bytes, stalls, reconnects } = stats.snapshot();
                tracing::info!(lines, bytes, stalls, reconnects, "recording");
            }
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recordings_are_named_after_their_start_second() {
        let path = recording_path(Path::new("data/recordings"), 1_700_000_000);
        assert!(path.ends_with("polymarket-1700000000.jsonl"));
    }

    #[test]
    fn metadata_sits_beside_its_recording() {
        let recording = recording_path(Path::new("data"), 1);
        assert!(metadata_path(&recording).ends_with("polymarket-1.meta.json"));
    }

    #[test]
    fn counters_accumulate_independently() {
        let stats = Stats::default();
        stats.on_written(10);
        stats.on_written(20);
        stats.on_stall();
        stats.on_reconnect();
        stats.on_reconnect();
        assert_eq!(
            stats.snapshot(),
            Summary {
                lines: 2,
                bytes: 30,
                stalls: 1,
                reconnects: 2
            }
        );
    }
}
