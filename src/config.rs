//! Configuration loading.
//!
//! Only the sections the current binaries need are modelled. Unknown sections
//! and fields are ignored, so `config.toml` can describe stages that are not
//! implemented yet without breaking the ones that are.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub polymarket: Polymarket,
    #[serde(default)]
    pub recording: Recording,
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

impl Config {
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
        if self.polymarket.asset_ids.is_empty() {
            bail!(
                "`polymarket.asset_ids` is empty — there is nothing to record. \
                 Outcome token ids come from the Gamma markets API."
            );
        }
        if self.polymarket.ping_interval_secs == 0 {
            bail!("`polymarket.ping_interval_secs` must be greater than zero");
        }
        if self.recording.queue_capacity == 0 {
            bail!("`recording.queue_capacity` must be greater than zero");
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
    fn rejects_empty_asset_ids() {
        let raw = r#"
            [polymarket]
            ws_url = "wss://example.invalid/ws/market"
        "#;
        assert!(parse(raw).is_err());
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
