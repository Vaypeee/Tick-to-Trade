//! Configuration loading.
//!
//! Only the sections the current binaries need are modelled. Unknown sections
//! and fields are ignored, so `config.toml` can describe stages that are not
//! implemented yet without breaking the ones that are.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Polling faster than this hammers the provider for no useful resolution.
const MIN_POLL_INTERVAL_MS: u64 = 100;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub polymarket: Polymarket,
    /// Absent until a sports feed has been chosen. The market recorder does
    /// not need one, so its absence is not an error until a sports recording
    /// is actually asked for.
    #[serde(default)]
    pub sports_feed: Option<SportsFeed>,
    /// Absent unless labels are being recorded.
    #[serde(default)]
    pub labels: Option<Labels>,
    #[serde(default)]
    pub decision: DecisionLimits,
    #[serde(default)]
    pub execution: Execution,
    #[serde(default)]
    pub recording: Recording,
}

/// The rules the decision layer applies. Defaults are deliberately cautious:
/// a configuration that is missing should not become a configuration that
/// trades everything.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DecisionLimits {
    /// Edge required beyond the round trip cost, in probability points.
    pub min_edge: f64,
    pub max_position: f64,
    /// Units per entry.
    pub size: f64,
    /// Refuse a book wider than this.
    pub max_spread: f64,
    /// Refuse when the touch cannot absorb the whole order.
    pub require_touch_depth: bool,
    /// A signal older than this is stale.
    pub max_signal_age_ms: u64,
}

impl Default for DecisionLimits {
    fn default() -> Self {
        Self {
            min_edge: 0.03,
            max_position: 50.0,
            size: 10.0,
            max_spread: 0.05,
            require_touch_depth: true,
            max_signal_age_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Execution {
    /// "paper" simulates, "live" would send orders and is not implemented.
    pub mode: String,
    /// Taker fee on each leg, in basis points of notional.
    pub fee_bps: f64,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            mode: "paper".to_string(),
            fee_bps: 0.0,
        }
    }
}

/// The spectator baseline: a local page whose buttons stamp the moment an
/// ordinary viewer, watching the public broadcast, could have known.
///
/// A click is not the instant the event happened — the stream is seconds
/// behind and a human needs time to react. That is the point. It marks when
/// public information arrived for someone with no advantage, which is the
/// only reference the measurement needs: how far ahead of a normal spectator
/// the order book already was.
#[derive(Debug, Deserialize)]
pub struct Labels {
    #[serde(default = "default_bind")]
    pub bind: String,
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Button {
    /// Short identifier written into the recording.
    pub id: String,
    /// What the button says on screen.
    pub text: String,
    /// Single character that triggers it. A mouse is far too slow for this:
    /// the whole point is to click as close to what you saw as possible.
    #[serde(default)]
    pub key: String,
    /// Which way this event should push the first outcome. The analysis uses
    /// it to tell a move that agrees with the event from one that does not,
    /// because a market moving the wrong way is noise, not a reaction.
    #[serde(default)]
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
    #[default]
    Neutral,
}

/// An HTTP source polled for live sports state.
#[derive(Debug, Deserialize)]
pub struct SportsFeed {
    pub url: String,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Sent with every request. A paid feed authenticates here; the values are
    /// credentials and are never written to the recording metadata.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Write a heartbeat after this many unchanged polls. Zero disables it.
    #[serde(default = "default_heartbeat_polls")]
    pub heartbeat_polls: u64,
}

#[derive(Debug, Deserialize)]
pub struct Polymarket {
    /// Websocket endpoint of the CLOB market channel.
    pub ws_url: String,
    /// Outcome token ids to subscribe to. These are the `assets_ids` of the
    /// market channel, not condition ids.
    #[serde(default)]
    pub asset_ids: Vec<String>,
    /// The market channel expects a `PING` text frame on this interval.
    #[serde(default = "default_ping_interval_secs")]
    pub ping_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Recording {
    pub output_dir: PathBuf,
    /// Frames buffered between the socket and the writer. A full queue stalls
    /// the read loop, which skews every later timestamp, so the counter for it
    /// is reported rather than hidden.
    pub queue_capacity: usize,
    pub stats_interval_secs: u64,
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("data/recordings"),
            queue_capacity: 65_536,
            stats_interval_secs: 60,
        }
    }
}

fn default_ping_interval_secs() -> u64 {
    10
}

fn default_poll_interval_ms() -> u64 {
    1_000
}

fn default_timeout_ms() -> u64 {
    5_000
}

fn default_heartbeat_polls() -> u64 {
    60
}

fn default_bind() -> String {
    "127.0.0.1:7878".to_string()
}

impl Config {
    /// Checks the configuration is sufficient to *record* the market.
    ///
    /// Kept apart from validation because being well formed and being enough
    /// for a particular job are different questions: replaying a recording
    /// offline needs the trading rules and no subscription at all.
    pub fn validate_for_recording(&self) -> Result<()> {
        if self.polymarket.asset_ids.is_empty() {
            bail!(
                "`polymarket.asset_ids` is empty — there is nothing to record. \
                 Outcome token ids come from the Gamma markets API."
            );
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read `{}` — copy `config.example.toml` and fill it in",
                path.display()
            )
        })?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("`{}` is not valid configuration", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let url = &self.polymarket.ws_url;
        if !(url.starts_with("wss://") || url.starts_with("ws://")) {
            bail!("`polymarket.ws_url` must be a websocket URL, got `{url}`");
        }
        if self.polymarket.ping_interval_secs == 0 {
            bail!("`polymarket.ping_interval_secs` must be greater than zero");
        }
        if self.recording.queue_capacity == 0 {
            bail!("`recording.queue_capacity` must be greater than zero");
        }
        let decision = &self.decision;
        if decision.size <= 0.0 {
            bail!("`decision.size` must be greater than zero");
        }
        if decision.max_position < decision.size {
            bail!(
                "`decision.max_position` is below `decision.size`, so no order could \
                 ever be placed"
            );
        }
        if !(decision.max_spread > 0.0 && decision.max_spread < 1.0) {
            bail!("`decision.max_spread` is a probability width, so 0 < w < 1");
        }
        if self.execution.fee_bps < 0.0 {
            bail!("`execution.fee_bps` cannot be negative");
        }
        if let Some(labels) = &self.labels {
            // The page is unauthenticated and its posts are written to disk,
            // so it is a loopback tool and nothing else.
            let host = labels
                .bind
                .rsplit_once(':')
                .map(|(host, _)| host)
                .unwrap_or("");
            if !(host == "127.0.0.1" || host == "localhost" || host == "[::1]") {
                bail!(
                    "`labels.bind` must stay on loopback, got `{}` — the page takes                      unauthenticated writes",
                    labels.bind
                );
            }
            if labels.buttons.is_empty() {
                bail!("`labels.buttons` is empty — there would be nothing to label with");
            }
            let mut ids: Vec<&str> = labels.buttons.iter().map(|b| b.id.as_str()).collect();
            ids.sort_unstable();
            let count = ids.len();
            ids.dedup();
            if ids.len() != count {
                bail!("`labels.buttons` has duplicate ids");
            }
            let mut keys: Vec<&str> = labels
                .buttons
                .iter()
                .map(|b| b.key.as_str())
                .filter(|key| !key.is_empty())
                .collect();
            keys.sort_unstable();
            let count = keys.len();
            keys.dedup();
            if keys.len() != count {
                bail!("`labels.buttons` has two buttons on the same key");
            }
        }
        if let Some(feed) = &self.sports_feed {
            let url = &feed.url;
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                bail!("`sports_feed.url` must be an HTTP URL, got `{url}`");
            }
            if feed.poll_interval_ms < MIN_POLL_INTERVAL_MS {
                bail!(
                    "`sports_feed.poll_interval_ms` must be at least {MIN_POLL_INTERVAL_MS}; \
                     polling faster buys no resolution and risks being rate limited"
                );
            }
            if feed.timeout_ms == 0 {
                bail!("`sports_feed.timeout_ms` must be greater than zero");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [polymarket]
        ws_url = "wss://example.invalid/ws/market"
        asset_ids = ["123", "456"]
    "#;

    fn parse(raw: &str) -> Result<Config> {
        let config: Config = toml::from_str(raw)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn applies_defaults() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.polymarket.ping_interval_secs, 10);
        assert_eq!(config.recording.queue_capacity, 65_536);
        assert_eq!(
            config.recording.output_dir,
            PathBuf::from("data/recordings")
        );
    }

    #[test]
    fn ignores_sections_for_unimplemented_stages() {
        let raw = format!("{MINIMAL}\n[decision]\nmin_edge = 0.03\n");
        assert!(parse(&raw).is_ok());
    }

    #[test]
    fn an_empty_subscription_is_a_recording_problem_not_a_parsing_one() {
        let raw = r#"
            [polymarket]
            ws_url = "wss://example.invalid/ws/market"
        "#;
        // Well formed, so it loads: an offline replay needs no subscription.
        let config = parse(raw).unwrap();
        // But it cannot be recorded with.
        assert!(config.validate_for_recording().is_err());
        assert!(parse(MINIMAL).unwrap().validate_for_recording().is_ok());
    }

    #[test]
    fn a_sports_feed_is_optional_but_validated_when_present() {
        assert!(parse(MINIMAL).unwrap().sports_feed.is_none());

        let good =
            format!("{MINIMAL}\n[sports_feed]\nurl = \"https://example.invalid/scoreboard\"\n");
        let feed = parse(&good).unwrap().sports_feed.unwrap();
        assert_eq!(feed.poll_interval_ms, 1_000);
        assert_eq!(feed.timeout_ms, 5_000);
        assert_eq!(feed.heartbeat_polls, 60);
        assert!(feed.headers.is_empty());
    }

    #[test]
    fn rejects_a_sports_feed_that_would_hammer_the_provider() {
        let raw = format!(
            "{MINIMAL}\n[sports_feed]\nurl = \"https://example.invalid\"\npoll_interval_ms = 5\n"
        );
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn rejects_a_sports_feed_that_is_not_http() {
        let raw = format!("{MINIMAL}\n[sports_feed]\nurl = \"wss://example.invalid\"\n");
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn reads_sports_feed_headers() {
        let raw = format!(
            "{MINIMAL}\n[sports_feed]\nurl = \"https://example.invalid\"\n\
             [sports_feed.headers]\n\"x-api-key\" = \"secret\"\n"
        );
        let feed = parse(&raw).unwrap().sports_feed.unwrap();
        assert_eq!(
            feed.headers.get("x-api-key").map(String::as_str),
            Some("secret")
        );
    }

    const BUTTONS: &str = r#"
        [labels]
        [[labels.buttons]]
        id = "blue_fight"
        text = "Blue won the fight"
        key = "q"
        direction = "up"
        [[labels.buttons]]
        id = "red_fight"
        text = "Red won the fight"
        key = "w"
        direction = "down"
    "#;

    #[test]
    fn reads_label_buttons_with_their_direction() {
        let labels = parse(&format!("{MINIMAL}{BUTTONS}"))
            .unwrap()
            .labels
            .unwrap();
        assert_eq!(labels.bind, "127.0.0.1:7878");
        assert_eq!(labels.buttons.len(), 2);
        assert_eq!(labels.buttons[0].direction, Direction::Up);
        assert_eq!(labels.buttons[1].direction, Direction::Down);
    }

    #[test]
    fn refuses_to_expose_the_label_page_off_loopback() {
        let raw = format!("{MINIMAL}{BUTTONS}").replace(
            "[labels]",
            "[labels]
bind = \"0.0.0.0:7878\"",
        );
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn rejects_buttons_that_collide() {
        let same_key = format!("{MINIMAL}{BUTTONS}").replace("key = \"w\"", "key = \"q\"");
        assert!(parse(&same_key).is_err());
        let same_id =
            format!("{MINIMAL}{BUTTONS}").replace("id = \"red_fight\"", "id = \"blue_fight\"");
        assert!(parse(&same_id).is_err());
    }

    #[test]
    fn rejects_a_label_section_with_no_buttons() {
        assert!(
            parse(&format!(
                "{MINIMAL}
[labels]
buttons = []
"
            ))
            .is_err()
        );
    }

    #[test]
    fn decision_limits_default_to_something_cautious() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.decision.min_edge, 0.03);
        assert_eq!(config.decision.size, 10.0);
        assert!(config.decision.require_touch_depth);
        assert_eq!(config.execution.mode, "paper");
        assert_eq!(config.execution.fee_bps, 0.0);
    }

    #[test]
    fn reads_decision_limits_when_given() {
        let raw = format!(
            "{MINIMAL}\n[decision]\nmin_edge = 0.01\nsize = 5.0\nmax_position = 20.0\n\
             [execution]\nfee_bps = 25.0\n"
        );
        let config = parse(&raw).unwrap();
        assert_eq!(config.decision.min_edge, 0.01);
        assert_eq!(config.decision.size, 5.0);
        assert_eq!(config.execution.fee_bps, 25.0);
    }

    #[test]
    fn rejects_limits_that_could_never_place_an_order() {
        let raw = format!("{MINIMAL}\n[decision]\nsize = 100.0\nmax_position = 10.0\n");
        assert!(parse(&raw).is_err());
        let zero = format!("{MINIMAL}\n[decision]\nsize = 0.0\n");
        assert!(parse(&zero).is_err());
    }

    #[test]
    fn rejects_a_spread_limit_that_is_not_a_width() {
        let raw = format!("{MINIMAL}\n[decision]\nmax_spread = 2.0\n");
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn rejects_non_websocket_url() {
        let raw = r#"
            [polymarket]
            ws_url = "https://example.invalid"
            asset_ids = ["123"]
        "#;
        assert!(parse(raw).is_err());
    }
}
