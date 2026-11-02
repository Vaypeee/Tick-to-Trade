//! The disk side of the recorder.
//!
//! Kept on its own task so that a slow write never delays the next read off
//! the socket. Everything arriving here has already been timestamped.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc::Receiver;

use super::Stats;
use super::capture::Capture;

/// Records pulled from the queue per wake-up.
const BATCH: usize = 256;
const BUFFER_BYTES: usize = 1 << 20;

pub async fn run(path: &Path, mut rx: Receiver<Capture>, stats: Arc<Stats>) -> Result<()> {
    let file = File::create(path)
        .await
        .with_context(|| format!("cannot create `{}`", path.display()))?;
    let mut out = BufWriter::with_capacity(BUFFER_BYTES, file);
    let mut batch = Vec::with_capacity(BATCH);
    let mut line = String::with_capacity(4096);

    loop {
        let received = rx.recv_many(&mut batch, BATCH).await;
        if received == 0 {
            break;
        }
        for capture in batch.drain(..) {
            line.clear();
            capture.write_line(&mut line);
            out.write_all(line.as_bytes())
                .await
                .with_context(|| format!("cannot write to `{}`", path.display()))?;
            stats.on_written(line.len() as u64);
        }
        // A short batch means the queue is drained, so the feed is quiet and a
        // flush costs nothing. Under load the buffer is left to fill instead.
        if received < BATCH {
            out.flush().await?;
        }
    }

    out.flush().await?;
    out.into_inner()
        .sync_all()
        .await
        .with_context(|| format!("cannot flush `{}` to disk", path.display()))?;
    Ok(())
}
