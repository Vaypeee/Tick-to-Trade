//! End-to-end test over a hand-built recording.
//!
//! The unit tests check each stage against inputs written for that stage.
//! This one runs the whole chain — replay, book reconstruction, trade
//! classification, surge detection, edge study — over a single file, so a
//! change that quietly breaks the seam between two stages fails here even
//! when every stage still passes on its own.
//!
//! The fixture is synthetic and says so. It encodes the shape the project is
//! looking for: a burst of buying crosses the ask, the touch jumps, and
//! anybody late buys the top.

use std::path::Path;

use tick_to_trade::{
    analysis::{
        align, book, edge, flow, reference_asset,
        replay::{self, Kind},
    },
    strategy::{
        decision::{Limits, Touch},
        paper::{self, Signal},
    },
};

const FIXTURE: &str = "tests/fixtures/synthetic-market.jsonl";
const HOLD_NS: u64 = 30 * 1_000_000_000;

fn load() -> (Vec<replay::Record>, Vec<align::Quote>, String) {
    let records = replay::read(Path::new(FIXTURE)).expect("fixture should load");
    let (quotes, agreement) = book::replay(&records);

    // The fixture states a touch on every delta, so the rebuild must match it
    // everywhere. Anything less means the reconstruction drifted.
    assert_eq!(agreement.checked, 4);
    assert_eq!(agreement.matched, 4, "book reconstruction disagreed");
    assert_eq!(agreement.skipped, 0);

    let asset = reference_asset(Path::new(FIXTURE)).expect("sidecar names the outcome");
    (records, quotes, asset)
}

#[test]
fn the_recording_is_intact() {
    let records = replay::read(Path::new(FIXTURE)).expect("fixture should load");
    assert!(replay::missing_sequences(&records).is_empty());
    assert!(records.iter().all(|record| record.kind == Kind::Data));
}

#[test]
fn the_touch_is_rebuilt_from_snapshot_and_deltas() {
    let (_, quotes, asset) = load();
    let last = quotes
        .iter()
        .rfind(|quote| quote.asset == asset)
        .expect("the outcome should have quotes");
    assert_eq!(last.bid, 0.49);
    assert_eq!(last.ask, 0.51);
}

#[test]
fn the_burst_is_classified_as_buyers_crossing() {
    let (records, quotes, asset) = load();
    let (trades, sides) = flow::trades(&records, &quotes);
    let mine: Vec<&flow::Trade> = trades.iter().filter(|trade| trade.asset == asset).collect();

    assert_eq!(mine.len(), 6);
    assert!(mine.iter().all(|trade| trade.taker == flow::Taker::Buy));
    // Printing at the ask and reporting BUY: the two readings agree.
    assert_eq!(sides.checked, 6);
    assert_eq!(sides.matched, 6);
    assert_eq!(sides.unclear, 0);
}

#[test]
fn the_burst_is_found_once_with_the_default_threshold() {
    let (records, quotes, asset) = load();
    let (trades, _) = flow::trades(&records, &quotes);

    // Six trades of 1000, and five times the median is 5000. The signal
    // fires on the fifth, the moment the threshold is crossed, rather than
    // waiting for a sixth trade nobody could know was coming. The tail is
    // then quiet, so one burst yields one signal.
    let surges = flow::surges(&trades, &asset, 5_000_000_000, 5_000.0);
    assert_eq!(surges.len(), 1);
    assert_eq!(surges[0].signed_size, 5_000.0);
    assert_eq!(surges[0].trades, 5);
    assert!(surges[0].signal().rose);
}

#[test]
fn acting_promptly_pays_and_acting_late_does_not() {
    let (records, quotes, asset) = load();
    let (trades, _) = flow::trades(&records, &quotes);
    let surges = flow::surges(&trades, &asset, 5_000_000_000, 5_000.0);

    let prompt = edge::study(&surges, &quotes, &asset, 0, HOLD_NS, 0.0);
    assert_eq!(prompt.trials.len(), 1);
    assert_eq!(prompt.truncated, 0);

    // Entering before the touch moved: buy 0.42, close at 0.50.
    assert_eq!(prompt.trials[0].entry, 0.42);
    assert!((prompt.trials[0].net - 0.08).abs() < 1e-9);

    // The same hold ten seconds later runs off the end of the recording, so
    // it is dropped rather than closed at a quote standing in for time
    // nobody observed.
    let past_the_end = edge::study(&surges, &quotes, &asset, 10 * 1_000_000_000, HOLD_NS, 0.0);
    assert!(past_the_end.trials.is_empty());
    assert_eq!(past_the_end.truncated, 1);

    // Shortened so it finishes inside the recording, the late entry loses:
    // buy 0.52, close at 0.50.
    let late = edge::study(
        &surges,
        &quotes,
        &asset,
        10 * 1_000_000_000,
        20 * 1_000_000_000,
        0.0,
    );
    assert_eq!(late.trials.len(), 1);
    assert_eq!(late.trials[0].entry, 0.52);
    assert!(late.trials[0].net < 0.0);
}

#[test]
fn the_edge_decays_as_the_reaction_gets_slower() {
    let (records, quotes, asset) = load();
    let (trades, _) = flow::trades(&records, &quotes);
    let surges = flow::surges(&trades, &asset, 5_000_000_000, 5_000.0);

    let nets: Vec<f64> = edge::REACTION_LADDER_MS
        .iter()
        .map(|delay_ms| {
            edge::study(
                &surges,
                &quotes,
                &asset,
                delay_ms.saturating_mul(1_000_000),
                HOLD_NS,
                0.0,
            )
            .total_net()
        })
        .collect();

    // Never improves with delay, and ends worse than it started. If this ever
    // fails upwards, the entry price is being taken before the fill could
    // have happened.
    assert!(nets.windows(2).all(|pair| pair[0] >= pair[1] - 1e-9));
    assert!(nets[0] > *nets.last().unwrap());
}

#[test]
fn fees_only_ever_make_it_worse() {
    let (records, quotes, asset) = load();
    let (trades, _) = flow::trades(&records, &quotes);
    let surges = flow::surges(&trades, &asset, 5_000_000_000, 5_000.0);

    let free = edge::study(&surges, &quotes, &asset, 0, HOLD_NS, 0.0).total_net();
    let charged = edge::study(&surges, &quotes, &asset, 0, HOLD_NS, 0.002).total_net();
    assert!(charged < free);
}

fn limits() -> Limits {
    Limits {
        min_edge: 0.0,
        max_position: 10.0,
        size: 10.0,
        max_spread: 0.10,
        require_touch_depth: false,
        fee_fraction: 0.0,
        max_signal_age_ns: 1_000_000_000,
    }
}

fn signals_and_touch() -> (Vec<Signal>, Vec<align::Quote>, String) {
    let (records, quotes, asset) = load();
    let (trades, _) = flow::trades(&records, &quotes);
    let surges = flow::surges(&trades, &asset, 5_000_000_000, 5_000.0);
    let signals = surges
        .iter()
        .map(|surge| Signal {
            at_unix_ns: surge.at_unix_ns,
            rose: surge.signed_size > 0.0,
        })
        .collect();
    (signals, quotes, asset)
}

fn touch_at(quotes: &[align::Quote], asset: &str, at: u64) -> Option<Touch> {
    quotes
        .iter()
        .rev()
        .find(|quote| quote.asset == asset && quote.at_unix_ns <= at)
        .map(|quote| Touch {
            bid: quote.bid,
            ask: quote.ask,
            bid_size: quote.bid_size,
            ask_size: quote.ask_size,
        })
}

#[test]
fn the_rules_take_the_burst_and_the_books_balance() {
    let (signals, quotes, asset) = signals_and_touch();
    let touch = |at: u64| touch_at(&quotes, &asset, at);

    let run = paper::run(&signals, touch, 0.10, 0, HOLD_NS, &limits());
    assert_eq!(run.trades.len(), 1);

    let trade = &run.trades[0];
    // Bought the ask before the move, sold the bid after it.
    assert_eq!(trade.entry, 0.42);
    assert_eq!(trade.exit, 0.50);
    assert!((run.net() - (0.50 - 0.42) * 10.0).abs() < 1e-9);
    assert_eq!(run.wins(), 1);
    assert_eq!(run.abandoned, 0);
}

#[test]
fn an_expected_move_below_the_spread_is_refused_rather_than_taken() {
    let (signals, quotes, asset) = signals_and_touch();
    let touch = |at: u64| touch_at(&quotes, &asset, at);

    // The touch is two cents wide, so a one-cent prediction cannot pay for
    // the round trip however right it turns out to be.
    let run = paper::run(&signals, touch, 0.01, 0, HOLD_NS, &limits());
    assert!(run.trades.is_empty());
    assert_eq!(run.refusals.get("edge below cost"), Some(&1));
}

#[test]
fn the_sweep_finds_where_the_rules_start_paying() {
    let (signals, quotes, asset) = signals_and_touch();
    let touch = |at: u64| touch_at(&quotes, &asset, at);

    let ladder = [0.005, 0.01, 0.02, 0.05, 0.10];
    let results = paper::sweep(&signals, touch, &ladder, 0, HOLD_NS, &limits());

    let profitable: Vec<f64> = results
        .iter()
        .filter(|(_, run)| run.net() > 0.0)
        .map(|(expected, _)| *expected)
        .collect();

    // Everything above the crossing point pays, nothing below it does.
    assert!(!profitable.is_empty());
    let crossing = profitable[0];
    assert!(
        results
            .iter()
            .all(|(expected, run)| (*expected < crossing) == (run.net() <= 0.0))
    );
}

#[test]
fn a_late_reaction_is_refused_before_it_can_lose_money() {
    let (signals, quotes, asset) = signals_and_touch();
    let touch = |at: u64| touch_at(&quotes, &asset, at);

    // The staleness rule fires first: ten seconds after the burst the signal
    // is no longer one, whatever the price has done since.
    let late = paper::run(
        &signals,
        touch,
        0.10,
        10 * 1_000_000_000,
        HOLD_NS,
        &limits(),
    );
    assert!(late.trades.is_empty());
    assert_eq!(late.refusals.get("signal too old"), Some(&1));
}

#[test]
fn without_the_staleness_rule_a_late_reaction_buys_the_top() {
    let (signals, quotes, asset) = signals_and_touch();
    let touch = |at: u64| touch_at(&quotes, &asset, at);

    let mut permissive = limits();
    permissive.max_signal_age_ns = u64::MAX;

    let prompt = paper::run(&signals, touch, 0.10, 0, HOLD_NS, &permissive);
    let late = paper::run(
        &signals,
        touch,
        0.10,
        10 * 1_000_000_000,
        HOLD_NS,
        &permissive,
    );

    assert!(prompt.net() > 0.0);
    // Entering after the move and leaving once it faded gives back more than
    // the spread, which is what the staleness rule exists to prevent.
    assert!(late.net() < 0.0);
}
