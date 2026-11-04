//! Collects resolved match markets and their whole trade history to disk.
//!
//! This is the input to the calibration question: across every past game,
//! when the market priced a team at `p`, how often did that team actually
//! win? Answering it needs two things per game — the path the price took and
//! the result — and nothing else here tries to be clever about either.
//!
//! Two sources, because only one of them goes back:
//!
//! - Gamma lists the events and their markets, and a resolved market carries
//!   its own answer in `outcomePrices`. That is the label.
//! - The CLOB's `prices-history` endpoint returns nothing for a resolved
//!   market, whatever interval or fidelity it is asked for. Measured, not
//!   assumed: of fifteen closed League of Legends markets sampled across
//!   April to November, only the one still inside the current week answered,
//!   including markets that had traded over a million dollars. The data API's
//!   trade log does go back, so the price path is rebuilt from executions.
//!
//! Trades are the better source anyway. A price history would give a line;
//! executions give price, size, side and counterparty, which is what any
//! later question about who was early will need.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tick_to_trade::tls::install_crypto_provider;

const GAMMA: &str = "https://gamma-api.polymarket.com";
const DATA_API: &str = "https://data-api.polymarket.com";
/// Gamma refuses offsets past this, so the catalogue walks backwards in time
/// instead of paging forever. See `catalogue`.
const PAGE: usize = 100;
const TRADE_PAGE: usize = 1000;

const USAGE: &str = "\
usage: history [--tag <id>] [--out <dir>] [--min-volume <n>]
               [--catalogue-only] [--max-markets <n>] [--pace-ms <n>]

    --tag <id>           Gamma tag to collect (default: 65, League of Legends)
    --out <dir>          where to write (default: data/history)
    --min-volume <n>     skip markets below this volume (default: 10000)
    --catalogue-only     list the markets and stop, fetching no trades
    --max-markets <n>    stop after n markets, for a trial run
    --from <YYYY-MM>     oldest month to walk (default: 2025-09)
    --to <YYYY-MM>       newest month to walk (default: this month)
    --pace-ms <n>        delay between requests (default: 250)
    -h, --help           print this message

The catalogue is walked one month at a time rather than by offset: Gamma
refuses offsets past about 2100, which silently cut the catalogue to its six
most recent months. Per-game winner markets start in October 2025.

Resumable. A market whose sidecar says the download finished is skipped, so
an interrupted run continues where it stopped rather than starting over.
";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    tag: u64,
    out: PathBuf,
    min_volume: u64,
    catalogue_only: bool,
    max_markets: Option<usize>,
    pace: Duration,
    /// Oldest month to walk, inclusive. The default is the month the per-game
    /// winner markets first appear; earlier months hold only series and
    /// tournament winners, which this study has no use for.
    from: (u32, u32),
    /// Newest month to walk, inclusive.
    to: (u32, u32),
}

impl Default for Args {
    fn default() -> Self {
        Self {
            tag: 65,
            out: PathBuf::from("data/history"),
            min_volume: 10_000,
            catalogue_only: false,
            max_markets: None,
            pace: Duration::from_millis(250),
            from: (2025, 9),
            to: current_month(),
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--tag" => {
                let value = args.next().ok_or("`--tag` expects an id")?;
                parsed.tag = value.parse().map_err(|_| format!("bad tag `{value}`"))?;
            }
            "--out" => {
                parsed.out = PathBuf::from(args.next().ok_or("`--out` expects a path")?);
            }
            "--min-volume" => {
                let value = args.next().ok_or("`--min-volume` expects a number")?;
                parsed.min_volume = value.parse().map_err(|_| format!("bad volume `{value}`"))?;
            }
            "--catalogue-only" => parsed.catalogue_only = true,
            "--max-markets" => {
                let value = args.next().ok_or("`--max-markets` expects a number")?;
                let n: usize = value.parse().map_err(|_| format!("bad count `{value}`"))?;
                if n == 0 {
                    return Err("`--max-markets` must be greater than zero".into());
                }
                parsed.max_markets = Some(n);
            }
            "--from" => {
                let value = args.next().ok_or("`--from` expects YYYY-MM")?;
                parsed.from = parse_month(&value)?;
            }
            "--to" => {
                let value = args.next().ok_or("`--to` expects YYYY-MM")?;
                parsed.to = parse_month(&value)?;
            }
            "--pace-ms" => {
                let value = args.next().ok_or("`--pace-ms` expects a number")?;
                let ms: u64 = value.parse().map_err(|_| format!("bad pace `{value}`"))?;
                parsed.pace = Duration::from_millis(ms);
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    if parsed.from > parsed.to {
        return Err(format!(
            "`--from` {:04}-{:02} is after `--to` {:04}-{:02}",
            parsed.from.0, parsed.from.1, parsed.to.0, parsed.to.1
        ));
    }
    Ok(Some(parsed))
}

/// One resolved per-game market, reduced to what the study needs.
#[derive(Debug, Clone)]
struct Market {
    event: String,
    slug: String,
    condition: String,
    tokens: Vec<String>,
    outcomes: Vec<String>,
    /// Which outcome resolved true, from `outcomePrices`. `None` when the
    /// market closed without a clean 1/0, which is not evidence of anything
    /// and is kept out of the study rather than guessed at.
    winner: Option<usize>,
    volume: f64,
    ended: String,
    tick: Option<String>,
}

impl Market {
    fn line(&self) -> Value {
        json!({
            "event": self.event,
            "slug": self.slug,
            "condition_id": self.condition,
            "token_ids": self.tokens,
            "outcomes": self.outcomes,
            "winner_index": self.winner,
            "volume": self.volume,
            "ended": self.ended,
            "tick_size": self.tick,
        })
    }
}

fn as_f64(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Gamma encodes several fields as JSON *inside* a JSON string.
fn nested(market: &Value, key: &str) -> Vec<String> {
    market
        .get(key)
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}

/// The per-game winner markets of one event.
///
/// An event carries about seventy markets and almost all of them are props —
/// first blood, total kills, whether both teams took a dragon — that trade a
/// few dollars or nothing at all. The per-game winner markets are where the
/// volume is, and they are the only ones whose price means "this team wins".
fn game_markets(event: &Value, min_volume: f64) -> Vec<Market> {
    let event_slug = event
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let ended = event
        .get("endDate")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut found = Vec::new();
    for market in event
        .get("markets")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let slug = market.get("slug").and_then(Value::as_str).unwrap_or("");
        if !is_game_winner(slug) {
            continue;
        }
        let volume = as_f64(market.get("volume"));
        if volume < min_volume {
            continue;
        }
        let tokens = nested(market, "clobTokenIds");
        let outcomes = nested(market, "outcomes");
        let prices = nested(market, "outcomePrices");
        if tokens.len() != 2 || outcomes.len() != 2 || prices.len() != 2 {
            continue;
        }
        let Some(condition) = market.get("conditionId").and_then(Value::as_str) else {
            continue;
        };

        found.push(Market {
            event: event_slug.clone(),
            slug: slug.to_owned(),
            condition: condition.to_owned(),
            tokens,
            outcomes,
            winner: winner_of(&prices),
            volume,
            ended: ended.clone(),
            tick: market
                .get("orderPriceMinTickSize")
                .map(|value| value.to_string().trim_matches('"').to_owned()),
        });
    }
    found
}

/// `lol-<teams>-<date>-game3` and nothing else.
fn is_game_winner(slug: &str) -> bool {
    let Some(tail) = slug.rsplit('-').next() else {
        return false;
    };
    tail.len() == 5
        && tail.starts_with("game")
        && tail
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_digit() && *byte != b'0')
}

/// Which side resolved true, from a pair that should be exactly one and zero.
fn winner_of(prices: &[String]) -> Option<usize> {
    let parsed: Vec<f64> = prices.iter().map(|p| p.parse().unwrap_or(-1.0)).collect();
    match (parsed.first(), parsed.get(1)) {
        (Some(&a), Some(&b)) if a > 0.99 && b < 0.01 => Some(0),
        (Some(&a), Some(&b)) if b > 0.99 && a < 0.01 => Some(1),
        _ => None,
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Value> {
    let mut last = None;
    for attempt in 0..4u32 {
        match client.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return serde_json::from_str(&body)
                        .with_context(|| format!("{url} returned unparseable JSON"));
                }
                // Gamma answers 422 past its deepest offset, which is a
                // boundary rather than a failure; the caller reads it as one.
                if status.as_u16() == 422 {
                    return Ok(json!({"__status": 422}));
                }
                last = Some(format!("{status}"));
            }
            Err(error) => last = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt + 1))).await;
    }
    bail!(
        "{url} failed after 4 attempts: {}",
        last.unwrap_or_else(|| "no response".into())
    )
}

/// Walks the tag's closed events, a month at a time, keeping the per-game
/// winner markets.
///
/// A plain offset walk cannot reach the whole catalogue: Gamma answers 422
/// past about offset 2100, which silently truncated this to the six most
/// recent months and 3,097 markets. Filtering by `endDate` and stepping
/// backwards one month at a time keeps every query shallow — the busiest
/// month seen holds 551 events, well inside the limit — and reaches back to
/// October 2025, where the per-game winner markets begin. The months before
/// that have League of Legends events but no per-game markets, only series
/// and tournament winners, so there is nothing there for this study.
async fn catalogue(client: &reqwest::Client, args: &Args) -> Result<Vec<Market>> {
    let mut markets = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated: Vec<String> = Vec::new();
    let mut windows = 0usize;

    let mut cursor = args.to;
    while cursor >= args.from {
        let (next_year, next_month) = next_month(cursor);
        let label = format!("{:04}-{:02}", cursor.0, cursor.1);
        let mut offset = 0usize;

        loop {
            // One line on purpose: split with backslash continuations,
            // rustfmt reflows the literal and leaves the indentation inside
            // the URL, which Gamma answers with 422 — indistinguishable from
            // its offset limit, so the walk silently returned nothing.
            let url = format!(
                "{GAMMA}/events?tag_id={tag}&closed=true&limit={PAGE}&offset={offset}&end_date_min={from_year:04}-{from_month:02}-01T00:00:00Z&end_date_max={to_year:04}-{to_month:02}-01T00:00:00Z",
                tag = args.tag,
                from_year = cursor.0,
                from_month = cursor.1,
                to_year = next_year,
                to_month = next_month,
            );
            let page = fetch(client, &url).await?;
            if page.get("__status").is_some() {
                // A single month deep enough to hit the cap would hide
                // markets, so it is named rather than passed over.
                truncated.push(label.clone());
                break;
            }
            let Some(events) = page.as_array() else {
                break;
            };
            if events.is_empty() {
                break;
            }
            for event in events {
                for market in game_markets(event, args.min_volume as f64) {
                    if seen.insert(market.condition.clone()) {
                        markets.push(market);
                    }
                }
            }
            if events.len() < PAGE {
                break;
            }
            offset += PAGE;
            tokio::time::sleep(args.pace).await;
        }

        windows += 1;
        cursor = previous_month(cursor);
        tokio::time::sleep(args.pace).await;
    }

    tracing::info!(
        windows,
        markets = markets.len(),
        "catalogue walked month by month"
    );
    if !truncated.is_empty() {
        tracing::warn!(
            months = truncated.join(", "),
            "these months hit Gamma's offset limit and may be incomplete"
        );
    }
    Ok(markets)
}

/// The month after this one.
fn next_month((year, month): (u32, u32)) -> (u32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

/// The month before this one.
fn previous_month((year, month): (u32, u32)) -> (u32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// Parses `YYYY-MM`.
fn parse_month(text: &str) -> Result<(u32, u32), String> {
    let (year, month) = text
        .split_once('-')
        .ok_or_else(|| format!("expected YYYY-MM, got `{text}`"))?;
    let year: u32 = year.parse().map_err(|_| format!("bad year in `{text}`"))?;
    let month: u32 = month
        .parse()
        .map_err(|_| format!("bad month in `{text}`"))?;
    if !(1..=12).contains(&month) || !(2000..=2100).contains(&year) {
        return Err(format!("`{text}` is not a real month"));
    }
    Ok((year, month))
}

/// This month, in UTC, so the walk never has to be told where to start.
///
/// Days are converted with Howard Hinnant's civil-from-days algorithm, which
/// is exact and needs no calendar table.
fn current_month() -> (u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as u32, month as u32)
}

/// Every trade of one market, oldest first.
///
/// The response carries a profile for each trade — name, bio, avatar — which
/// is empty on every row seen and would multiply the stored size for nothing.
/// Only the execution is kept, plus the wallet, because asking later which
/// wallets were consistently early is the obvious next question and the
/// answer cannot be recovered once it is dropped.
async fn trades(client: &reqwest::Client, condition: &str, pace: Duration) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut url = format!("{DATA_API}/v2/trades?condition={condition}&limit={TRADE_PAGE}");
        if let Some(next) = &cursor {
            url.push_str("&cursor=");
            url.push_str(next);
        }
        let page = fetch(client, &url).await?;
        let rows = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for row in &rows {
            all.push(json!({
                "t_unix": row.get("timestamp").and_then(Value::as_u64),
                "price": as_f64(row.get("price")),
                "size": as_f64(row.get("size")),
                "side": row.get("side").and_then(Value::as_str),
                "token_id": row.get("token_id").and_then(Value::as_str),
                "outcome_index": row.get("outcome_index").and_then(Value::as_u64),
                "wallet": row.get("proxy_wallet").and_then(Value::as_str),
                "tx": row.get("transaction_hash").and_then(Value::as_str),
            }));
        }

        let pagination = page.get("pagination");
        let more = pagination
            .and_then(|p| p.get("has_more"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next = pagination
            .and_then(|p| p.get("next_cursor"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        match (more, next) {
            (true, Some(next)) if rows.len() == TRADE_PAGE => cursor = Some(next),
            _ => break,
        }
        tokio::time::sleep(pace).await;
    }

    // The log arrives newest first. Stored oldest first, so a reader can walk
    // a price path forwards without having to know that.
    all.reverse();
    Ok(all)
}

fn write_lines(path: &Path, lines: &[Value]) -> Result<u64> {
    let mut buffer = Vec::new();
    for line in lines {
        serde_json::to_writer(&mut buffer, line)?;
        buffer.push(b'\n');
    }
    std::fs::write(path, &buffer).with_context(|| format!("writing {}", path.display()))?;
    Ok(buffer.len() as u64)
}

/// Whether this market's trades are already on disk and finished.
fn already_done(dir: &Path, condition: &str) -> bool {
    let sidecar = dir.join(format!("{condition}.meta.json"));
    let Ok(raw) = std::fs::read_to_string(&sidecar) else {
        return false;
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|meta| meta.get("complete").and_then(Value::as_bool))
        .unwrap_or(false)
        && dir.join(format!("{condition}.jsonl")).exists()
}

async fn run(args: &Args) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("tick-to-trade/0.1 (research)")
        .timeout(Duration::from_secs(60))
        .build()?;

    std::fs::create_dir_all(&args.out)?;
    let trades_dir = args.out.join("trades");
    std::fs::create_dir_all(&trades_dir)?;

    tracing::info!(tag = args.tag, "building the catalogue");
    let mut markets = catalogue(&client, args).await?;
    markets.sort_by(|left, right| right.ended.cmp(&left.ended));

    let resolved = markets.iter().filter(|m| m.winner.is_some()).count();
    tracing::info!(
        markets = markets.len(),
        resolved,
        unresolved = markets.len() - resolved,
        "catalogue built"
    );

    let lines: Vec<Value> = markets.iter().map(Market::line).collect();
    let path = args.out.join("markets.jsonl");
    write_lines(&path, &lines)?;
    tracing::info!(path = %path.display(), "catalogue written");

    if args.catalogue_only {
        return Ok(());
    }

    let todo: Vec<&Market> = markets
        .iter()
        .filter(|m| m.winner.is_some())
        .take(args.max_markets.unwrap_or(usize::MAX))
        .collect();

    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut total_trades = 0usize;

    for (index, market) in todo.iter().enumerate() {
        if already_done(&trades_dir, &market.condition) {
            skipped += 1;
            continue;
        }

        match trades(&client, &market.condition, args.pace).await {
            Ok(rows) => {
                let file = trades_dir.join(format!("{}.jsonl", market.condition));
                let bytes = write_lines(&file, &rows)?;
                let first = rows.first().and_then(|r| r.get("t_unix").cloned());
                let last = rows.last().and_then(|r| r.get("t_unix").cloned());
                let meta = json!({
                    "complete": true,
                    "condition_id": market.condition,
                    "slug": market.slug,
                    "event": market.event,
                    "outcomes": market.outcomes,
                    "winner_index": market.winner,
                    "token_ids": market.tokens,
                    "volume": market.volume,
                    "ended": market.ended,
                    "trades": rows.len(),
                    "bytes": bytes,
                    "first_t_unix": first,
                    "last_t_unix": last,
                });
                std::fs::write(
                    trades_dir.join(format!("{}.meta.json", market.condition)),
                    serde_json::to_vec_pretty(&meta)?,
                )?;
                total_trades += rows.len();
                done += 1;
            }
            Err(error) => {
                // One market failing is not a reason to lose the rest; the
                // sidecar is simply not written, so a later run retries it.
                tracing::error!(slug = %market.slug, "{error:#}");
                failed += 1;
            }
        }

        if (index + 1) % 25 == 0 {
            tracing::info!(
                progress = format!("{}/{}", index + 1, todo.len()),
                done,
                skipped,
                failed,
                total_trades,
                "collecting"
            );
        }
        tokio::time::sleep(args.pace).await;
    }

    tracing::info!(done, skipped, failed, total_trades, "collection finished");
    if failed > 0 {
        tracing::warn!(failed, "run it again to retry the markets that failed");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    install_crypto_provider();

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

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn defaults_to_league_of_legends() {
        let parsed = parse_args(args(&[])).unwrap().unwrap();
        assert_eq!(parsed, Args::default());
        assert_eq!(parsed.tag, 65);
        assert_eq!(parsed.out, PathBuf::from("data/history"));
    }

    #[test]
    fn parses_the_knobs() {
        let parsed = parse_args(args(&[
            "--tag",
            "64",
            "--min-volume",
            "500",
            "--catalogue-only",
            "--max-markets",
            "3",
            "--pace-ms",
            "10",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.tag, 64);
        assert_eq!(parsed.min_volume, 500);
        assert!(parsed.catalogue_only);
        assert_eq!(parsed.max_markets, Some(3));
        assert_eq!(parsed.pace, Duration::from_millis(10));
    }

    #[test]
    fn parses_and_orders_the_month_range() {
        let parsed = parse_args(args(&["--from", "2025-10", "--to", "2026-03"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.from, (2025, 10));
        assert_eq!(parsed.to, (2026, 3));
        // A range the wrong way round would walk nothing and report success.
        assert!(parse_args(args(&["--from", "2026-05", "--to", "2026-01"])).is_err());
        assert!(parse_args(args(&["--from", "2026-13"])).is_err());
        assert!(parse_args(args(&["--from", "nope"])).is_err());
    }

    #[test]
    fn months_step_across_the_year_boundary() {
        assert_eq!(next_month((2025, 12)), (2026, 1));
        assert_eq!(previous_month((2026, 1)), (2025, 12));
        assert_eq!(next_month((2026, 3)), (2026, 4));
        assert_eq!(previous_month((2026, 3)), (2026, 2));
    }

    #[test]
    fn the_default_range_covers_the_per_game_era() {
        let parsed = Args::default();
        assert_eq!(parsed.from, (2025, 9));
        // Walking backwards from this month has to reach the start.
        assert!(parsed.to >= parsed.from);
        assert!(parsed.to.0 >= 2026, "clock looks wrong: {:?}", parsed.to);
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_args(args(&["--tag", "lol"])).is_err());
        assert!(parse_args(args(&["--max-markets", "0"])).is_err());
        assert!(parse_args(args(&["--out"])).is_err());
        assert!(parse_args(args(&["--wat"])).is_err());
    }

    #[test]
    fn only_per_game_winner_slugs_are_collected() {
        assert!(is_game_winner("lol-dkc-ktc-2026-09-14-game1"));
        assert!(is_game_winner("lol-dkc-ktc-2026-09-14-game5"));
        // The series market, the props and the handicaps are not games.
        assert!(!is_game_winner("lol-dkc-ktc-2026-09-14"));
        assert!(!is_game_winner("lol-dkc-ktc-2026-09-14-game1-first-blood"));
        assert!(!is_game_winner("lol-dkc-ktc-2026-09-14-total-games-3pt5"));
        assert!(!is_game_winner("lol-dkc-ktc-2026-09-14-game0"));
        assert!(!is_game_winner("lol-x-game12"));
    }

    #[test]
    fn a_resolution_that_is_not_one_and_zero_is_not_a_winner() {
        assert_eq!(winner_of(&["1".into(), "0".into()]), Some(0));
        assert_eq!(winner_of(&["0".into(), "1".into()]), Some(1));
        // Voided, half-resolved or still-open markets carry no label, and a
        // study that guessed one would be scoring its own guess.
        assert_eq!(winner_of(&["0.5".into(), "0.5".into()]), None);
        assert_eq!(winner_of(&["0".into(), "0".into()]), None);
        assert_eq!(winner_of(&["1".into()]), None);
    }

    #[test]
    fn gamma_fields_that_are_json_inside_json_are_unwrapped() {
        let market = json!({"outcomes": "[\"A\", \"B\"]"});
        assert_eq!(nested(&market, "outcomes"), vec!["A", "B"]);
        assert!(nested(&market, "missing").is_empty());
        assert!(nested(&json!({"outcomes": "not json"}), "outcomes").is_empty());
    }

    #[test]
    fn volume_is_read_whether_it_is_a_number_or_a_string() {
        assert_eq!(as_f64(Some(&json!(1234.5))), 1234.5);
        assert_eq!(as_f64(Some(&json!("1234.5"))), 1234.5);
        assert_eq!(as_f64(Some(&json!(null))), 0.0);
        assert_eq!(as_f64(None), 0.0);
    }

    #[test]
    fn props_are_left_out_of_an_event() {
        let event = json!({
            "slug": "lol-a-b-2026-01-01",
            "endDate": "2026-01-01T00:00:00Z",
            "markets": [
                {
                    "slug": "lol-a-b-2026-01-01-game1",
                    "conditionId": "0xabc",
                    "clobTokenIds": "[\"1\", \"2\"]",
                    "outcomes": "[\"A\", \"B\"]",
                    "outcomePrices": "[\"1\", \"0\"]",
                    "volume": 50000.0,
                    "orderPriceMinTickSize": "0.001"
                },
                {
                    "slug": "lol-a-b-2026-01-01-game1-first-blood",
                    "conditionId": "0xdef",
                    "clobTokenIds": "[\"3\", \"4\"]",
                    "outcomes": "[\"A\", \"B\"]",
                    "outcomePrices": "[\"1\", \"0\"]",
                    "volume": 50000.0
                },
                {
                    "slug": "lol-a-b-2026-01-01-game2",
                    "conditionId": "0x123",
                    "clobTokenIds": "[\"5\", \"6\"]",
                    "outcomes": "[\"A\", \"B\"]",
                    "outcomePrices": "[\"0\", \"1\"]",
                    "volume": 10.0
                }
            ]
        });

        let found = game_markets(&event, 10_000.0);
        assert_eq!(found.len(), 1, "only the liquid game winner survives");
        assert_eq!(found[0].condition, "0xabc");
        assert_eq!(found[0].winner, Some(0));
        assert_eq!(found[0].event, "lol-a-b-2026-01-01");
        assert_eq!(found[0].tick.as_deref(), Some("0.001"));
    }
}
