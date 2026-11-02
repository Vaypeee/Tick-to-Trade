//! Records when an ordinary spectator could have known.
//!
//! A local page with large buttons and keyboard shortcuts. Watching the
//! public broadcast, you press a key the moment you see something happen, and
//! the press is written into a recording in the same format, against the same
//! clock, as the market.
//!
//! The press is not the instant the event occurred: the stream is seconds
//! behind and a human takes a few hundred milliseconds more. That is exactly
//! what makes it useful. It marks when public information reached somebody
//! with no advantage, so the distance between it and the moment the order
//! book moved *is* the advantage, measured in the only units that matter to
//! someone deciding whether to bother.
//!
//! The page is unauthenticated and its presses are written to disk, so it is
//! bound to loopback and the configuration refuses anything else.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use super::capture::{Payload, Stamp};
use super::session::Session;
use crate::config::{Button, Config, Labels};

const PAGE: &str = include_str!("../../assets/label-ui.html");

/// One press, on its way from the handler to the recording.
struct Press {
    id: String,
    direction: String,
    received: Stamp,
    clicked: Stamp,
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<Press>,
    buttons: Arc<Vec<Button>>,
    origin: Instant,
}

#[derive(Debug, Deserialize)]
struct PressBody {
    id: String,
    /// The browser clock at the press. Same machine clock as the server, so
    /// the two are directly comparable.
    clicked_unix_ms: i64,
}

#[derive(Debug, Serialize)]
struct PressReply {
    ok: bool,
    label: String,
}

fn labels(config: &Config) -> Result<&Labels> {
    config
        .labels
        .as_ref()
        .context("`[labels]` is missing from the configuration")
}

/// Describes the recording for its metadata sidecar.
pub fn metadata(config: &Config) -> Result<Value> {
    let labels = labels(config)?;
    Ok(serde_json::json!({
        "bind": labels.bind,
        "buttons": labels.buttons,
        "baseline_note": "t_click_unix_ns is when a spectator on the public broadcast pressed the key; it trails the real event by the stream delay plus human reaction, and that is the comparison it exists to provide",
    }))
}

/// Serves the page and records presses until `shutdown` flips to `true`.
pub async fn run(
    config: &Config,
    session: &mut Session,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let labels = labels(config)?;
    let (tx, mut rx) = mpsc::channel::<Press>(1024);
    let state = AppState {
        tx,
        buttons: Arc::new(labels.buttons.clone()),
        origin: session.origin(),
    };

    let app = Router::new()
        .route("/", get(page))
        .route("/buttons", get(buttons))
        .route("/label", post(press))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&labels.bind)
        .await
        .with_context(|| format!("cannot bind `{}`", labels.bind))?;
    let served = listener
        .local_addr()
        .map(|address| address.to_string())
        .unwrap_or_else(|_| labels.bind.clone());

    let closing = shutdown.clone();
    let server = tokio::spawn(async move {
        let mut closing = closing;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = closing.changed().await;
            })
            .await
    });

    session
        .emit(Payload::control_with("serving", served.clone()))
        .await?;
    tracing::info!(url = %format!("http://{served}/"), "open this and press keys as you watch");

    loop {
        tokio::select! {
            press = rx.recv() => {
                let Some(press) = press else { break };
                session
                    .emit_at(
                        press.received,
                        Payload::label(press.id, press.direction, press.clicked),
                    )
                    .await?;
            }
            _ = shutdown.changed() => break,
        }
    }

    // Drain whatever was pressed between the stop and the server winding down:
    // the last press of a session is usually the interesting one.
    while let Ok(press) = rx.try_recv() {
        session
            .emit_at(
                press.received,
                Payload::label(press.id, press.direction, press.clicked),
            )
            .await?;
    }

    let _ = server.await;
    session.emit(Payload::control("stopped")).await?;
    Ok(())
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn buttons(State(state): State<AppState>) -> Json<Vec<Button>> {
    Json(state.buttons.as_ref().clone())
}

async fn press(
    State(state): State<AppState>,
    Json(body): Json<PressBody>,
) -> Result<Json<PressReply>, StatusCode> {
    // Stamped before anything else, so queueing and lookup stay out of it.
    let received = Stamp::now(state.origin);

    // The direction comes from the configuration, never from the page. A
    // recording is evidence, and evidence should not be shaped by whatever
    // happened to POST at it.
    let button = state
        .buttons
        .iter()
        .find(|button| button.id == body.id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let clicked_unix_ns = body.clicked_unix_ms.max(0) as u64 * 1_000_000;
    // The browser and the server share a wall clock but not a monotonic
    // origin, so the press is placed on the monotonic timeline by subtracting
    // the loopback delay the wall clocks just measured.
    let delay_ns = received.unix_ns.saturating_sub(clicked_unix_ns);
    let clicked = Stamp {
        unix_ns: clicked_unix_ns,
        mono_ns: received.mono_ns.saturating_sub(delay_ns),
    };

    let direction = serde_json::to_value(button.direction)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "neutral".into());

    state
        .tx
        .send(Press {
            id: button.id.clone(),
            direction,
            received,
            clicked,
        })
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    Ok(Json(PressReply {
        ok: true,
        label: button.text.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use crate::config::Direction;

    #[test]
    fn directions_serialise_to_the_strings_the_recording_stores() {
        let as_text = |direction| {
            serde_json::to_value(direction)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(as_text(Direction::Up), "up");
        assert_eq!(as_text(Direction::Down), "down");
        assert_eq!(as_text(Direction::Neutral), "neutral");
    }

    #[test]
    fn the_page_is_embedded_and_wired_to_the_endpoints() {
        assert!(super::PAGE.contains("/buttons"));
        assert!(super::PAGE.contains("/label"));
        assert!(super::PAGE.contains("clicked_unix_ms"));
    }
}
