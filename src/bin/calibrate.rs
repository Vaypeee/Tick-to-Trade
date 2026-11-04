//! Reads the collected match history and asks whether the price was the
//! probability.
//!
//! Input is whatever `history` wrote: one trade log per resolved market, with
//! a sidecar naming the winner. Nothing here touches the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tick_to_trade::analysis::calibration::{
    self, Bucket, Entry, bucket_count, first_touches, kelly, study_rule,
};

const USAGE: &str = "\
usage: calibrate [--dir <path>] [--bucket <width>] [--cost <p>]
                 [--band <p>] [--min-trades <n>]

    --dir <path>        where `history` wrote (default: data/history)
    --bucket <width>    price bucket width (default: 0.05)
    --cost <p>          round-trip cost in probability points (default: 0.01)
    --band <p>          entries at or below this price feed the exit study
                        (default: 0.10)
    --min-trades <n>    skip markets with fewer trades (default: 50)
    -h, --help          print this message

Every game enters a price bucket once, at its first trade there, so a game
that lingered cannot outvote one that passed through.
";

#[derive(Debug, PartialEq)]
struct Args {
    dir: PathBuf,
    bucket: f64,
    cost: f64,
    band: f64,
    min_trades: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("data/history"),
            bucket: 0.05,
            cost: 0.01,
            band: 0.10,
            min_trades: 50,
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--dir" => parsed.dir = PathBuf::from(args.next().ok_or("`--dir` expects a path")?),
            "--bucket" => {
                let value = args.next().ok_or("`--bucket` expects a width")?;
                parsed.bucket = value.parse().map_err(|_| format!("bad width `{value}`"))?;
                if !(0.001..=0.5).contains(&parsed.bucket) {
                    return Err("`--bucket` must be between 0.001 and 0.5".into());
                }
            }
            "--cost" => {
                let value = args.next().ok_or("`--cost` expects a price")?;
                parsed.cost = value.parse().map_err(|_| format!("bad cost `{value}`"))?;
                if !(0.0..=0.5).contains(&parsed.cost) {
                    return Err("`--cost` must be between 0 and 0.5".into());
                }
            }
            "--band" => {
                let value = args.next().ok_or("`--band` expects a price")?;
                parsed.band = value.parse().map_err(|_| format!("bad band `{value}`"))?;
                if !(0.0..1.0).contains(&parsed.band) {
                    return Err("`--band` must be below 1".into());
                }
            }
            "--min-trades" => {
                let value = args.next().ok_or("`--min-trades` expects a number")?;
                parsed.min_trades = value.parse().map_err(|_| format!("bad count `{value}`"))?;
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(Some(parsed))
}

/// One market's trade log, split into a price series per side.
struct Game {
    slug: String,
    /// Prices in arrival order, per outcome index.
    sides: BTreeMap<usize, Vec<f64>>,
    winner: usize,
    trades: usize,
}

fn read_game(jsonl: &Path) -> Result<Option<Game>> {
    let sidecar = jsonl.with_extension("meta.json");
    let Ok(raw) = std::fs::read_to_string(&sidecar) else {
        return Ok(None);
    };
    let meta: Value = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", sidecar.display()))?;
    let Some(winner) = meta.get("winner_index").and_then(Value::as_u64) else {
        return Ok(None);
    };
    let tokens: Vec<String> = meta
        .get("token_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if tokens.len() != 2 {
        return Ok(None);
    }

    let body =
        std::fs::read_to_string(jsonl).with_context(|| format!("reading {}", jsonl.display()))?;
    let mut sides: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    let mut trades = 0usize;

    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = match serde_json::from_str(line) {
            Ok(row) => row,
            Err(_) => continue,
        };
        let Some(price) = row.get("price").and_then(Value::as_f64) else {
            continue;
        };
        if !(0.0..=1.0).contains(&price) {
            continue;
        }
        // `outcome_index` is 999 on some rows, so the token is the authority.
        let Some(token) = row.get("token_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(index) = tokens.iter().position(|id| id == token) else {
            continue;
        };
        sides.entry(index).or_default().push(price);
        trades += 1;
    }

    Ok(Some(Game {
        slug: meta
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned(),
        sides,
        winner: winner as usize,
        trades,
    }))
}

fn report_calibration(buckets: &[Bucket], cost: f64) {
    println!("\n== was the price the probability? ==");
    println!("  One entry per game per bucket, at its first trade there.\n");
    println!(
        "  {:>11}  {:>8}  {:>8}  {:>9}  {:>9}  {:>17}  {:>7}",
        "price", "games", "won", "implied", "realised", "95% interval", "Kelly"
    );

    for bucket in buckets {
        let Some(realised) = bucket.realised() else {
            continue;
        };
        let (low, high) = bucket.interval().unwrap_or((0.0, 0.0));
        let mark = if bucket.significant() { "*" } else { " " };
        let stake = kelly(realised, bucket.hi + cost);
        println!(
            "  {:>4.2}-{:<4.2} {} {:>8}  {:>8}  {:>8.1}%  {:>8.1}%  {:>7.1}% - {:<6.1}% {:>6.1}%",
            bucket.lo,
            bucket.hi,
            mark,
            bucket.entries,
            bucket.wins,
            100.0 * bucket.implied(),
            100.0 * realised,
            100.0 * low,
            100.0 * high,
            100.0 * stake,
        );
    }
    println!("\n  * the interval excludes the implied probability.");
    println!("  The intervals assume independent entries and are therefore too");
    println!("  narrow: both sides of one game appear, and the games of one series");
    println!("  share a roster and a day. Read them as the best case for the");
    println!("  evidence, not the confidence a referee would accept.");
    println!("  Kelly is sized against the top of the bucket plus cost, which is");
    println!("  the worst price inside it, and is zero wherever there is no edge.");
}

fn report_exits(entries: &[&Entry], band: f64, cost: f64) {
    println!("\n== buying at or below {band:.2} and selling into a bounce ==");
    if entries.is_empty() {
        println!("  No entry in that band. Nothing to study.");
        return;
    }

    let average: f64 = entries.iter().map(|entry| entry.price).sum::<f64>() / entries.len() as f64;
    let wins = entries.iter().filter(|entry| entry.won).count();
    println!(
        "  {} entries, average price {:.3}, {} of them went on to win ({:.1}%).",
        entries.len(),
        average,
        wins,
        100.0 * wins as f64 / entries.len() as f64
    );
    println!("  Cost charged once per entry: {cost:.3}.\n");
    println!(
        "  {:>8}  {:>8}  {:>8}  {:>10}  {:>11}  {:>9}",
        "sell at", "reached", "rode on", "mean net", "per stake", "total"
    );

    for target in [0.15, 0.20, 0.25, 0.30, 0.40, 0.50, 0.75, 1.00] {
        if target <= band {
            continue;
        }
        let rule = study_rule(entries, target, cost);
        let mean = rule.mean_net().unwrap_or(0.0);
        let roi = rule.roi(average).unwrap_or(0.0);
        println!(
            "  {:>8.2}  {:>7.1}%  {:>7.1}%  {:>+10.4}  {:>+10.1}%  {:>+9.2}",
            target,
            100.0 * rule.filled as f64 / rule.trials as f64,
            100.0 * rule.rode_to_win as f64 / rule.trials as f64,
            mean,
            100.0 * roi,
            rule.total_net,
        );
    }

    println!("\n  `reached` is how often the target was actually traded after entry;");
    println!("  `rode on` is how often it was not and the position won anyway.");
    println!("  `per stake` is the mean net over the average entry price, which is");
    println!("  what doubling or tripling a stake really amounts to once the");
    println!("  entries that went to zero are counted alongside it.");
}

fn run(args: &Args) -> Result<()> {
    let trades_dir = args.dir.join("trades");
    if !trades_dir.is_dir() {
        bail!(
            "{} does not exist — run `history` first",
            trades_dir.display()
        );
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(&trades_dir)
        .with_context(|| format!("reading {}", trades_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!("no recordings in {}", trades_dir.display());
    }

    let mut touches: Vec<(usize, Entry)> = Vec::new();
    let mut games = 0usize;
    let mut skipped_thin = 0usize;
    let mut skipped_unlabelled = 0usize;
    let mut total_trades = 0usize;
    let mut reversals = Vec::new();

    for file in &files {
        let Some(game) = read_game(file)? else {
            skipped_unlabelled += 1;
            continue;
        };
        if game.trades < args.min_trades {
            skipped_thin += 1;
            continue;
        }
        games += 1;
        total_trades += game.trades;

        for (index, series) in &game.sides {
            let won = *index == game.winner;
            let found = first_touches(series, won, args.bucket);
            // Worth naming: a side that traded in the lowest bucket and still
            // won is the phenomenon this whole study is about.
            if won && series.iter().any(|price| *price <= 0.02) {
                reversals.push(game.slug.clone());
            }
            touches.extend(found);
        }
    }

    println!("== {} game(s), {total_trades} trades ==", games);
    if skipped_thin > 0 {
        println!("   {skipped_thin} skipped as too thin to price");
    }
    if skipped_unlabelled > 0 {
        println!("   {skipped_unlabelled} skipped with no resolution on file");
    }
    println!(
        "   {} bucket(s) of {:.3}, {} entries in all",
        bucket_count(args.bucket),
        args.bucket,
        touches.len()
    );
    if !reversals.is_empty() {
        println!(
            "   {} side(s) traded at or below 0.02 and still won",
            reversals.len()
        );
        for slug in reversals.iter().take(5) {
            println!("     {slug}");
        }
        if reversals.len() > 5 {
            println!("     and {} more", reversals.len() - 5);
        }
    }

    let buckets = calibration::calibrate(&touches, args.bucket);
    report_calibration(&buckets, args.cost);

    let band: Vec<&Entry> = touches
        .iter()
        .map(|(_, entry)| entry)
        .filter(|entry| entry.price <= args.band)
        .collect();
    report_exits(&band, args.band, args.cost);

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

    match run(&args) {
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
    fn defaults_are_the_documented_ones() {
        let parsed = parse_args(args(&[])).unwrap().unwrap();
        assert_eq!(parsed, Args::default());
        assert_eq!(parsed.bucket, 0.05);
        assert_eq!(parsed.cost, 0.01);
        assert_eq!(parsed.band, 0.10);
    }

    #[test]
    fn parses_the_knobs() {
        let parsed = parse_args(args(&[
            "--bucket",
            "0.01",
            "--cost",
            "0.02",
            "--band",
            "0.05",
            "--min-trades",
            "10",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.bucket, 0.01);
        assert_eq!(parsed.cost, 0.02);
        assert_eq!(parsed.band, 0.05);
        assert_eq!(parsed.min_trades, 10);
    }

    #[test]
    fn rejects_settings_that_would_produce_nonsense() {
        // A bucket wider than a half, or a cost of most of the stake, would
        // print a table that looks fine and means nothing.
        assert!(parse_args(args(&["--bucket", "0.9"])).is_err());
        assert!(parse_args(args(&["--bucket", "0"])).is_err());
        assert!(parse_args(args(&["--cost", "0.9"])).is_err());
        assert!(parse_args(args(&["--band", "1.0"])).is_err());
        assert!(parse_args(args(&["--dir"])).is_err());
        assert!(parse_args(args(&["--nope"])).is_err());
    }
}
