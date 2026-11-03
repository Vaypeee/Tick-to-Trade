//! Mapping values onto a drawing.
//!
//! Small enough to be obvious and important enough to be wrong quietly: an
//! axis that is off by a pixel is invisible, and an axis that is off by a
//! factor is a chart that lies. Both are cheap to pin down with tests, so
//! they are.

/// A linear map from a domain of values to a range of coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    from: f64,
    to: f64,
    start: f64,
    end: f64,
}

impl Scale {
    /// Builds a scale, widening a degenerate domain rather than dividing by
    /// zero. A flat series is a real thing — a market that did not move —
    /// and it should draw as a flat line, not as a crash.
    pub fn new(from: f64, to: f64, start: f64, end: f64) -> Self {
        let (from, to) = if (to - from).abs() < f64::EPSILON {
            (from - 0.5, to + 0.5)
        } else {
            (from, to)
        };
        Self {
            from,
            to,
            start,
            end,
        }
    }

    pub fn map(&self, value: f64) -> f64 {
        let fraction = (value - self.from) / (self.to - self.from);
        self.start + fraction * (self.end - self.start)
    }

    pub fn domain(&self) -> (f64, f64) {
        (self.from, self.to)
    }

    /// Evenly spaced values across the domain, ends included.
    pub fn ticks(&self, count: usize) -> Vec<f64> {
        if count < 2 {
            return vec![self.from];
        }
        let step = (self.to - self.from) / (count - 1) as f64;
        (0..count)
            .map(|index| self.from + step * index as f64)
            .collect()
    }
}

/// Pads a range by a fraction of its own width, so a line never runs along
/// the edge of its own chart.
pub fn padded(min: f64, max: f64, fraction: f64) -> (f64, f64) {
    let width = (max - min).abs();
    let padding = if width < f64::EPSILON {
        0.01
    } else {
        width * fraction
    };
    (min - padding, max + padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ends_of_the_domain_land_on_the_ends_of_the_range() {
        let scale = Scale::new(0.0, 10.0, 100.0, 200.0);
        assert!((scale.map(0.0) - 100.0).abs() < 1e-9);
        assert!((scale.map(10.0) - 200.0).abs() < 1e-9);
        assert!((scale.map(5.0) - 150.0).abs() < 1e-9);
    }

    #[test]
    fn a_descending_range_flips_the_drawing() {
        // Screen coordinates grow downwards, so a price axis runs backwards.
        let scale = Scale::new(0.0, 1.0, 400.0, 0.0);
        assert!((scale.map(0.0) - 400.0).abs() < 1e-9);
        assert!((scale.map(1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn a_flat_series_draws_down_the_middle_rather_than_dividing_by_zero() {
        let scale = Scale::new(0.5, 0.5, 0.0, 100.0);
        let drawn = scale.map(0.5);
        assert!(drawn.is_finite());
        assert!((drawn - 50.0).abs() < 1e-9);
    }

    #[test]
    fn values_outside_the_domain_extrapolate_rather_than_clamp() {
        // Clamping would silently flatten an outlier into the axis.
        let scale = Scale::new(0.0, 10.0, 0.0, 100.0);
        assert!((scale.map(20.0) - 200.0).abs() < 1e-9);
        assert!((scale.map(-5.0) + 50.0).abs() < 1e-9);
    }

    #[test]
    fn ticks_span_the_domain_including_both_ends() {
        let scale = Scale::new(0.0, 8.0, 0.0, 1.0);
        assert_eq!(scale.ticks(5), vec![0.0, 2.0, 4.0, 6.0, 8.0]);
        assert_eq!(scale.ticks(1), vec![0.0]);
    }

    #[test]
    fn padding_keeps_a_line_off_the_edge() {
        let (low, high) = padded(0.40, 0.50, 0.1);
        assert!(low < 0.40 && high > 0.50);
        assert!((low - 0.39).abs() < 1e-9);
        assert!((high - 0.51).abs() < 1e-9);
    }

    #[test]
    fn padding_a_flat_range_still_gives_it_height() {
        let (low, high) = padded(0.5, 0.5, 0.1);
        assert!(high > low);
    }
}
