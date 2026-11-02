//! Entry point for the tick-to-trade pipeline.
//!
//! The pipeline itself is not implemented yet: this binary only resolves the
//! run mode and the configuration file, so that the repository builds and the
//! command-line contract documented in the README is fixed early.

use std::path::Path;
use std::process::ExitCode;

const CONFIG_PATH: &str = "config.toml";
const USAGE: &str = "\
usage: tick-to-trade --mode <paper|live>

    --mode paper    simulate fills locally, send no orders (default)
    --mode live     send real orders to the CLOB
    -h, --help      print this message
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Simulate fills locally. Sends nothing to the exchange.
    Paper,
    /// Send real orders. Requires credentials in the configuration file.
    Live,
}

impl Mode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            other => Err(format!(
                "unknown mode `{other}`, expected `paper` or `live`"
            )),
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Mode>, String> {
    let mut mode = Mode::Paper;
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--mode" => {
                let raw = args.next().ok_or("`--mode` expects a value")?;
                mode = Mode::parse(&raw)?;
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    Ok(Some(mode))
}

fn run(mode: Mode) -> Result<(), String> {
    if !Path::new(CONFIG_PATH).exists() {
        return Err(format!(
            "`{CONFIG_PATH}` not found — copy `config.example.toml` to `{CONFIG_PATH}` and fill it in"
        ));
    }

    // Guard the dangerous path before any of it exists, rather than after.
    if mode == Mode::Live {
        return Err(
            "live mode is not implemented: no execution path has been written or tested yet".into(),
        );
    }

    println!("tick-to-trade: {mode:?} mode, config `{CONFIG_PATH}`");
    println!("no pipeline stage is implemented yet — see the roadmap in README.md");
    Ok(())
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Ok(None) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Some(mode)) => match run(mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            eprintln!("error: {err}\n");
            eprint!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn defaults_to_paper_mode() {
        assert_eq!(parse_args(args(&[])).unwrap(), Some(Mode::Paper));
    }

    #[test]
    fn parses_explicit_modes() {
        assert_eq!(
            parse_args(args(&["--mode", "live"])).unwrap(),
            Some(Mode::Live)
        );
        assert_eq!(
            parse_args(args(&["--mode", "paper"])).unwrap(),
            Some(Mode::Paper)
        );
    }

    #[test]
    fn help_requests_no_mode() {
        assert_eq!(parse_args(args(&["--help"])).unwrap(), None);
    }

    #[test]
    fn rejects_unknown_mode_and_missing_value() {
        assert!(parse_args(args(&["--mode", "yolo"])).is_err());
        assert!(parse_args(args(&["--mode"])).is_err());
        assert!(parse_args(args(&["--verbose"])).is_err());
    }
}
