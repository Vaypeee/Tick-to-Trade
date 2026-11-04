//! Is the market's price the real probability?
//!
//! The hypothesis this answers is that a near-certain win gets overbought —
//! that a team the market prices at 0.90 wins less than 90% of the time, so
//! the other side at 0.10 is worth more than it costs. That claim is about
//! calibration and nothing else, so it is measured directly: across every
//! resolved game, when a team could be bought at `p`, how often did it win?
//!
//! Two traps decide whether the number means anything.
//!
//! The first is counting time instead of games. A game that sat at 0.10 for
//! forty minutes produced hundreds of trades there, and letting each one
//! count would weight the curve by how long positions lingered rather than by
//! how often they paid. Every game enters a bucket **once**, the first time
//! its price reaches it.
//!
//! The second is the shape of the payoff. Buying at 0.10 and selling at 0.20
//! doubles the stake, which is vivid, and the paths that quietly decayed to
//! zero are invisible next to it. A reversal is not evidence by itself: the
//! question is whether reversals happen more often than the price implied,
//! which is why every entry here is kept, winners and losers alike, and why
//! the exit study reports what each rule paid over all of them.

/// One game, entered the first time its price reached a bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Price of the trade that first reached the bucket.
    pub price: f64,
    /// Highest price the same side traded at afterwards. This is what any
    /// exit rule can be judged against: a target above it was never offered.
    pub max_after: f64,
    /// Whether this side resolved as the winner.
    pub won: bool,
}

/// What one price bucket turned out to be worth.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub lo: f64,
    pub hi: f64,
    /// Games that reached this bucket.
    pub entries: usize,
    /// How many of them went on to win.
    pub wins: usize,
}

impl Bucket {
    /// The probability the price was claiming, taken at the bucket's middle.
    pub fn implied(&self) -> f64 {
        (self.lo + self.hi) / 2.0
    }

    /// The probability the games actually delivered.
    pub fn realised(&self) -> Option<f64> {
        (self.entries > 0).then(|| self.wins as f64 / self.entries as f64)
    }

    /// Realised minus implied, in probability points. Positive means this
    /// side was cheap.
    pub fn edge(&self) -> Option<f64> {
        self.realised().map(|realised| realised - self.implied())
    }

    /// A 95% interval on the realised rate, by the normal approximation.
    ///
    /// Reported because a bucket holding nine games will show a wild rate and
    /// mean nothing by it, and an edge inside its own error bars is not an
    /// edge yet.
    pub fn interval(&self) -> Option<(f64, f64)> {
        let realised = self.realised()?;
        let n = self.entries as f64;
        let half = 1.96 * (realised * (1.0 - realised) / n).sqrt();
        Some(((realised - half).max(0.0), (realised + half).min(1.0)))
    }

    /// Whether the whole interval sits above the implied probability.
    pub fn significant(&self) -> bool {
        self.interval()
            .is_some_and(|(low, high)| low > self.implied() || high < self.implied())
    }
}

/// Which bucket a price falls in.
fn bucket_of(price: f64, width: f64) -> usize {
    let count = (1.0 / width).round() as usize;
    ((price / width).floor() as usize).min(count.saturating_sub(1))
}

/// The number of buckets a width implies.
pub fn bucket_count(width: f64) -> usize {
    (1.0 / width).round().max(1.0) as usize
}

/// Every bucket this price series reached, entered once at its first touch.
///
/// `series` is one side of one game, oldest trade first. `won` says whether
/// that side resolved true.
pub fn first_touches(series: &[f64], won: bool, width: f64) -> Vec<(usize, Entry)> {
    let count = bucket_count(width);
    let mut first: Vec<Option<usize>> = vec![None; count];

    for (index, price) in series.iter().enumerate() {
        let bucket = bucket_of(*price, width);
        if first[bucket].is_none() {
            first[bucket] = Some(index);
        }
    }

    // The highest price still to come, from each point backwards, so every
    // entry's best possible exit is read in one pass rather than rescanned.
    let mut best_ahead = vec![0.0f64; series.len()];
    let mut running = 0.0f64;
    for index in (0..series.len()).rev() {
        running = running.max(series[index]);
        best_ahead[index] = running;
    }

    let mut touches = Vec::new();
    for (bucket, at) in first.iter().enumerate() {
        let Some(at) = *at else { continue };
        touches.push((
            bucket,
            Entry {
                price: series[at],
                max_after: best_ahead[at],
                won,
            },
        ));
    }
    touches
}

/// Collects entries into buckets.
pub fn calibrate(touches: &[(usize, Entry)], width: f64) -> Vec<Bucket> {
    let count = bucket_count(width);
    let mut buckets: Vec<Bucket> = (0..count)
        .map(|index| Bucket {
            lo: index as f64 * width,
            hi: (index + 1) as f64 * width,
            entries: 0,
            wins: 0,
        })
        .collect();

    for (bucket, entry) in touches {
        if let Some(slot) = buckets.get_mut(*bucket) {
            slot.entries += 1;
            if entry.won {
                slot.wins += 1;
            }
        }
    }
    buckets
}

/// The Kelly stake for a binary at `price` believed to be worth `probability`,
/// as a fraction of the bankroll.
///
/// Kelly sizes an edge; it cannot create one. A negative result means the bet
/// is the wrong way round, and the honest answer there is zero rather than a
/// short nobody sized for.
pub fn kelly(probability: f64, price: f64) -> f64 {
    if !(0.0..1.0).contains(&price) || !(0.0..=1.0).contains(&probability) {
        return 0.0;
    }
    let edge = probability - price;
    if edge <= 0.0 {
        return 0.0;
    }
    (edge / (1.0 - price)).clamp(0.0, 1.0)
}

/// What one exit rule paid on one entry, per unit of stake.
///
/// `target` is sold into only if the price actually reached it; otherwise the
/// position is held to resolution and settles at one or nothing. `cost` is
/// charged once, for the round trip, because a position held to resolution
/// pays no exit spread while one sold early does — charging the same either
/// way is the simplification, and it is the pessimistic one for the hold.
pub fn exit_net(entry: &Entry, target: f64, cost: f64) -> f64 {
    let exit = if entry.max_after >= target {
        target
    } else if entry.won {
        1.0
    } else {
        0.0
    };
    exit - entry.price - cost
}

/// Summary of one exit rule over many entries.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rule {
    pub target: f64,
    pub trials: usize,
    /// Entries where the target was reached and the position was sold.
    pub filled: usize,
    /// Entries that were never sold and won at resolution.
    pub rode_to_win: usize,
    pub total_net: f64,
}

impl Rule {
    pub fn mean_net(&self) -> Option<f64> {
        (self.trials > 0).then(|| self.total_net / self.trials as f64)
    }

    /// Mean net as a fraction of the average stake, which is what "x2" talk
    /// is really about: a cent of edge on a ten-cent entry is 10%.
    pub fn roi(&self, average_price: f64) -> Option<f64> {
        (average_price > 0.0)
            .then(|| self.mean_net().map(|mean| mean / average_price))
            .flatten()
    }
}

/// Scores one exit rule across every entry given.
pub fn study_rule(entries: &[&Entry], target: f64, cost: f64) -> Rule {
    let mut rule = Rule {
        target,
        ..Rule::default()
    };
    for entry in entries {
        rule.trials += 1;
        if entry.max_after >= target {
            rule.filled += 1;
        } else if entry.won {
            rule.rode_to_win += 1;
        }
        rule.total_net += exit_net(entry, target, cost);
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(price: f64, max_after: f64, won: bool) -> Entry {
        Entry {
            price,
            max_after,
            won,
        }
    }

    #[test]
    fn a_game_enters_each_bucket_once_however_long_it_lingered() {
        // Thirty trades at a tenth, then a recovery. Counting trades would
        // make this one game look like thirty observations of a tenth.
        let mut series = vec![0.10; 30];
        series.push(0.40);
        let touches = first_touches(&series, true, 0.05);

        let tenth: Vec<&(usize, Entry)> = touches
            .iter()
            .filter(|(bucket, _)| *bucket == bucket_of(0.10, 0.05))
            .collect();
        assert_eq!(tenth.len(), 1, "one entry per bucket, not per trade");
        assert_eq!(tenth[0].1.max_after, 0.40);
    }

    #[test]
    fn the_best_exit_is_what_came_after_the_entry_not_before_it() {
        // The high comes first. An entry at the low must not be credited
        // with a price that had already gone.
        let series = [0.80, 0.05, 0.09];
        let touches = first_touches(&series, false, 0.05);
        let low = touches
            .iter()
            .find(|(bucket, _)| *bucket == bucket_of(0.05, 0.05))
            .expect("the low bucket was reached");
        assert_eq!(low.1.price, 0.05);
        assert_eq!(low.1.max_after, 0.09, "0.80 was already in the past");
    }

    #[test]
    fn buckets_count_entries_and_wins() {
        let touches = vec![
            (1, entry(0.07, 0.5, true)),
            (1, entry(0.09, 0.2, false)),
            (1, entry(0.06, 0.1, false)),
        ];
        let buckets = calibrate(&touches, 0.05);
        assert_eq!(buckets[1].entries, 3);
        assert_eq!(buckets[1].wins, 1);
        // Compared with a tolerance, not with `==`: 0.05 + 0.10 halved is
        // 0.07500000000000001 in binary floating point.
        assert!((buckets[1].implied() - 0.075).abs() < 1e-9);
        let realised = buckets[1].realised().unwrap();
        assert!((realised - 1.0 / 3.0).abs() < 1e-9);
        assert!(buckets[1].edge().unwrap() > 0.0, "cheap at this price");
    }

    #[test]
    fn an_empty_bucket_reports_nothing_rather_than_zero() {
        let buckets = calibrate(&[], 0.05);
        assert_eq!(buckets.len(), 20);
        assert!(buckets[0].realised().is_none());
        assert!(buckets[0].edge().is_none());
        assert!(buckets[0].interval().is_none());
        assert!(!buckets[0].significant());
    }

    #[test]
    fn a_thin_bucket_is_not_significant_however_extreme_it_looks() {
        // Three games, all won, priced at a tenth. The rate is 100% and it
        // means nothing; the interval has to say so.
        let touches: Vec<(usize, Entry)> = (0..3).map(|_| (1, entry(0.07, 1.0, true))).collect();
        let narrow = calibrate(&touches, 0.05);
        assert_eq!(narrow[1].realised(), Some(1.0));

        // The same rate over many games is a different statement.
        let many: Vec<(usize, Entry)> = (0..400).map(|_| (1, entry(0.07, 1.0, true))).collect();
        assert!(calibrate(&many, 0.05)[1].significant());
    }

    #[test]
    fn prices_land_in_the_bucket_that_contains_them() {
        assert_eq!(bucket_of(0.00, 0.05), 0);
        assert_eq!(bucket_of(0.049, 0.05), 0);
        assert_eq!(bucket_of(0.05, 0.05), 1);
        // A price of exactly one belongs to the last bucket, not past it.
        assert_eq!(bucket_of(1.0, 0.05), 19);
        assert_eq!(bucket_of(0.999, 0.001), 999);
        assert_eq!(bucket_count(0.05), 20);
    }

    #[test]
    fn kelly_sizes_an_edge_and_refuses_to_invent_one() {
        // Worth 0.15, costs 0.10: edge of five points on a 0.90 payoff.
        let stake = kelly(0.15, 0.10);
        assert!((stake - 0.05 / 0.90).abs() < 1e-9);
        // No edge, or the wrong way round, is not a small bet. It is none.
        assert_eq!(kelly(0.10, 0.10), 0.0);
        assert_eq!(kelly(0.05, 0.10), 0.0);
        // Nonsense in, zero out, rather than a stake built on it.
        assert_eq!(kelly(0.5, 1.0), 0.0);
        assert_eq!(kelly(2.0, 0.1), 0.0);
    }

    #[test]
    fn an_exit_is_only_taken_at_a_price_the_market_offered() {
        let reached = entry(0.10, 0.25, false);
        // Sold at the target: ten cents in, twenty out, one cent of cost.
        assert!((exit_net(&reached, 0.20, 0.01) - 0.09).abs() < 1e-9);

        // The target was never offered, so the position ran to resolution
        // and this one lost.
        let missed = entry(0.10, 0.15, false);
        assert!((exit_net(&missed, 0.20, 0.01) - -0.11).abs() < 1e-9);

        // The same path, but it won.
        let held = entry(0.10, 0.15, true);
        assert!((exit_net(&held, 0.20, 0.01) - 0.89).abs() < 1e-9);
    }

    #[test]
    fn a_rule_reports_how_it_got_its_result() {
        let a = entry(0.10, 0.30, false);
        let b = entry(0.10, 0.12, true);
        let c = entry(0.10, 0.12, false);
        let entries = [&a, &b, &c];

        let rule = study_rule(&entries, 0.20, 0.0);
        assert_eq!(rule.trials, 3);
        assert_eq!(rule.filled, 1, "only one reached the target");
        assert_eq!(rule.rode_to_win, 1, "one rode it out and won");
        // +0.10 sold, +0.90 resolved, -0.10 lost.
        assert!((rule.total_net - 0.90).abs() < 1e-9);
        assert!((rule.mean_net().unwrap() - 0.30).abs() < 1e-9);
        // Three times the stake, per unit staked.
        assert!((rule.roi(0.10).unwrap() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_rule_over_nothing_reports_nothing() {
        let rule = study_rule(&[], 0.20, 0.01);
        assert_eq!(rule.trials, 0);
        assert!(rule.mean_net().is_none());
        assert!(rule.roi(0.10).is_none());
    }
}
