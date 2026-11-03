//! Draws a recording as a timeline.
//!
//! The question this project keeps asking is whether the market moved before
//! somebody could have known. A table of offsets answers it; a timeline shows
//! it, which is a different and sometimes more honest thing — a lead that
//! only exists because of one outlier looks convincing in a percentile and
//! obvious in a picture.
//!
//! The output is one self-contained HTML file with an inline SVG. No
//! dependencies, no runtime, nothing to serve: open it, or commit it next to
//! the recording it describes.

use std::fmt::Write as _;

use super::scale::{Scale, padded};

const WIDTH: f64 = 1240.0;
const HEIGHT: f64 = 470.0;
const LEFT: f64 = 58.0;
const RIGHT: f64 = 18.0;
const TOP: f64 = 18.0;
const BOTTOM: f64 = 38.0;

/// A point on the book, already reduced to what the drawing needs.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub at_unix_ns: u64,
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
}

/// A trade that crossed, and which way.
#[derive(Debug, Clone, Copy)]
pub struct Cross {
    pub at_unix_ns: u64,
    pub price: f64,
    pub bought: bool,
}

/// A burst of one-sided taking.
#[derive(Debug, Clone, Copy)]
pub struct Burst {
    pub at_unix_ns: u64,
    pub rose: bool,
}

/// A moment somebody with no advantage knew something.
#[derive(Debug, Clone)]
pub struct Press {
    pub at_unix_ns: u64,
    pub label: String,
    /// `None` for an event that should not push the price either way.
    pub rose: Option<bool>,
}

pub struct Timeline<'a> {
    pub title: String,
    pub subtitle: String,
    pub points: &'a [Point],
    pub crosses: &'a [Cross],
    pub bursts: &'a [Burst],
    pub presses: &'a [Press],
    /// Headline figures, shown as cards above the chart.
    pub cards: Vec<(String, String)>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Turns a series into a staircase.
///
/// A quote holds until the next one replaces it, so joining two quotes with a
/// sloped line draws prices the book never showed. At this density it would
/// look tidier and be wrong.
fn staircase(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(points.len() * 2);
    for (index, (x, y)) in points.iter().copied().enumerate() {
        if index > 0 {
            out.push((x, points[index - 1].1));
        }
        out.push((x, y));
    }
    out
}

fn path(points: &[(f64, f64)]) -> String {
    let mut out = String::with_capacity(points.len() * 16);
    for (index, (x, y)) in points.iter().enumerate() {
        let _ = write!(out, "{}{x:.1},{y:.1}", if index == 0 { "M" } else { " L" });
    }
    out
}

/// Minutes and seconds from the start of the recording.
///
/// Takes the raw axis value rather than a difference, because an axis is
/// padded past its own domain: subtracting the origin in unsigned arithmetic
/// wraps on the ticks that fall before the first quote, and prints a time
/// four centuries long instead of a negative one.
fn seconds_label(value: f64, origin_ns: u64) -> String {
    let from_start = (value - origin_ns as f64).max(0.0);
    let total = (from_start / 1e9) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Renders the page.
pub fn render(timeline: &Timeline) -> String {
    let (plot_w, plot_h) = (WIDTH - LEFT - RIGHT, HEIGHT - TOP - BOTTOM);

    // One point is not a timeline. Drawing it anyway produces an axis with
    // no width and a chart implying a window the recording never covered.
    if timeline.points.len() < 2 {
        return page(
            timeline,
            &format!(
                "<p class=\"empty\">Only {} quote(s) for this outcome — not enough to draw                  a timeline. Either the market did not move during the recording, or the                  wrong outcome was chosen.</p>",
                timeline.points.len()
            ),
        );
    }
    let first = &timeline.points[0];
    let last = &timeline.points[timeline.points.len() - 1];

    let x = Scale::new(
        first.at_unix_ns as f64,
        last.at_unix_ns as f64,
        LEFT,
        LEFT + plot_w,
    );

    let low = timeline
        .points
        .iter()
        .map(|point| point.bid)
        .fold(f64::INFINITY, f64::min);
    let high = timeline
        .points
        .iter()
        .map(|point| point.ask)
        .fold(f64::NEG_INFINITY, f64::max);
    let (low, high) = padded(low, high, 0.12);
    let y = Scale::new(low, high, TOP + plot_h, TOP);

    let at = |nanos: u64| x.map(nanos as f64);
    let mut svg = String::with_capacity(16_384);

    // Axes first, so everything else sits on top of them.
    for value in y.ticks(5) {
        let line_y = y.map(value);
        let _ = write!(
            svg,
            r##"<line x1="{LEFT}" y1="{line_y:.1}" x2="{:.1}" y2="{line_y:.1}" class="grid"/>"##,
            LEFT + plot_w
        );
        let _ = write!(
            svg,
            r##"<text x="{:.1}" y="{:.1}" class="axis" text-anchor="end">{value:.3}</text>"##,
            LEFT - 8.0,
            line_y + 3.5
        );
    }
    for value in x.ticks(7) {
        let line_x = x.map(value);
        let _ = write!(
            svg,
            r##"<text x="{line_x:.1}" y="{:.1}" class="axis" text-anchor="middle">{}</text>"##,
            HEIGHT - 14.0,
            seconds_label(value, first.at_unix_ns)
        );
    }

    // The spread, as the band between bid and ask.
    let asks: Vec<(f64, f64)> = timeline
        .points
        .iter()
        .map(|point| (at(point.at_unix_ns), y.map(point.ask)))
        .collect();
    let bids: Vec<(f64, f64)> = timeline
        .points
        .iter()
        .map(|point| (at(point.at_unix_ns), y.map(point.bid)))
        .collect();
    let mut band = staircase(&asks);
    let mut back = staircase(&bids);
    back.reverse();
    band.extend(back);
    let _ = write!(svg, r##"<path d="{} Z" class="spread"/>"##, path(&band));

    // The mid.
    let mids: Vec<(f64, f64)> = timeline
        .points
        .iter()
        .map(|point| (at(point.at_unix_ns), y.map(point.mid)))
        .collect();
    let _ = write!(
        svg,
        r##"<path d="{}" class="mid" fill="none"/>"##,
        path(&staircase(&mids))
    );

    // Trades that crossed.
    for cross in timeline.crosses {
        let _ = write!(
            svg,
            r##"<circle cx="{:.1}" cy="{:.1}" r="2.6" class="{}"/>"##,
            at(cross.at_unix_ns),
            y.map(cross.price),
            if cross.bought { "took-ask" } else { "took-bid" }
        );
    }

    // Bursts of one-sided taking.
    for burst in timeline.bursts {
        let burst_x = at(burst.at_unix_ns);
        let class = if burst.rose { "burst-up" } else { "burst-down" };
        let _ = write!(
            svg,
            r##"<line x1="{burst_x:.1}" y1="{TOP}" x2="{burst_x:.1}" y2="{:.1}" class="{class}"/>"##,
            TOP + plot_h
        );
        let tip = if burst.rose { TOP + 4.0 } else { TOP + 12.0 };
        let base = if burst.rose { TOP + 12.0 } else { TOP + 4.0 };
        let _ = write!(
            svg,
            r##"<polygon points="{burst_x:.1},{tip:.1} {:.1},{base:.1} {:.1},{base:.1}" class="{class}-fill"/>"##,
            burst_x - 4.5,
            burst_x + 4.5
        );
    }

    // What a spectator knew, and when.
    for press in timeline.presses {
        let press_x = at(press.at_unix_ns);
        let class = match press.rose {
            Some(true) => "press-up",
            Some(false) => "press-down",
            None => "press",
        };
        let _ = write!(
            svg,
            r##"<line x1="{press_x:.1}" y1="{TOP}" x2="{press_x:.1}" y2="{:.1}" class="{class}"/>"##,
            TOP + plot_h
        );
        let _ = write!(
            svg,
            r##"<text x="{:.1}" y="{:.1}" class="press-label" transform="rotate(-90 {:.1} {:.1})">{}</text>"##,
            press_x - 4.0,
            TOP + plot_h - 6.0,
            press_x - 4.0,
            TOP + plot_h - 6.0,
            escape(&press.label)
        );
    }

    let figure = format!(
        r##"<svg viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-label="{}">{svg}</svg>"##,
        escape(&timeline.subtitle)
    );
    page(timeline, &figure)
}

fn page(timeline: &Timeline, figure: &str) -> String {
    let cards: String = timeline
        .cards
        .iter()
        .map(|(key, value)| {
            format!(
                r##"<div class="card"><div class="n">{}</div><div class="k">{}</div></div>"##,
                escape(value),
                escape(key)
            )
        })
        .collect();

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<script>
  if (new URLSearchParams(location.search).get('theme') === 'dark') {{
    document.documentElement.dataset.theme = 'dark';
  }}
</script>
<style>
  :root {{
    --bg:#0d1117; --panel:#161b22; --line:#30363d; --fg:#e6edf3; --muted:#8b949e;
    --mid:#58a6ff; --spread:#1f6feb28; --up:#3fb950; --down:#f85149; --press:#d29922;
    --mono: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }}
  @media (prefers-color-scheme: light) {{
    :root:not([data-theme="dark"]) {{
      --bg:#ffffff; --panel:#f6f8fa; --line:#d0d7de; --fg:#1f2328; --muted:#656d76;
      --mid:#0969da; --spread:#0969da22; --up:#1a7f37; --down:#cf222e; --press:#9a6700;
    }}
  }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; background:var(--bg); color:var(--fg);
    font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif; }}
  .wrap {{ max-width:1320px; margin:0 auto; padding:26px 16px 40px; }}
  h1 {{ font-size:19px; margin:0 0 4px; }}
  .sub {{ color:var(--muted); font-size:13px; margin:0 0 20px; }}
  .cards {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr));
    background:var(--panel); border:1px solid var(--line); border-radius:10px; margin-bottom:18px; }}
  .card {{ padding:14px 18px; border-right:1px solid var(--line); }}
  .card:last-child {{ border-right:0; }}
  .card .n {{ font-family:var(--mono); font-size:19px; }}
  .card .k {{ font-size:11px; color:var(--muted); text-transform:uppercase;
    letter-spacing:.06em; margin-top:3px; }}
  figure {{ margin:0; background:var(--panel); border:1px solid var(--line);
    border-radius:10px; padding:10px; }}
  svg {{ width:100%; height:auto; display:block; }}
  .grid {{ stroke:var(--line); stroke-width:1; }}
  .axis {{ fill:var(--muted); font-size:10.5px; font-family:var(--mono); }}
  .spread {{ fill:var(--spread); stroke:none; }}
  .mid {{ stroke:var(--mid); stroke-width:1.8; }}
  .took-ask {{ fill:var(--up); }}
  .took-bid {{ fill:var(--down); }}
  .burst-up, .burst-down {{ stroke-width:1.2; stroke-dasharray:3 3; }}
  .burst-up {{ stroke:var(--up); }}
  .burst-down {{ stroke:var(--down); }}
  .burst-up-fill {{ fill:var(--up); }}
  .burst-down-fill {{ fill:var(--down); }}
  .press, .press-up, .press-down {{ stroke:var(--press); stroke-width:1.6; }}
  .press-label {{ fill:var(--press); font-size:10px; font-family:var(--mono);
    text-anchor:start; }}
  .legend {{ display:flex; gap:20px; flex-wrap:wrap; margin-top:14px;
    font-size:12px; color:var(--muted); }}
  .legend span {{ display:inline-flex; align-items:center; gap:7px; }}
  .swatch {{ width:13px; height:3px; border-radius:2px; display:inline-block; }}
  .dot {{ width:8px; height:8px; border-radius:50%; display:inline-block; }}
  .empty {{ color:var(--muted); font-size:13px; padding:28px; }}
  footer {{ margin-top:20px; color:var(--muted); font-size:12px; line-height:1.6; }}
</style>
</head>
<body>
<div class="wrap">
  <h1>{title}</h1>
  <p class="sub">{subtitle}</p>
  <div class="cards">{cards}</div>
  <figure>{figure}</figure>
  <div class="legend">
    <span><i class="swatch" style="background:var(--mid)"></i>mid</span>
    <span><i class="swatch" style="background:var(--spread);height:9px"></i>spread</span>
    <span><i class="dot" style="background:var(--up)"></i>trade lifting the ask</span>
    <span><i class="dot" style="background:var(--down)"></i>trade hitting the bid</span>
    <span><i class="swatch" style="background:var(--up)"></i>buying surge</span>
    <span><i class="swatch" style="background:var(--down)"></i>selling surge</span>
    <span><i class="swatch" style="background:var(--press)"></i>spectator press</span>
  </div>
  <footer>
    Prices are drawn as a staircase: a quote holds until the next replaces it,
    so a sloped line would show prices the book never had. The horizontal axis
    is time from the first quote, in minutes and seconds.
  </footer>
</div>
</body>
</html>
"##,
        title = escape(&timeline.title),
        subtitle = escape(&timeline.subtitle),
        cards = cards,
        figure = figure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(at: u64, bid: f64, ask: f64) -> Point {
        Point {
            at_unix_ns: at,
            bid,
            ask,
            mid: (bid + ask) / 2.0,
        }
    }

    fn timeline(points: &[Point]) -> Timeline<'_> {
        Timeline {
            title: "Test".into(),
            subtitle: "A recording".into(),
            points,
            crosses: &[],
            bursts: &[],
            presses: &[],
            cards: vec![("quotes".into(), "2".into())],
        }
    }

    #[test]
    fn a_quote_holds_until_the_next_one_replaces_it() {
        let stepped = staircase(&[(0.0, 10.0), (5.0, 20.0)]);
        // The corner is at the new time and the old price.
        assert_eq!(stepped, vec![(0.0, 10.0), (5.0, 10.0), (5.0, 20.0)]);
    }

    #[test]
    fn a_single_point_needs_no_step() {
        assert_eq!(staircase(&[(1.0, 2.0)]), vec![(1.0, 2.0)]);
        assert!(staircase(&[]).is_empty());
    }

    #[test]
    fn a_path_starts_with_a_move_and_continues_with_lines() {
        let drawn = path(&[(0.0, 1.0), (2.0, 3.0)]);
        assert!(drawn.starts_with("M0.0,1.0"));
        assert!(drawn.contains(" L2.0,3.0"));
    }

    #[test]
    fn an_empty_recording_says_so_instead_of_drawing_nothing() {
        let rendered = render(&timeline(&[]));
        assert!(rendered.contains("not enough to draw"));
        assert!(!rendered.contains("<svg"));
    }

    #[test]
    fn a_single_quote_is_not_a_timeline() {
        let rendered = render(&timeline(&[point(0, 0.40, 0.42)]));
        assert!(rendered.contains("not enough to draw"));
        assert!(!rendered.contains("<svg"));
    }

    #[test]
    fn ticks_before_the_first_quote_do_not_wrap_into_the_distant_future() {
        // An axis is padded past its own domain, so a tick can land before
        // the origin. In unsigned arithmetic that used to print 734:33.
        assert_eq!(seconds_label(0.0, 1_000_000_000), "0:00");
        assert_eq!(seconds_label(-5e9, 0), "0:00");
        assert_eq!(seconds_label(65e9, 0), "1:05");
        assert_eq!(seconds_label(70e9, 5_000_000_000), "1:05");
    }

    #[test]
    fn a_recording_draws_its_spread_and_its_mid() {
        let points = [point(0, 0.40, 0.42), point(1_000_000_000, 0.45, 0.47)];
        let rendered = render(&timeline(&points));
        assert!(rendered.contains("class=\"spread\""));
        assert!(rendered.contains("class=\"mid\""));
        assert!(rendered.contains("<svg"));
    }

    #[test]
    fn labels_cannot_inject_markup() {
        let points = [point(0, 0.4, 0.42), point(1_000, 0.4, 0.42)];
        let presses = [Press {
            at_unix_ns: 500,
            label: "<script>alert(1)</script>".into(),
            rose: Some(true),
        }];
        let mut timeline = timeline(&points);
        timeline.presses = &presses;
        let rendered = render(&timeline);
        assert!(!rendered.contains("<script>alert"));
        assert!(rendered.contains("&lt;script&gt;"));
    }

    #[test]
    fn both_sides_of_a_trade_are_drawn_differently() {
        let points = [point(0, 0.4, 0.42), point(1_000, 0.4, 0.42)];
        let crosses = [
            Cross {
                at_unix_ns: 200,
                price: 0.42,
                bought: true,
            },
            Cross {
                at_unix_ns: 600,
                price: 0.40,
                bought: false,
            },
        ];
        let mut timeline = timeline(&points);
        timeline.crosses = &crosses;
        let rendered = render(&timeline);
        assert!(rendered.contains("took-ask"));
        assert!(rendered.contains("took-bid"));
    }

    #[test]
    fn the_time_axis_counts_from_the_first_quote() {
        assert_eq!(seconds_label(0.0, 0), "0:00");
        assert_eq!(seconds_label(65e9, 0), "1:05");
    }
}
