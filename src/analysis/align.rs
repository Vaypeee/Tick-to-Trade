//! Lines a sports recording up against a market recording.
//!
//! This is the measurement the whole project is built to make: how long after
//! a feed shows something does the market move. Everything else — the Rust,
//! the event loop, the VPS region — is only worth building if the answer here
//! leaves room for it.
//!
//! Alignment is on wall clock, never on the monotonic clock. Two recorders
//! each start their own monotonic origin, so comparing those across files
//! produces a confident, meaningless number.

/// A mid-price observation reconstructed from the market recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub at_unix_ns: u64,
    pub asset: String,
    /// The touch is kept, not just the mid. Classifying a trade as aggressive
    /// needs to know which side of the spread it crossed, and a mid alone
    /// cannot say.
    pub bid: f64,
    pub ask: f64,
    /// What is resting at the touch. A decision that ignores depth is a
    /// decision to assume any size fills at the quoted price.
    pub bid_size: f64,
    pub ask_size: f64,
    pub mid: f64,
}

/// Something the sports feed was seen to change.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// Earliest instant the change could have happened: the request that
    /// found it went out no earlier than this.
    pub from_unix_ns: u64,
    /// Latest instant it could have happened: we had the new state by now.
    pub at_unix_ns: u64,
    pub label: String,
}

/// A market move large enough to count as a reaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub at_unix_ns: u64,
    pub asset: String,
    pub from: f64,
    pub to: f64,
}

/// An observation matched to the market move that followed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Lag {
    pub observation: Observation,
    pub reaction: Move,
    /// Shortest the true lag can be, measured from the moment we knew.
    pub lag_min_ns: u64,
    /// Longest it can be, measured from the earliest the change could
    /// have happened. The spread between the two is the price of polling.
    pub lag_max_ns: u64,
}

/// The first move of at least `min_move` after `after_unix_ns`.
///
/// The baseline is the last quote at or before that instant, not the first
/// quote after it: the question is how far the price travelled from where it
/// stood when the feed showed the change, and a market that had already begun
/// moving should not have that movement discounted.
pub fn first_move_after(
    quotes: &[Quote],
    after_unix_ns: u64,
    min_move: f64,
    horizon_ns: u64,
) -> Option<Move> {
    let baseline = quotes
        .iter()
        .rev()
        .find(|quote| quote.at_unix_ns <= after_unix_ns)?;
    let deadline = after_unix_ns.saturating_add(horizon_ns);

    quotes
        .iter()
        .filter(|quote| quote.at_unix_ns > after_unix_ns && quote.at_unix_ns <= deadline)
        .filter(|quote| quote.asset == baseline.asset)
        .find(|quote| {
            (super::ticks(quote.mid) - super::ticks(baseline.mid)).abs() >= super::ticks(min_move)
        })
        .map(|quote| Move {
            at_unix_ns: quote.at_unix_ns,
            asset: quote.asset.clone(),
            from: baseline.mid,
            to: quote.mid,
        })
}

/// The outcome of matching observations against market moves.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Matching {
    /// One lag per market move, credited to the nearest observation before it.
    pub lags: Vec<Lag>,
    /// Observations that drew no qualifying move inside the horizon.
    ///
    /// These are not lag-zero. Counting them as instant reactions would
    /// invent an edge, so they are reported and excluded.
    pub ignored: usize,
    /// Observations whose move was claimed by a later, nearer observation.
    ///
    /// A feed that changes often puts many observations in front of a single
    /// market move. Letting each of them claim that one move would turn one
    /// event into a whole distribution of lags, all of it fictional, and the
    /// percentiles computed from it would look perfectly respectable.
    pub contested: usize,
}

/// Matches observations to the market moves that followed them.
///
/// Each move is credited to exactly one observation: the last one before it.
/// This is the conservative reading — the most recent thing the feed showed
/// is the likeliest cause — and it is what keeps a noisy feed from
/// manufacturing agreement.
pub fn match_observations(
    observations: &[Observation],
    quotes: &[Quote],
    min_move: f64,
    horizon_ns: u64,
) -> Matching {
    let mut ordered: Vec<&Observation> = observations.iter().collect();
    ordered.sort_by_key(|observation| observation.at_unix_ns);

    let mut best: Vec<Lag> = Vec::new();
    let mut ignored = 0;
    let mut contested = 0;

    for observation in ordered {
        let Some(reaction) = first_move_after(quotes, observation.at_unix_ns, min_move, horizon_ns)
        else {
            ignored += 1;
            continue;
        };
        let lag = Lag {
            lag_min_ns: reaction.at_unix_ns.saturating_sub(observation.at_unix_ns),
            lag_max_ns: reaction.at_unix_ns.saturating_sub(observation.from_unix_ns),
            observation: observation.clone(),
            reaction,
        };
        // Observations arrive in time order, so a later one is always nearer
        // to the move it shares and simply takes it over.
        match best
            .iter_mut()
            .find(|held| held.reaction.at_unix_ns == lag.reaction.at_unix_ns)
        {
            Some(held) => {
                contested += 1;
                *held = lag;
            }
            None => best.push(lag),
        }
    }

    best.sort_by_key(|lag| lag.reaction.at_unix_ns);
    Matching {
        lags: best,
        ignored,
        contested,
    }
}

/// Every move of at least `min_move` in one outcome, in time order.
///
/// Extracted from the price series alone, with no knowledge of the labels.
/// That independence is the point: a move set chosen while looking at the
/// events it will be compared against is not evidence of anything.
///
/// The baseline resets to each move as it is emitted, so a slow drift of many
/// small steps registers once per `min_move` travelled rather than on every
/// tick above the threshold.
pub fn significant_moves(quotes: &[Quote], asset: &str, min_move: f64) -> Vec<Move> {
    let mut moves = Vec::new();
    let mut baseline: Option<f64> = None;

    for quote in quotes.iter().filter(|quote| quote.asset == asset) {
        let Some(previous) = baseline else {
            baseline = Some(quote.mid);
            continue;
        };
        if (super::ticks(quote.mid) - super::ticks(previous)).abs() >= super::ticks(min_move) {
            moves.push(Move {
                at_unix_ns: quote.at_unix_ns,
                asset: quote.asset.clone(),
                from: previous,
                to: quote.mid,
            });
            baseline = Some(quote.mid);
        }
    }
    moves
}

/// The move closest in time to `at_unix_ns`, within `window_ns` either side.
///
/// Deliberately symmetric. Looking only backwards would find a lead whenever
/// the market happened to move at all, and report nothing when it moved after
/// — which is itself the result that would sink the hypothesis.
pub fn nearest_signal(signals: &[Signal], at_unix_ns: u64, window_ns: u64) -> Option<&Signal> {
    signals
        .iter()
        .map(|candidate| (offset_ns(candidate.at_unix_ns, at_unix_ns), candidate))
        .filter(|(offset, _)| offset.unsigned_abs() <= window_ns)
        .min_by_key(|(offset, _)| offset.abs())
        .map(|(_, candidate)| candidate)
}

/// Signed nanoseconds from `reference` to `point`; negative means earlier.
pub fn offset_ns(point: u64, reference: u64) -> i64 {
    point as i64 - reference as i64
}

/// Something in the market that might be a reaction, reduced to the three
/// things the comparison needs: when it happened, which way it pointed, and
/// how to print it.
///
/// Both a mid-price move and a burst of aggressive trading become one of
/// these, so the two can be measured against the spectator with the same
/// code and read off the same table.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub at_unix_ns: u64,
    /// True when the signal points at the first outcome becoming likelier.
    pub rose: bool,
    pub detail: String,
}

impl Move {
    pub fn signal(&self) -> Signal {
        Signal {
            at_unix_ns: self.at_unix_ns,
            rose: super::ticks(self.to) > super::ticks(self.from),
            detail: format!("{:.3} -> {:.3}", self.from, self.to),
        }
    }
}

/// One press matched to the signal nearest it.
#[derive(Debug, Clone, PartialEq)]
pub struct Lead {
    pub label: String,
    pub pressed_unix_ns: u64,
    /// Negative when the market moved before the press: that is the advantage.
    pub offset_ns: i64,
    /// Whether the signal pointed the way the event should have pushed the
    /// price. A market moving the wrong way is noise, not a reaction.
    pub agrees: bool,
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Leads {
    pub leads: Vec<Lead>,
    /// Presses with no qualifying move anywhere near them. The market simply
    /// did not react, and these are reported rather than quietly dropped.
    pub unmatched: usize,
}

/// Matches each press to the nearest significant move.
///
/// `direction` is the way the labelled event should push this outcome: `up`,
/// `down`, or anything else for neutral, which agrees with any move.
pub fn leads(presses: &[(u64, String, String)], signals: &[Signal], window_ns: u64) -> Leads {
    let mut found = Vec::new();
    let mut unmatched = 0;

    for (pressed, label, direction) in presses {
        let Some(candidate) = nearest_signal(signals, *pressed, window_ns) else {
            unmatched += 1;
            continue;
        };
        let agrees = match direction.as_str() {
            "up" => candidate.rose,
            "down" => !candidate.rose,
            _ => true,
        };
        found.push(Lead {
            label: label.clone(),
            pressed_unix_ns: *pressed,
            offset_ns: offset_ns(candidate.at_unix_ns, *pressed),
            agrees,
            detail: candidate.detail.clone(),
        });
    }

    Leads {
        leads: found,
        unmatched,
    }
}

/// Convenience for the common case of comparing against price moves.
pub fn move_signals(moves: &[Move]) -> Vec<Signal> {
    moves.iter().map(Move::signal).collect()
}

/// Nearest-rank percentile of an already sorted slice.
pub fn percentile(sorted: &[u64], percent: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percent / 100.0 * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(at: u64, mid: f64) -> Quote {
        Quote {
            at_unix_ns: at,
            asset: "A".into(),
            bid: mid - 0.005,
            ask: mid + 0.005,
            bid_size: 1_000.0,
            ask_size: 1_000.0,
            mid,
        }
    }

    fn quotes() -> Vec<Quote> {
        vec![
            quote(100, 0.50),
            quote(200, 0.50),
            quote(300, 0.51),
            quote(400, 0.60),
            quote(500, 0.61),
        ]
    }

    #[test]
    fn finds_the_first_move_past_the_threshold() {
        let found = first_move_after(&quotes(), 200, 0.05, 1_000).unwrap();
        assert_eq!(found.at_unix_ns, 400);
        assert_eq!(found.from, 0.50);
        assert_eq!(found.to, 0.60);
    }

    #[test]
    fn small_moves_do_not_count_as_a_reaction() {
        assert!(first_move_after(&quotes(), 200, 0.5, 1_000).is_none());
    }

    #[test]
    fn a_move_past_the_horizon_does_not_count() {
        assert!(first_move_after(&quotes(), 200, 0.05, 100).is_none());
    }

    #[test]
    fn the_baseline_is_the_price_when_the_feed_showed_the_change() {
        // At t=250 the last known price is the 0.50 from t=200, so the move to
        // 0.60 counts as 0.10 rather than the 0.09 from the t=300 quote.
        let found = first_move_after(&quotes(), 250, 0.10, 1_000).unwrap();
        assert_eq!(found.from, 0.50);
        assert_eq!(found.at_unix_ns, 400);
    }

    #[test]
    fn without_a_prior_quote_there_is_no_baseline_to_measure_from() {
        assert!(first_move_after(&quotes(), 50, 0.01, 1_000).is_none());
    }

    fn observation(from: u64, at: u64, label: &str) -> Observation {
        Observation {
            from_unix_ns: from,
            at_unix_ns: at,
            label: label.into(),
        }
    }

    #[test]
    fn polling_uncertainty_becomes_a_range_not_a_point() {
        let found = [observation(150, 200, "score")];
        let matched = match_observations(&found, &quotes(), 0.05, 1_000);
        assert_eq!(matched.lags.len(), 1);
        assert_eq!(matched.lags[0].lag_min_ns, 200);
        assert_eq!(matched.lags[0].lag_max_ns, 250);
        assert_eq!(matched.ignored, 0);
        assert_eq!(matched.contested, 0);
    }

    #[test]
    fn observations_the_market_ignored_are_counted_not_treated_as_zero() {
        let found = [observation(200, 200, "nothing happened")];
        let matched = match_observations(&found, &quotes(), 0.9, 1_000);
        assert!(matched.lags.is_empty());
        assert_eq!(matched.ignored, 1);
    }

    #[test]
    fn one_move_cannot_be_claimed_by_several_observations() {
        // Three noisy feed changes all sit in front of the single move at
        // t=400. Only the nearest may keep it, or one event becomes three.
        let found = [
            observation(210, 210, "noise"),
            observation(220, 220, "noise"),
            observation(290, 290, "the real one"),
        ];
        let matched = match_observations(&found, &quotes(), 0.05, 1_000);
        assert_eq!(matched.lags.len(), 1);
        assert_eq!(matched.lags[0].observation.label, "the real one");
        assert_eq!(matched.lags[0].lag_min_ns, 110);
        assert_eq!(matched.contested, 2);
        assert_eq!(matched.ignored, 0);
    }

    #[test]
    fn distinct_moves_keep_distinct_claimants() {
        let quotes = vec![
            quote(100, 0.50),
            quote(200, 0.60),
            quote(300, 0.60),
            quote(400, 0.70),
        ];
        let found = [
            observation(100, 100, "first"),
            observation(300, 300, "second"),
        ];
        let matched = match_observations(&found, &quotes, 0.05, 1_000);
        assert_eq!(matched.lags.len(), 2);
        assert_eq!(matched.contested, 0);
    }

    #[test]
    fn moves_are_extracted_without_reference_to_any_event() {
        let series = vec![
            quote(100, 0.50),
            quote(200, 0.51),
            quote(300, 0.53),
            quote(400, 0.52),
            quote(500, 0.58),
        ];
        let found = significant_moves(&series, "A", 0.02);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].at_unix_ns, 300);
        assert_eq!(found[1].at_unix_ns, 500);
        // The baseline resets, so the second move is measured from 0.53.
        assert_eq!(found[1].from, 0.53);
    }

    #[test]
    fn moves_in_another_outcome_are_ignored() {
        let mut series = vec![quote(100, 0.50), quote(200, 0.90), quote(300, 0.53)];
        series[1].asset = "B".into();
        let found = significant_moves(&series, "A", 0.02);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].at_unix_ns, 300);
    }

    #[test]
    fn the_nearest_move_can_be_on_either_side_of_the_press() {
        let moves = significant_moves(
            &[quote(100, 0.50), quote(200, 0.60), quote(400, 0.70)],
            "A",
            0.05,
        );
        let signals = move_signals(&moves);
        // Press at 250: the move at 200 is 50 ns back, the one at 400 is 150 on.
        assert_eq!(
            nearest_signal(&signals, 250, 1_000).unwrap().at_unix_ns,
            200
        );
        // Press at 380: the later move is nearer, so the book followed.
        assert_eq!(
            nearest_signal(&signals, 380, 1_000).unwrap().at_unix_ns,
            400
        );
        assert!(nearest_signal(&signals, 5_000, 100).is_none());
    }

    #[test]
    fn a_book_that_moved_first_shows_a_negative_offset() {
        let moves = significant_moves(&[quote(100, 0.50), quote(200, 0.60)], "A", 0.05);
        let presses = [(250u64, "blue_fight".to_string(), "up".to_string())];
        let report = leads(&presses, &move_signals(&moves), 1_000);
        assert_eq!(report.leads.len(), 1);
        assert_eq!(report.leads[0].offset_ns, -50);
        assert!(report.leads[0].agrees);
    }

    #[test]
    fn a_move_the_wrong_way_does_not_agree_with_the_event() {
        let moves = significant_moves(&[quote(100, 0.60), quote(200, 0.50)], "A", 0.05);
        let down = [(250u64, "blue_fight".to_string(), "up".to_string())];
        assert!(!leads(&down, &move_signals(&moves), 1_000).leads[0].agrees);
        let up = [(250u64, "red_fight".to_string(), "down".to_string())];
        assert!(leads(&up, &move_signals(&moves), 1_000).leads[0].agrees);
        let neutral = [(250u64, "notable".to_string(), "neutral".to_string())];
        assert!(leads(&neutral, &move_signals(&moves), 1_000).leads[0].agrees);
    }

    #[test]
    fn presses_the_market_ignored_are_counted() {
        let moves = significant_moves(&[quote(100, 0.50), quote(200, 0.60)], "A", 0.05);
        let presses = [(9_000u64, "blue_fight".to_string(), "up".to_string())];
        let report = leads(&presses, &move_signals(&moves), 100);
        assert!(report.leads.is_empty());
        assert_eq!(report.unmatched, 1);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted = [10u64, 20, 30, 40];
        assert_eq!(percentile(&sorted, 0.0), Some(10));
        assert_eq!(percentile(&sorted, 50.0), Some(20));
        assert_eq!(percentile(&sorted, 100.0), Some(40));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
