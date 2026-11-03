//! Draws a recording as a self-contained HTML timeline.
//!
//! Replay only. A live view of the book beside the labelling page would
//! quietly destroy the measurement it is meant to illustrate: a spectator who
//! can see the market is no longer an independent baseline, and nothing in
//! the recording would show that it had happened.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use tick_to_trade::{
    analysis::{
        book, flow, presses, reference_asset,
        replay::{self},
    },
    report::timeline::{Burst, Cross, Point, Press, Timeline, render},
};

const USAGE: &str = "\
usage: dashboard --market <file> [--labels <file>] [options]

    --market <file>        a polymarket-*.jsonl recording
    --labels <file>        a labels-*.jsonl recording over the same window
    --out <file>           where to write the page (default: dashboard.html)
    --surge-secs <n>       window over which taking is netted (default: 5)
    --surge-size <n>       net size that counts as a surge; the default is
                           5x the median trade size in the recording
    -h, --help             print this message

Writes one HTML file with an inline SVG. Nothing to serve and nothing to
install: open it in a browser.
";

#[derive(Debug, PartialEq)]
struct Args {
    market: PathBuf,
    labels: Option<PathBuf>,
    out: PathBuf,
    surge_secs: u64,
    surge_size: Option<f64>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            market: PathBuf::new(),
            labels: None,
            out: PathBuf::from("dashboard.html"),
            surge_secs: 5,
            surge_size: None,
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;

    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("`{arg}` expects a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--market" => parsed.market = value()?.into(),
            "--labels" => parsed.labels = Some(value()?.into()),
            "--out" => parsed.out = value()?.into(),
            "--surge-secs" => {
                let secs: u64 = value()?
                    .parse()
                    .map_err(|_| "`--surge-secs` expects a number")?;
                if secs == 0 {
                    return Err("`--surge-secs` must be greater than zero".into());
                }
                parsed.surge_secs = secs;
            }
            "--surge-size" => {
                let size: f64 = value()?
                    .parse()
                    .map_err(|_| "`--surge-size` expects a number")?;
                if size <= 0.0 {
                    return Err("`--surge-size` must be greater than zero".into());
                }
                parsed.surge_size = Some(size);
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }

    if parsed.market.as_os_str().is_empty() {
        return Err("`--market` is required".into());
    }
    Ok(Some(parsed))
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn run(args: &Args) -> Result<()> {
    let records = replay::read(&args.market)?;
    let (quotes, agreement) = book::replay(&records);
    let Some(asset) =
        reference_asset(&args.market).or_else(|| quotes.first().map(|quote| quote.asset.clone()))
    else {
        bail!("the market recording has no prices to draw");
    };

    let points: Vec<Point> = quotes
        .iter()
        .filter(|quote| quote.asset == asset)
        .map(|quote| Point {
            at_unix_ns: quote.at_unix_ns,
            bid: quote.bid,
            ask: quote.ask,
            mid: quote.mid,
        })
        .collect();

    let (trades, sides) = flow::trades(&records, &quotes);
    let crosses: Vec<Cross> = trades
        .iter()
        .filter(|trade| trade.asset == asset && trade.taker != flow::Taker::Unclear)
        .map(|trade| Cross {
            at_unix_ns: trade.at_unix_ns,
            price: trade.price,
            bought: trade.taker == flow::Taker::Buy,
        })
        .collect();

    let threshold = match args.surge_size {
        Some(size) => size,
        None => {
            let mut sizes: Vec<f64> = trades
                .iter()
                .filter(|trade| trade.asset == asset)
                .map(|trade| trade.size)
                .collect();
            median(&mut sizes)
                .map(|value| value * 5.0)
                .filter(|value| *value > 0.0)
                .unwrap_or(1.0)
        }
    };
    let surges = flow::surges(
        &trades,
        &asset,
        args.surge_secs.saturating_mul(1_000_000_000),
        threshold,
    );
    let bursts: Vec<Burst> = surges
        .iter()
        .map(|surge| Burst {
            at_unix_ns: surge.at_unix_ns,
            rose: surge.signed_size > 0.0,
        })
        .collect();

    let marks: Vec<Press> = match &args.labels {
        Some(path) => {
            let labels = replay::read(path)?;
            presses(&labels)
                .into_iter()
                .map(|(at, label, direction)| Press {
                    at_unix_ns: at,
                    label,
                    rose: match direction.as_str() {
                        "up" => Some(true),
                        "down" => Some(false),
                        _ => None,
                    },
                })
                .collect()
        }
        None => Vec::new(),
    };

    let span_secs = points
        .last()
        .zip(points.first())
        .map(|(last, first)| (last.at_unix_ns - first.at_unix_ns) as f64 / 1e9)
        .unwrap_or(0.0);

    let mut cards = vec![
        ("window".into(), format!("{span_secs:.0} s")),
        ("quotes".into(), points.len().to_string()),
        ("trades".into(), crosses.len().to_string()),
        ("surges".into(), bursts.len().to_string()),
    ];
    if !marks.is_empty() {
        cards.push(("presses".into(), marks.len().to_string()));
    }
    if let Some(rate) = agreement.rate() {
        cards.push(("book agrees".into(), format!("{:.0}%", rate * 100.0)));
    }
    if let Some(rate) = sides.rate() {
        cards.push(("side agrees".into(), format!("{:.0}%", rate * 100.0)));
    }

    let subtitle = match &args.labels {
        Some(_) => "The market, and the moments a spectator on the public broadcast knew \
                    something. Where a press sits to the right of a surge, the book moved first."
            .to_string(),
        None => "The market on its own: the spread it quoted, the trades that crossed it, \
                 and the bursts of one-sided taking."
            .to_string(),
    };

    let page = render(&Timeline {
        title: format!(
            "{} — outcome {}",
            args.market
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            &asset[..asset.len().min(16)]
        ),
        subtitle,
        points: &points,
        crosses: &crosses,
        bursts: &bursts,
        presses: &marks,
        cards,
    });

    std::fs::write(&args.out, page)
        .with_context(|| format!("cannot write `{}`", args.out.display()))?;
    println!("wrote {}", args.out.display());
    if points.is_empty() {
        println!("the recording held no quotes for that outcome, so the page is empty");
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Some(args)) => args,
        Err(error) => {
            eprintln!("error: {error}\n");
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match run(&args).context("dashboard failed") {
        Ok(()) => ExitCode::SUCCESS,
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
    fn a_market_recording_is_required() {
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--labels", "l.jsonl"])).is_err());
    }

    #[test]
    fn labels_are_optional_and_the_output_has_a_default() {
        let parsed = parse_args(args(&["--market", "m.jsonl"])).unwrap().unwrap();
        assert!(parsed.labels.is_none());
        assert_eq!(parsed.out, PathBuf::from("dashboard.html"));
        assert_eq!(parsed.surge_secs, 5);
    }

    #[test]
    fn the_output_path_can_be_chosen() {
        let parsed = parse_args(args(&["--market", "m", "--out", "x.html"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.out, PathBuf::from("x.html"));
    }

    #[test]
    fn nonsense_surge_settings_are_refused() {
        assert!(parse_args(args(&["--market", "m", "--surge-secs", "0"])).is_err());
        assert!(parse_args(args(&["--market", "m", "--surge-size", "-1"])).is_err());
    }
}
