//! Finds a live market and prints its outcome token ids.
//!
//! The recorders need `assets_ids`, which are not the market id and are not
//! shown anywhere obvious. Getting them wrong — or listing only one of a
//! market's two outcomes — produces a recording whose book cannot be rebuilt,
//! and the mistake is only visible afterwards.
//!
//! So this prints both of them, with the outcome each one belongs to, in the
//! shape they go into the configuration. The order matters: label directions
//! are expressed relative to whichever outcome is listed first.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

const GAMMA: &str = "https://gamma-api.polymarket.com/markets";

const USAGE: &str = "\
usage: markets [--search <text>] [options]

    --search <text>        keep markets whose question or slug contains this,
                           case-insensitively; repeatable
    --min-liquidity <n>    skip anything thinner (default: 10000)
    --limit <n>            how many to print (default: 10)
    --scan <n>             how many live markets to look through (default: 400)
    -h, --help             print this message

Prints the outcome token ids of live markets, in the shape they go into
`polymarket.asset_ids`. Both outcomes are listed: the channel pushes price
changes for every outcome of a market but only snapshots the ones subscribed
to, so recording one side leaves half the deltas with nothing to apply them
to. Whichever you list first is the one label directions refer to.
";

#[derive(Debug, PartialEq)]
struct Args {
    search: Vec<String>,
    min_liquidity: f64,
    limit: usize,
    scan: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            search: Vec::new(),
            min_liquidity: 10_000.0,
            limit: 10,
            scan: 400,
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
            "--search" => parsed.search.push(value()?),
            "--min-liquidity" => {
                let raw = value()?;
                parsed.min_liquidity = raw
                    .parse()
                    .map_err(|_| format!("`--min-liquidity` expects a number, got `{raw}`"))?;
            }
            "--limit" => {
                parsed.limit = value()?
                    .parse()
                    .map_err(|_| "`--limit` expects a number".to_string())?;
            }
            "--scan" => {
                parsed.scan = value()?
                    .parse()
                    .map_err(|_| "`--scan` expects a number".to_string())?;
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(Some(parsed))
}

/// Gamma returns several numbers as strings, inconsistently.
fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `clobTokenIds` and `outcomes` arrive as JSON encoded inside a JSON string.
fn nested_list(value: Option<&Value>) -> Vec<String> {
    let Some(Value::String(text)) = value else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(text).unwrap_or_default()
}

fn matches(market: &Value, needles: &[String]) -> bool {
    if needles.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {}",
        market.get("question").and_then(Value::as_str).unwrap_or(""),
        market.get("slug").and_then(Value::as_str).unwrap_or("")
    )
    .to_lowercase();
    needles
        .iter()
        .any(|needle| haystack.contains(&needle.to_lowercase()))
}

async fn fetch(scan: usize) -> Result<Vec<Value>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("tick-to-trade/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url =
        format!("{GAMMA}?closed=false&active=true&limit={scan}&order=volume24hr&ascending=false");
    let body = client
        .get(&url)
        .send()
        .await
        .context("cannot reach the Gamma markets API")?
        .text()
        .await
        .context("cannot read the Gamma response")?;
    serde_json::from_str::<Vec<Value>>(&body).context("the Gamma response was not a market list")
}

async fn run(args: &Args) -> Result<()> {
    crate_tls();
    let markets = fetch(args.scan).await?;

    let mut shown = 0;
    for market in &markets {
        if shown >= args.limit {
            break;
        }
        if !matches(market, &args.search) {
            continue;
        }
        let ids = nested_list(market.get("clobTokenIds"));
        // A market whose outcomes are not both listed cannot be recorded
        // properly, so it is not offered.
        if ids.len() < 2 {
            continue;
        }
        let liquidity = number(market.get("liquidity"));
        if liquidity < args.min_liquidity {
            continue;
        }

        let outcomes = nested_list(market.get("outcomes"));
        let question = market
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)");

        println!(
            "\n{question}\n  liquidity {:.0}   volume 24h {:.0}   spread {:.3}",
            liquidity,
            number(market.get("volume24hr")),
            number(market.get("spread")),
        );
        println!("  asset_ids = [");
        for (index, id) in ids.iter().enumerate() {
            let name = outcomes.get(index).map(String::as_str).unwrap_or("?");
            println!("    \"{id}\",  # {name}");
        }
        println!("  ]");
        shown += 1;
    }

    if shown == 0 {
        println!("Nothing matched. Try a broader --search or a lower --min-liquidity.");
    } else {
        println!(
            "\nList both ids. The first one is what label directions refer to,\n\
             so put the side you think of as \"blue\" first."
        );
    }
    Ok(())
}

fn crate_tls() {
    tick_to_trade::tls::install_crypto_provider();
}

#[tokio::main]
async fn main() -> ExitCode {
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

    match run(&args).await {
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
    use serde_json::json;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn searching_is_optional_and_repeatable() {
        let parsed = parse_args(args(&[])).unwrap().unwrap();
        assert!(parsed.search.is_empty());
        assert_eq!(parsed.min_liquidity, 10_000.0);

        let parsed = parse_args(args(&["--search", "lol", "--search", "worlds"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.search, ["lol", "worlds"]);
    }

    #[test]
    fn numbers_arrive_as_numbers_or_as_strings() {
        assert_eq!(number(Some(&json!(12.5))), 12.5);
        assert_eq!(number(Some(&json!("12.5"))), 12.5);
        assert_eq!(number(Some(&json!("not a number"))), 0.0);
        assert_eq!(number(None), 0.0);
    }

    #[test]
    fn token_ids_are_json_inside_a_json_string() {
        let market = json!({"clobTokenIds": r#"["111", "222"]"#});
        assert_eq!(nested_list(market.get("clobTokenIds")), ["111", "222"]);
    }

    #[test]
    fn a_malformed_list_yields_nothing_rather_than_half_a_market() {
        let market = json!({"clobTokenIds": "not json"});
        assert!(nested_list(market.get("clobTokenIds")).is_empty());
        assert!(nested_list(Some(&json!(["111"]))).is_empty());
        assert!(nested_list(None).is_empty());
    }

    #[test]
    fn searching_looks_at_the_question_and_the_slug() {
        let market = json!({"question": "Will T1 win?", "slug": "lol-worlds-t1"});
        assert!(matches(&market, &["t1".into()]));
        assert!(matches(&market, &["WORLDS".into()]));
        assert!(matches(&market, &[]));
        assert!(!matches(&market, &["tennis".into()]));
    }

    #[test]
    fn any_of_the_search_terms_is_enough() {
        let market = json!({"question": "Dota 2 final", "slug": "dota-final"});
        assert!(matches(&market, &["tennis".into(), "dota".into()]));
    }
}
