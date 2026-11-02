//! Websocket client for the Polymarket CLOB market channel.
//!
//! The market channel is public: it carries order book snapshots and
//! incremental price changes, and needs no credentials. Only the user channel,
//! which this recorder does not touch, is authenticated.
//!
//! Nothing here interprets the feed. Frames are stored exactly as they arrive,
//! because which fields matter is one of the things the project does not know
//! yet, and a raw recording can be re-analysed while a parsed one cannot.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::capture::Payload;
use super::session::Session;
use crate::config::Config;

/// Shortest and longest wait between reconnection attempts.
const MIN_DELAY: Duration = Duration::from_millis(250);
const MAX_DELAY: Duration = Duration::from_secs(30);

/// A connection that survived this long is treated as healthy, so the next
/// failure starts backing off from scratch. Without it, a long recording that
/// drops once an hour would slowly climb to the maximum delay and stay there.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Exponential backoff between reconnection attempts.
#[derive(Debug)]
pub struct Backoff {
    current: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self { current: MIN_DELAY }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(MAX_DELAY);
        delay
    }

    pub fn reset(&mut self) {
        self.current = MIN_DELAY;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Describes the recording for its metadata sidecar.
pub fn metadata(config: &Config) -> Value {
    serde_json::json!({
        "ws_url": config.polymarket.ws_url,
        "asset_ids": config.polymarket.asset_ids,
        "ping_interval_secs": config.polymarket.ping_interval_secs,
    })
}

/// Why a connection ended.
enum Disconnect {
    /// The recorder was asked to stop.
    Shutdown,
    /// The peer or the network ended it; the recorder should reconnect.
    Remote(String),
}

/// Connects, records, and reconnects until `shutdown` flips to `true`.
///
/// Returns `Ok` on a clean stop. Failing to reach the feed is not an error
/// here: the loop keeps retrying, because a recording left running overnight
/// should survive the network going away for a while.
pub async fn run(
    config: &Config,
    session: &mut Session,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut backoff = Backoff::new();
    let stats = session.stats();

    while !*shutdown.borrow() {
        let started = Instant::now();
        let outcome = stream_once(config, session, &mut shutdown).await;
        let connected_for = started.elapsed();

        let reason = match outcome {
            Ok(Disconnect::Shutdown) => break,
            Ok(Disconnect::Remote(reason)) => reason,
            // A failed handshake or a dropped socket is expected operationally,
            // so it is recorded and retried rather than propagated.
            Err(error) => format!("{error:#}"),
        };
        session
            .emit(Payload::control_with("disconnected", reason))
            .await?;

        if connected_for >= STABLE_AFTER {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        tracing::warn!(?delay, "feed lost, reconnecting");

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => {}
        }
        if *shutdown.borrow() {
            break;
        }
        stats.on_reconnect();
    }

    session.emit(Payload::control("stopped")).await?;
    Ok(())
}

/// One connection, from handshake to disconnect.
async fn stream_once(
    config: &Config,
    session: &mut Session,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Disconnect> {
    let url = &config.polymarket.ws_url;
    let (socket, _) = connect_async(url)
        .await
        .with_context(|| format!("websocket handshake failed for `{url}`"))?;
    let (mut sink, mut stream) = socket.split();

    let subscribe = serde_json::json!({
        "assets_ids": config.polymarket.asset_ids,
        "type": "market",
    })
    .to_string();
    sink.send(Message::Text(subscribe.into()))
        .await
        .context("cannot send the subscribe frame")?;

    session.emit(Payload::control("connected")).await?;
    session
        .emit(Payload::control_with(
            "subscribed",
            format!("{} asset ids", config.polymarket.asset_ids.len()),
        ))
        .await?;
    tracing::info!(assets = config.polymarket.asset_ids.len(), "subscribed");

    let mut keepalive =
        tokio::time::interval(Duration::from_secs(config.polymarket.ping_interval_secs));
    keepalive.tick().await;

    loop {
        tokio::select! {
            frame = stream.next() => {
                // Taken before anything looks at the frame. Every line of
                // parsing added above this point is latency attributed to the
                // market that actually belongs to the recorder.
                let stamp = session.stamp();
                let Some(frame) = frame else {
                    return Ok(Disconnect::Remote("stream ended".into()));
                };
                match frame.context("websocket error")? {
                    Message::Text(text) => {
                        let payload = Payload::from_frame(text.as_str().to_owned());
                        session.emit_at(stamp, payload).await?;
                    }
                    Message::Binary(bytes) => {
                        let payload = Payload::from_frame(
                            String::from_utf8_lossy(&bytes).into_owned(),
                        );
                        session.emit_at(stamp, payload).await?;
                    }
                    Message::Close(frame) => {
                        let reason = frame
                            .map(|frame| format!("closed by peer: {}", frame.reason))
                            .unwrap_or_else(|| "closed by peer".into());
                        return Ok(Disconnect::Remote(reason));
                    }
                    // Protocol-level ping and pong are answered by the
                    // transport and carry no market data.
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            _ = keepalive.tick() => {
                // The market channel expects a PING text frame, which is
                // separate from the websocket protocol ping above.
                sink.send(Message::Text("PING".into()))
                    .await
                    .context("cannot send the keepalive frame")?;
            }
            _ = shutdown.changed() => return Ok(Disconnect::Shutdown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let mut backoff = Backoff::new();
        let delays: Vec<Duration> = (0..10).map(|_| backoff.next_delay()).collect();
        assert_eq!(delays[0], MIN_DELAY);
        assert_eq!(delays[1], MIN_DELAY * 2);
        assert_eq!(delays[2], MIN_DELAY * 4);
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(*delays.last().unwrap(), MAX_DELAY);
    }

    #[test]
    fn reset_returns_to_the_shortest_delay() {
        let mut backoff = Backoff::new();
        for _ in 0..5 {
            backoff.next_delay();
        }
        backoff.reset();
        assert_eq!(backoff.next_delay(), MIN_DELAY);
    }
}
