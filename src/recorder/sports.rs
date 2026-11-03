//! Polling recorder for a live sports feed.
//!
//! No provider is hard-coded. Vendors publish latency figures that are not
//! comparable with each other and that none of them measures the same way, so
//! the only number worth having is one measured here, against the same clock
//! as the market recording. Point `sports_feed.url` at whatever source is
//! being evaluated — a free scoreboard endpoint or a paid feed — and compare.
//!
//! Polling is the honest default because most sports data is still delivered
//! over HTTP. It also makes the limitation visible: a poll cannot tell you
//! when something happened, only that it had happened by the time you asked.
//! Every recorded change therefore carries both ends of that window.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::capture::Payload;
use super::session::Session;
use crate::config::{Config, SportsFeed};

/// Describes the recording for its metadata sidecar.
///
/// Header *names* are recorded, never their values: a paid feed authenticates
/// with a header, and a metadata file sitting next to a recording is exactly
/// the kind of file that gets shared.
pub fn metadata(config: &Config) -> Result<Value> {
    let feed = feed(config)?;
    Ok(serde_json::json!({
        "url": feed.url,
        "poll_interval_ms": feed.poll_interval_ms,
        "timeout_ms": feed.timeout_ms,
        "header_names": feed.headers.keys().collect::<Vec<_>>(),
        "detection_note": "a change is bounded by t_request_* of the recording poll and t_* of its response; unchanged_polls widens that bound",
    }))
}

fn feed(config: &Config) -> Result<&SportsFeed> {
    config
        .sports_feed
        .as_ref()
        .context("`[sports_feed]` is missing from the configuration")
}

/// Polls until `shutdown` flips to `true`.
///
/// Only responses that differ from the previous one are written. Recording
/// every poll would multiply an unchanged scoreboard by the poll rate for no
/// extra information, and the information that matters — when the feed first
/// showed the new state — is preserved exactly.
pub async fn run(
    config: &Config,
    session: &mut Session,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let feed = feed(config)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(feed.timeout_ms))
        .user_agent(concat!("tick-to-trade/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot build the HTTP client")?;

    let mut ticker = tokio::time::interval(Duration::from_millis(feed.poll_interval_ms));
    // A slow response must not be repaid with a burst of catch-up requests:
    // that would hammer the provider and bunch up the timestamps.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    session
        .emit(Payload::control_with("polling", feed.url.clone()))
        .await?;
    tracing::info!(url = %feed.url, interval_ms = feed.poll_interval_ms, "polling");

    let mut previous: Option<(u16, String)> = None;
    let mut unchanged: u64 = 0;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }

        let requested = session.stamp();
        let mut request = client.get(&feed.url);
        for (name, value) in &feed.headers {
            request = request.header(name, value);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                session
                    .emit(Payload::control_with("poll_failed", format!("{error}")))
                    .await?;
                continue;
            }
        };
        let status = response.status().as_u16();
        let (received, body) = match response.text().await {
            // Stamped the moment the body is in hand, before it is compared to
            // anything, so no recorder work lands inside the measurement.
            Ok(body) => (session.stamp(), body),
            Err(error) => {
                session
                    .emit(Payload::control_with("poll_failed", format!("{error}")))
                    .await?;
                continue;
            }
        };

        if is_new(previous.as_ref(), status, &body) {
            session
                .emit_at(
                    received,
                    Payload::poll(status, requested, unchanged, body.clone()),
                )
                .await?;
            previous = Some((status, body));
            unchanged = 0;
        } else {
            unchanged += 1;
            // Proof that the poller was alive through a quiet stretch, so a
            // silent recording is never mistaken for a dead one.
            if should_heartbeat(unchanged, feed.heartbeat_polls) {
                session
                    .emit_at(
                        received,
                        Payload::control_with("unchanged", unchanged.to_string()),
                    )
                    .await?;
            }
        }
    }

    session.emit(Payload::control("stopped")).await?;
    Ok(())
}

/// Whether a response differs from the last one recorded.
///
/// The status is part of the comparison: a feed that starts returning 429 has
/// changed in a way the analysis needs to see, even if the body repeats.
fn is_new(previous: Option<&(u16, String)>, status: u16, body: &str) -> bool {
    match previous {
        None => true,
        Some((previous_status, previous_body)) => {
            *previous_status != status || previous_body != body
        }
    }
}

fn should_heartbeat(unchanged: u64, every: u64) -> bool {
    every > 0 && unchanged.is_multiple_of(every)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_response_is_always_new() {
        assert!(is_new(None, 200, "{}"));
    }

    #[test]
    fn identical_responses_are_not_new() {
        let previous = (200, String::from(r#"{"score":0}"#));
        assert!(!is_new(Some(&previous), 200, r#"{"score":0}"#));
    }

    #[test]
    fn a_changed_body_or_status_is_new() {
        let previous = (200, String::from(r#"{"score":0}"#));
        assert!(is_new(Some(&previous), 200, r#"{"score":1}"#));
        assert!(is_new(Some(&previous), 429, r#"{"score":0}"#));
    }

    #[test]
    fn heartbeats_fire_on_the_configured_multiple_only() {
        assert!(should_heartbeat(30, 30));
        assert!(should_heartbeat(60, 30));
        assert!(!should_heartbeat(29, 30));
        assert!(!should_heartbeat(31, 30));
    }

    #[test]
    fn heartbeats_can_be_turned_off() {
        assert!(!should_heartbeat(100, 0));
    }
}
