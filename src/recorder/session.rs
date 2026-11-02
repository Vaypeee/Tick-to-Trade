//! A recording session: the output file, the queue, the writer task and the
//! counters that describe how well the recorder itself behaved.
//!
//! Both feeds record through this, so a Polymarket recording and a sports
//! recording share a format, a clock convention and a failure mode. That is
//! what makes them comparable afterwards, which is the entire point of
//! recording them at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::mpsc::{self, Sender, error::TrySendError};
use tokio::task::JoinHandle;

use super::capture::{Capture, Payload, Stamp};
use super::metrics::Timings;
use super::writer;
use super::{Stats, Summary};

/// Names a recording after the feed and the second it started, so a directory
/// of them sorts chronologically and no run can overwrite another.
pub fn recording_path(dir: &Path, name: &str, started_unix_secs: u64) -> PathBuf {
    dir.join(format!("{name}-{started_unix_secs}.jsonl"))
}

pub fn metadata_path(recording: &Path) -> PathBuf {
    recording.with_extension("meta.json")
}

pub struct Session {
    path: PathBuf,
    origin: Instant,
    seq: u64,
    tx: Sender<Capture>,
    stats: Arc<Stats>,
    writer: JoinHandle<Result<Timings>>,
    producer: Option<Timings>,
}

/// Everything a finished recording has to say about itself.
pub struct Outcome {
    pub summary: Summary,
    pub timings: Timings,
}

impl Session {
    /// Opens a recording and starts its writer task.
    ///
    /// `metadata` describes the feed; the clock convention and the recorder
    /// version are added here so every sidecar explains itself without the
    /// reader having to find this source file.
    pub async fn open(
        name: &str,
        dir: &Path,
        queue_capacity: usize,
        metadata: Value,
    ) -> Result<Self> {
        let origin = Instant::now();
        let started_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);

        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("cannot create `{}`", dir.display()))?;

        let path = recording_path(dir, name, started_unix_secs);
        write_metadata(&metadata_path(&path), name, started_unix_secs, metadata).await?;

        let stats = Arc::new(Stats::default());
        let (tx, rx) = mpsc::channel(queue_capacity);
        let writer = {
            let path = path.clone();
            let stats = Arc::clone(&stats);
            tokio::spawn(async move { writer::run(&path, rx, stats, origin).await })
        };

        Ok(Self {
            path,
            origin,
            seq: 0,
            tx,
            stats,
            writer,
            producer: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The monotonic origin of this recording, for stamping on another task.
    pub fn origin(&self) -> Instant {
        self.origin
    }

    /// Hands over the histograms the producing task kept.
    ///
    /// Each task owns its own and they are merged once, here, so nothing on
    /// the hot path ever waits on a lock to record a measurement.
    pub fn submit_timings(&mut self, timings: Timings) {
        self.producer = Some(timings);
    }

    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Reads the clocks. Call it the moment data arrives, not later.
    pub fn stamp(&self) -> Stamp {
        Stamp::now(self.origin)
    }

    /// Records something the recorder itself produced, stamped now.
    pub async fn emit(&mut self, payload: Payload) -> Result<()> {
        let stamp = self.stamp();
        self.emit_at(stamp, payload).await
    }

    /// Records data using a stamp taken earlier, at the moment it arrived.
    pub async fn emit_at(&mut self, stamp: Stamp, payload: Payload) -> Result<()> {
        self.seq += 1;
        self.dispatch(Capture::new(self.seq, stamp, payload)).await
    }

    /// Hands a record to the writer.
    ///
    /// A full queue is never resolved by dropping the record: a hole in a
    /// recording is worse than a delay in it. The wait is counted instead, so
    /// the analysis can tell which recordings had the recorder in the way.
    async fn dispatch(&self, capture: Capture) -> Result<()> {
        match self.tx.try_send(capture) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(capture)) => {
                self.stats.on_stall();
                self.tx
                    .send(capture)
                    .await
                    .map_err(|_| anyhow!("the writer stopped while the queue was full"))
            }
            Err(TrySendError::Closed(_)) => bail!("the writer stopped"),
        }
    }

    /// Drains and flushes, then reports.
    ///
    /// The writer is joined even when the feed failed: whatever was captured
    /// before the failure is still worth keeping, and `outcome` is only
    /// propagated once the file is durable.
    pub async fn finish(self, outcome: Result<()>) -> Result<Outcome> {
        let Self {
            tx,
            writer,
            stats,
            producer,
            ..
        } = self;
        drop(tx);
        let mut timings = writer
            .await
            .context("writer task panicked")?
            .context("writer task failed")?;
        if let Some(producer) = producer {
            timings.merge(producer);
        }
        outcome?;
        Ok(Outcome {
            summary: stats.snapshot(),
            timings,
        })
    }
}

async fn write_metadata(
    path: &Path,
    name: &str,
    started_unix_secs: u64,
    feed: Value,
) -> Result<()> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("feed".into(), Value::String(name.to_owned()));
    metadata.insert(
        "recorder_version".into(),
        Value::String(env!("CARGO_PKG_VERSION").to_owned()),
    );
    metadata.insert("started_unix_secs".into(), started_unix_secs.into());
    metadata.insert(
        "clocks".into(),
        serde_json::json!({
            "t_mono_ns": "nanoseconds since the recorder started; use this for durations",
            "t_unix_ns": "wall clock at receipt; the only clock comparable across recordings, and it can step under NTP",
        }),
    );
    if let Value::Object(feed) = feed {
        metadata.extend(feed);
    }

    let encoded = serde_json::to_string_pretty(&Value::Object(metadata))? + "\n";
    tokio::fs::write(path, encoded)
        .await
        .with_context(|| format!("cannot write `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recordings_are_named_after_their_feed_and_start_second() {
        let path = recording_path(Path::new("data/recordings"), "sports", 1_700_000_000);
        assert!(path.ends_with("sports-1700000000.jsonl"));
    }

    #[test]
    fn metadata_sits_beside_its_recording() {
        let recording = recording_path(Path::new("data"), "polymarket", 1);
        assert!(metadata_path(&recording).ends_with("polymarket-1.meta.json"));
    }
}
