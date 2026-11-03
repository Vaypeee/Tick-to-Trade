//! Checks the configuration and says what is ready to run.
//!
//! The pipeline this crate is named after does not exist yet, and until the
//! measurements justify building it, it should not. What does exist is a set
//! of tools that each need the configuration to be right, and a mistake in it
//! only shows up as an empty recording afterwards — which is the worst time
//! to find out, because the match is over.
//!
//! So the default command is a pre-flight: load the configuration, say what
//! it will and will not let you do, and fail loudly if something would stop a
//! recording before it starts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use tick_to_trade::config::Config;

const DEFAULT_CONFIG: &str = "config.toml";
const USAGE: &str = "\
usage: tick-to-trade [--config <path>]

    --config <path>    configuration to check (default: config.toml)
    -h, --help         print this message

Checks the configuration and reports what is ready. Exits non-zero if
something would stop a recording.

The tools this project ships:

    markets      find a live market and its outcome token ids
    record       capture a feed to disk, timestamped
    analyse      compare recordings: market against feed, spectator, itself
    paper        replay the trading rules over a recording
    dashboard    draw a recording as an HTML timeline
";

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--config" => {
                config = args.next().ok_or("`--config` expects a path")?.into();
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(Some(config))
}

fn count_recordings(dir: &Path) -> Option<(usize, usize, usize)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut market = 0;
    let mut sports = 0;
    let mut labels = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        if name.starts_with("polymarket-") {
            market += 1;
        } else if name.starts_with("sports-") {
            sports += 1;
        } else if name.starts_with("labels-") {
            labels += 1;
        }
    }
    Some((market, sports, labels))
}

fn check(path: &Path) -> Result<bool> {
    println!("configuration: {}", path.display());
    let config = Config::load(path)?;

    let outcomes = config.polymarket.asset_ids.len();
    let ready_to_record = config.validate_for_recording().is_ok();
    println!("\nmarket");
    println!("  {}", config.polymarket.ws_url);
    match outcomes {
        0 => println!("  no outcomes subscribed — `markets` prints the ids to paste"),
        1 => println!(
            "  1 outcome subscribed; list the other one too, or half the deltas\n  \
             arrive with no book to apply them to"
        ),
        n => println!("  {n} outcomes subscribed, first is the reference for label directions"),
    }

    match &config.labels {
        Some(labels) => {
            println!("\nspectator baseline");
            println!("  http://{}/", labels.bind);
            println!("  {} button(s)", labels.buttons.len());
            let without_key = labels
                .buttons
                .iter()
                .filter(|button| button.key.is_empty())
                .count();
            if without_key > 0 {
                println!(
                    "  {without_key} with no key: reachable only by mouse, which costs a\n  \
                     few hundred milliseconds of the thing being measured"
                );
            }
        }
        None => println!("\nspectator baseline\n  not configured"),
    }

    match &config.sports_feed {
        Some(feed) => {
            println!("\nsports feed");
            println!("  {} every {} ms", feed.url, feed.poll_interval_ms);
        }
        None => println!("\nsports feed\n  not configured (the first hypothesis is on hold)"),
    }

    println!("\nrules");
    println!(
        "  size {:.0}, max position {:.0}, min edge {:.3} beyond the round trip cost",
        config.decision.size, config.decision.max_position, config.decision.min_edge
    );
    println!(
        "  refuse a spread over {:.3}, signals stale after {} ms, {:.1} bps per leg",
        config.decision.max_spread, config.decision.max_signal_age_ms, config.execution.fee_bps
    );
    if config.execution.mode != "paper" {
        println!(
            "  execution.mode is `{}` — only `paper` exists, and it sends nothing",
            config.execution.mode
        );
    }

    let dir = &config.recording.output_dir;
    println!("\nrecordings in {}", dir.display());
    match count_recordings(dir) {
        Some((market, sports, labels)) => {
            println!("  {market} market, {sports} sports, {labels} labels")
        }
        None => println!("  none yet; the directory is created on the first run"),
    }

    println!();
    if ready_to_record {
        println!("Ready. Record both feeds at once, one process each:");
        println!("  record --feed polymarket &");
        if config.labels.is_some() {
            println!("  record --feed labels &");
        }
    } else {
        println!("Not ready to record: `polymarket.asset_ids` is empty.");
        println!("  markets --search \"league of legends\"");
    }
    Ok(ready_to_record)
}

fn main() -> ExitCode {
    let path = match parse_args(std::env::args().skip(1)) {
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Some(path)) => path,
        Err(error) => {
            eprintln!("error: {error}\n");
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match check(&path) {
        Ok(true) => ExitCode::SUCCESS,
        // A configuration that cannot record is not a crash, but it is not a
        // success either: a script that chains this should stop here.
        Ok(false) => ExitCode::FAILURE,
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
    fn the_default_configuration_is_config_toml() {
        assert_eq!(
            parse_args(args(&[])).unwrap(),
            Some(PathBuf::from("config.toml"))
        );
    }

    #[test]
    fn another_configuration_can_be_checked() {
        assert_eq!(
            parse_args(args(&["--config", "other.toml"])).unwrap(),
            Some(PathBuf::from("other.toml"))
        );
    }

    #[test]
    fn help_checks_nothing() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
    }

    #[test]
    fn unknown_arguments_are_refused() {
        assert!(parse_args(args(&["--mode", "paper"])).is_err());
        assert!(parse_args(args(&["--config"])).is_err());
    }

    #[test]
    fn counting_a_directory_that_is_not_there_is_not_an_error() {
        assert!(count_recordings(Path::new("no/such/directory")).is_none());
    }

    #[test]
    fn recordings_are_counted_by_the_feed_that_wrote_them() {
        let dir = std::env::temp_dir().join("tick-to-trade-main-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "polymarket-1.jsonl",
            "polymarket-2.jsonl",
            "labels-1.jsonl",
            "polymarket-1.meta.json",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert_eq!(count_recordings(&dir), Some((2, 0, 1)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
