//! Captures raw feeds to disk, with a timestamp on every frame.
//!
//! This is deliberately the first thing the project builds. The hypothesis it
//! exists to test — that the market reprices slowly enough to be beaten — is
//! answerable from recordings alone, and answering it that way costs weeks
//! rather than months. Recordings also give every later benchmark something
//! reproducible to run against, which a live feed never can.
//!
//! Two feeds are recorded, in the same format and against the same clocks:
//! [`polymarket`] for the market, [`sports`] for the real-world events the
//! market is supposed to be slow to price. Neither is useful alone. The
//! measurement is the offset between them.

pub mod capture;
pub mod labels;
pub mod polymarket;
pub mod session;
pub mod sports;
pub mod writer;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use session::Session;

/// Counters worth reporting at the end of a run.
///
/// `stalls` is the one to watch: it counts the times the writer could not keep
/// up and the producer had to wait, which delays every timestamp taken after
/// it. A recording with stalls is still usable, but its latency figures then
/// describe the recorder as much as the feed.
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

/// Which feed to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Polymarket,
    Sports,
    /// Key presses from a spectator watching the public broadcast.
    Labels,
}

impl Feed {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "polymarket" => Ok(Self::Polymarket),
            "sports" => Ok(Self::Sports),
            "labels" => Ok(Self::Labels),
            other => Err(format!(
                "unknown feed `{other}`, expected `polymarket`, `sports` or `labels`"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Polymarket => "polymarket",
            Self::Sports => "sports",
            Self::Labels => "labels",
        }
    }
}

/// Records `feed` until `shutdown` flips to `true`, then drains and flushes.
pub async fn run(feed: Feed, config: &Config, shutdown: watch::Receiver<bool>) -> Result<Summary> {
    crate::tls::install_crypto_provider();

    let metadata = match feed {
        Feed::Polymarket => polymarket::metadata(config),
        Feed::Sports => sports::metadata(config)?,
        Feed::Labels => labels::metadata(config)?,
    };
    let mut session = Session::open(
        feed.name(),
        &config.recording.output_dir,
        config.recording.queue_capacity,
        metadata,
    )
    .await?;
    tracing::info!(feed = feed.name(), path = %session.path().display(), "recording started");

    let reporter = spawn_reporter(
        session.stats(),
        Duration::from_secs(config.recording.stats_interval_secs.max(1)),
        shutdown.clone(),
    );

    let outcome = match feed {
        Feed::Polymarket => polymarket::run(config, &mut session, shutdown).await,
        Feed::Sports => sports::run(config, &mut session, shutdown).await,
        Feed::Labels => labels::run(config, &mut session, shutdown).await,
    };
    reporter.abort();

    session.finish(outcome).await
}

fn spawn_reporter(
    stats: Arc<Stats>,
    every: Duration,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(report_periodically(stats, every, shutdown))
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
    fn feeds_round_trip_through_their_names() {
        for feed in [Feed::Polymarket, Feed::Sports, Feed::Labels] {
            assert_eq!(Feed::parse(feed.name()), Ok(feed));
        }
    }

    #[test]
    fn unknown_feeds_are_rejected() {
        assert!(Feed::parse("tennis").is_err());
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
