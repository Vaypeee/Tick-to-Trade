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
    fn percentiles_use_nearest_rank() {
        let sorted = [10u64, 20, 30, 40];
        assert_eq!(percentile(&sorted, 0.0), Some(10));
        assert_eq!(percentile(&sorted, 50.0), Some(20));
        assert_eq!(percentile(&sorted, 100.0), Some(40));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
