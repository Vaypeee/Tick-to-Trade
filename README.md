# Tick to trade

> **Status: early development.** Both recorders work and are in use. Nothing
> downstream of them exists yet, and no end-to-end latency numbers have been
> measured — every table below marked *not measured* is a placeholder, not a
> claim.

A low-latency, event-driven inference pipeline in Rust, built to test one
hypothesis: during live sports events, Polymarket prices lag behind real-world
events long enough for a fast system to act before the market reprices.

This is an educational project. The goal is to design an infrastructure, build
it, measure it, and then validate or reject the hypothesis with data. It runs in
paper mode by default and places no real orders.

## 1. Hypothesis

When something happens in a live game (a goal, a break point, a timeout), human
traders on Polymarket take several seconds to react. If a system can

1. detect the event from a live data feed,
2. estimate the new fair probability,
3. send an order to the CLOB,

faster than the market reprices, there is a measurable edge window.

The questions this project is meant to answer:

- How long does the repricing window actually last?
- What end-to-end latency is required to capture it?
- Does the edge survive fees, spread and slippage?

## 2. Architecture

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/architecture-dark.svg">
    <img alt="Two websocket feeds enter a single-threaded event loop: the sports feed is parsed into events, scored into a fair probability, and compared against order book state before an order is sent to the Polymarket CLOB. Latency is timestamped at every boundary." src="assets/architecture-light.svg" width="100%">
  </picture>
</p>

The sports feed drives the hot path: every stage between a real-world event and
an order leaving the process is on the latency budget. The CLOB websocket is a
separate, continuous stream — it keeps order book state so the decision stage
can price the spread it would actually cross.

- **Ingestion** — websocket client for the sports feed, parsing each frame into
  a normalised event; zero-copy where the format allows it.
- **Order book state** — a second websocket client maintaining the CLOB book,
  so the decision stage prices against the spread it would actually cross.
- **Inference** — a lightweight model estimating the post-event probability.
- **Decision** — trade only if the estimated edge exceeds fees + spread + a
  safety margin.
- **Execution** — orders pre-built ahead of time to minimise work on the hot
  path.
- **Deployment** — VPS region chosen by measured round-trip time to the data
  feed and to the Polymarket API.

## 3. Design choices

| Choice | Rationale | Measured impact |
|---|---|---|
| Rust | No GC pauses, predictable tail latency | not measured |
| Single-threaded event loop | No context switches or lock contention on the hot path | not measured |
| Pre-built orders | Keeps serialisation and signing off the hot path | not measured |
| VPS region selection | Network is expected to be the largest fixed cost | not measured |

Each row is a hypothesis about where the latency goes. The point of the project
is to replace "not measured" with a number, and to drop the choices that turn
out not to matter.

## 4. Latency budget

Timestamps are taken at each stage and aggregated into HDR histograms. Both
columns stay empty until there are enough events across enough matches for the
percentiles to mean anything.

| Stage | Baseline | Current |
|---|---|---|
| Feed → ingestion | — | — |
| Parsing | — | — |
| Inference | — | — |
| Decision | — | — |
| Order → CLOB ack | — | — |
| **Total (p50 / p95)** | — | — |

## 5. Results

The hypothesis is not yet tested: that needs a sports feed that actually shows
events, and no such feed has been found. Three things have been measured.

**The book reconstruction is correct.** Replaying a 17-minute recording of a
live Serie A market, the order book rebuilt from the snapshot and the deltas
agreed with the exchange's own `best_bid` and `best_ask` on **100% of 14,866
updates**. The live pipeline will price the spread it crosses with this same
code, so this is the one piece that is now known to be right rather than
assumed to be.

**A free scoreboard endpoint is too slow to be the input.** Polling ESPN at
1 Hz during a live match, the response changed only every 10–20 seconds, with
HTTP round-trip at a median 87 ms: the ceiling is the source cadence, not the
transport. A feed with 10-second granularity cannot beat a market that
reprices in seconds, however fast the system behind it is.

**Most of what a feed shows is not an event.** Over 17 minutes of a live
match, the same endpoint produced 23 changes and not one of them was a score:
they were bookmaker odds and the match clock. This matters more than it looks.
Run naively, the analysis credited five of those changes with the same single
market move and reported a tidy distribution of lags from 23 s to 114 s — all
of it an artifact. Each market move is now claimed by exactly one observation,
the nearest before it, and contested matches are reported rather than counted.
A noisy feed will otherwise manufacture whatever result it is asked for.

*Sample: one match, one market, 1,020 s, 7,649 market frames, 24 stored feed
responses, no sequence gaps and no recorder stalls.*

Still to measure, with the sample size and the method stated:

- the observed repricing window, once a feed shows events,
- captured edge as a function of end-to-end latency,
- the net result after fees and slippage,
- whether the hypothesis is validated, rejected, or holds only in some
  conditions.

A negative result is a result and will be published as one.

## 6. Known limits

- The latency of the data feed itself is a hard floor, and it is not under the
  system's control.
- Liquidity on live markets may be too thin for the edge to scale, even if it
  exists.
- Platform rules (order delays, terms of use) can remove the edge entirely, and
  can change at any time.
- Paper-mode results overstate live performance: they assume fills that a real
  order book may not provide.

## 7. Running it

```bash
cargo build --release
cp config.example.toml config.toml
```

### Recording

The recorders are the part that works today, and they are deliberately the
first thing built: the hypothesis above is answerable from recordings alone,
without a fast system. Recordings are also what makes every later benchmark
reproducible, which a live feed can never be.

Record both feeds at once, in two processes, so the two can be aligned:

```bash
cargo run --release --bin record -- --feed polymarket &
cargo run --release --bin record -- --feed sports &
```

**The market.** The CLOB market channel is public, so no credentials are
needed. Put the outcome token ids you want in `polymarket.asset_ids` — they
are the `clobTokenIds` of a market on the
[Gamma API](https://gamma-api.polymarket.com/markets?closed=false&limit=5).

**The sports feed.** No provider is hard-coded. Vendors publish latency
figures that are not comparable with each other and that none of them measures
the same way, so `sports_feed.url` points at whatever source is being
evaluated and the recording measures its lag directly, against the same clock
as the market.

Each run writes `data/recordings/<feed>-<unix-secs>.jsonl` plus a `.meta.json`
sidecar describing what was recorded and with which clocks. One JSON object
per line:

| Field | Meaning |
|---|---|
| `seq` | Dense counter; a gap means a lost record, not a quiet feed |
| `t_mono_ns` | Monotonic nanoseconds since start — use this for durations |
| `t_unix_ns` | Wall clock at receipt — the only clock comparable across recordings |
| `kind` | `data` (a websocket frame), `poll` (a changed HTTP response), `text`, or `control` |
| `t_request_*` | On a poll: when the request went out, bounding the change |

Both clocks are kept because neither is enough alone: the monotonic one cannot
jump when NTP steps the system clock, and the wall clock is the only one that
lines a market recording up with a sports recording taken by another process.

A polled source cannot say when something happened, only that it had happened
by the time you asked. Every `poll` record therefore carries `t_request_*` as
well, and `unchanged_polls` — the number of identical responses before it. The
change happened somewhere inside that window, and the record says how wide it
is instead of assuming it away.

Only changed responses are stored; unchanged ones become a periodic
`unchanged` heartbeat, so a quiet recording is never mistaken for a dead one.
Connection lifecycle is recorded the same way. If a writer ever falls behind
its feed, the run reports a non-zero `stalls` count: those recordings measure
the recorder as much as the market.

### Analysing

```bash
cargo run --release --bin analyse --   --market data/recordings/polymarket-<unix>.jsonl   --sports data/recordings/sports-<unix>.jsonl
```

This is the measurement the project exists to make. It replays the market
recording into a mid-price series by rebuilding the order book from the
snapshot and the deltas, diffs each stored sports response against the one
before it to find what the feed showed changing, and reports how long the
market took to move afterwards.

Three choices in it are worth stating, because each one is a way the number
could have been flattered:

- **Alignment is on wall clock, never on the monotonic clock.** Each recorder
  starts its own monotonic origin, so comparing those across two files yields
  a confident, meaningless offset.
- **Polling uncertainty stays visible.** A change is bounded by the request
  that found it and the response that carried it, so every lag is reported as
  a range rather than a point.
- **Feed changes the market ignored are excluded, not counted as zero.** They
  are reported separately. Averaging them in as instant reactions would invent
  an edge that is not there.

The rebuilt book is cross-checked against the `best_bid` and `best_ask` the
exchange reports on every delta, and the agreement rate is printed. The live
pipeline will depend on this same reconstruction to price the spread it is
about to cross, so a disagreement is better found here, against a recording,
than later against money.

`--min-move` sets what counts as a reaction (default 0.02 of probability) and
`--horizon-secs` how long to wait for one. `--path` narrows which parts of the
feed document count as events; with none, every change counts.

### Running the pipeline

```bash
cargo run --release -- --mode paper
```

`--mode paper` is simulation only and sends no orders; `--mode live` is
refused, because no execution path has been written or tested yet. `config.toml`
holds credentials and is git-ignored, as is `data/`; only
`config.example.toml` is committed.

## 8. Roadmap

Ordered so that the hypothesis is tested before the fast system is built.

- [x] Recorder for the Polymarket CLOB market channel
- [x] Recorder for a sports feed, provider-agnostic, measuring its own lag
- [ ] A sports feed fast enough to be worth trading on, found by measurement
- [x] Offline analysis: replay, book reconstruction, lag measurement
- [ ] Edge against latency, once a fast enough feed exists — fills §5
- [ ] Live terminal dashboard: feed state, latency histograms, captured edge
- [ ] Order book state in the live pipeline, reusing the replay reconstruction
- [ ] Latency instrumentation end to end (HDR histograms)
- [ ] First fair-value model, however crude
- [ ] Decision layer with an explicit fee and spread model
- [ ] Paper execution and simulated PnL
- [ ] Feed and VPS region comparison
- [ ] Better fair-value model, measured against its latency cost

## Disclaimer

This repository is published for educational purposes. It is not financial
advice, and it is not an invitation to trade. Anyone running it is responsible
for complying with the terms of service of the platforms involved and with the
law in their own jurisdiction.

## License

MIT — see [LICENSE](LICENSE).
