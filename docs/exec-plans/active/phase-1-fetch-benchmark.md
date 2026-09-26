# Phase 1 — M5.6: the fetch-batch benchmark and the round-trip budget

**Task:** M5.6 ★ (`phase-1.md` §C.2, M5 table). **Decision it feeds:** `phase-1.md` §C.3 item 10,
re-asked as ADR-0004 owner-review point (a); also point (g), the wire array size on Oracle.
**Date:** 2026-09-26. **Status:** measured; the proposed default is registered as a setting and
awaits the owner's sign-off. The wire behaviour is unchanged (ADR-0004 accepted limitation 12).
**Data:** `phase-1-fetch-benchmark-data/` (raw runs, per-cell medians, fits, environment; its
README lists every file). **Harness:** `crates/drivers/oracle-thin/tests/m5_6_fetch_benchmark.rs`.

## Summary

**Proposed shipped default:** a budget of **192 KiB per round trip**
(`results.round_trip_bytes`, new) with **`results.fetch_rows` = 1,000** as the row bound
(unchanged). The Result Store asks for `clamp(192 KiB / row width, 1, 1,000)` rows a round trip
(ADR-0004 RS2).

- **Latency ceiling (proposed).** At the default, a round trip must stay under **50 ms p50 on
  loopback and 100 ms p50 at 10 ms round-trip time** for every measured shape. With
  `fetches_in_flight` = 2 that keeps "Stop fetching" under about 200 ms at 10 ms. The default is
  far inside it: at most 10.9 ms on loopback and 21.2 ms at 10 ms.
- **Why 192 KiB.** It maximises the mixed shape's throughput on loopback and at 10 ms together:
  the worst of the two is 88% of that link's best, and no other budget does better on both
  (Decision). Narrow rows are bound by `results.fetch_rows` at about 90% of their best on both
  links.
- **The cost of it.** Wide rows (about 16 KB) get 12 rows a round trip. That is 99% of their best
  on loopback but 56% at 10 ms and 36% at 40 ms. The best budget grows with the round-trip time,
  which is why the setting is allowed at profile level.
- **The quadratic term (U-19), quantified.** On 19c a round trip's time is
  `a + b·P + c·P²` in its packets `P`. On loopback `c` is 0.076 ms/packet² for 10 `NUMBER`s,
  0.012 for 5 texts + 2 dates and 0.0023 for 16 KB rows. Doubling a large round trip's bytes
  multiplies its time by about 3.8: 10,000 → 20,000 narrow rows took 0.56 → 2.13 s, where the
  1,000-row time scaled linearly predicts 0.2 s. Per re-parse, the client pays about **1 µs per
  row and 0.5–0.7 µs per KB**, so both bounds are needed: bytes for wide rows, rows for narrow
  ones.
- **The wire array stays at 100 rows** (ADR-0004 limitation 12; this task does not change it).
  Its measured cost: at 10 ms round-trip time, 100,000 rows take **12.8 s** (narrow) and
  **13.7 s** (mixed) through a 100-row array, against **2.1 s** and **4.0 s** if the budget sized
  the wire. On loopback the difference is small (1.4 against 1.0 s; 2.2 against 2.1 s).
- **Oracle 23ai is not measured.** Its server marks end-of-response, so the quadratic term should
  vanish; the budget would then be free to grow. The setting and the harness are ready for it.

### Headline table

Rows per second at the proposed default, against the best rows-per-round-trip on the same link
and against today's 100-row wire array. Per-round-trip times are medians of three runs;
rows-per-round-trip sizes between measured points are interpolated log-log
(`default-vs-today.csv`).

| Shape | Link | Best rows/s (at rows/trip) | Default: rows/trip | p50 ms | rows/s | % of best | 100-row wire: rows/s | Default ÷ 100-row |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `numbers10` | loopback | 111,643 (502) | 1,000 | 10.2 | 97,819 | 88% | 71,736 | 1.4× |
| `numbers10` | 2 ms | 75,620 (1,000) | 1,000 | 13.2 | 75,620 | 100% | 22,262 | 3.4× |
| `numbers10` | 10 ms | 52,777 (1,992) | 1,000 | 21.0 | 47,585 | 90% | 7,795 | 6.1× |
| `numbers10` | 40 ms | 29,423 (2,022) | 1,000 | 52.2 | 19,168 | 65% | 2,324 | 8.2× |
| `text5date2` | loopback | 55,060 (251) | 529 | 10.9 | 48,654 | 88% | 46,232 | 1.1× |
| `text5date2` | 2 ms | 41,444 (499) | 529 | 12.9 | 41,108 | 99% | 19,845 | 2.1× |
| `text5date2` | 10 ms | 25,816 (993) | 529 | 21.2 | 24,992 | 97% | 7,293 | 3.4× |
| `text5date2` | 40 ms | 14,097 (1,007) | 529 | 53.2 | 9,935 | 70% | 2,299 | 4.3× |
| `wide4k` | loopback | 1,830 (10) | 12 | 6.6 | 1,807 | 99% | 828 | 2.2× |
| `wide4k` | 2 ms | 1,461 (50) | 12 | 9.7 | 1,238 | 85% | 774 | 1.6× |
| `wide4k` | 10 ms | 1,201 (50) | 12 | 18.0 | 668 | 56% | 686 | 1.0× |
| `wide4k` | 40 ms | 685 (50) | 12 | 48.5 | 247 | 36% | 602 | 0.4× |

"Loopback" is the `direct` link: the container's listener through Docker Desktop's forwarding,
0.8 ms measured round trip. "2/10/40 ms" are the relay links; their measured round trips were
4.1, 12.3 and 42.4 ms, because the relay adds its delay to the loopback path, plus about
1–1.5 ms of Windows timer granularity.

## Environment

Full list: `phase-1-fetch-benchmark-data/environment.csv`.

- **Machine.** AMD Ryzen 7 5700G (8 cores, 16 threads), 63.4 GB RAM, Windows 11 Pro build 26200.
  The power plan was left as found (Balanced, on AC). No power, display or input change was made.
- **Toolchain.** rustc 1.98.1 (MSVC), `cargo test --release` (thin LTO, one codegen unit).
- **Driver.** `oracledb` 26.0.0-beta.3 (pinned), under `reldex-driver-oracle-thin`. Raw
  `oracledb` was used only for the prefetch experiment.
- **Database.** Oracle Database 19c Enterprise Edition 19.3.0.0.0 in the `reldex-oracle19c`
  container (Docker Desktop 29.4.3, WSL2; 4 GiB memory limit, no CPU limit). Negotiated **SDU
  8,192 bytes, TNS protocol version 318**, read from the listener's `ACCEPT` packet. 318 is below
  319, so `oracledb` does not ask for end-of-response and the O(packets²) re-parse (U-19) applies.
- **Machine state.** The machine was shared, and another worker was building in its own
  worktree. Its `cargo` processes were present throughout. At the start of 77 of the 375 matrix
  runs at least one build tool (`rustc`, `cl`, `link`, `clippy-driver`, `ninja`) was running,
  mostly a single `rustc`; **9 runs started during a heavy build** (6–17 compiler processes), all
  on the 10 and 40 ms links: `text5date2` at 100 and 250 rows (run 3) at 10 ms; `numbers10` at
  250, 1,000 and 2,000 rows and `text5date2` at 100, 250 and 5,000 rows at 40 ms (`runs.csv`
  `machine_state`). They explain most of the CoVs above 10%; each is one run of three, so the
  medians absorb them. The wait for quiet before each group gave up after 300 s once (the 40 ms
  `wide4k` group; `groups.csv`). A non-build desktop process used about 2.2 cores continuously.
  CPU load was 32–63% at the start of each group.
- **When.** The matrix ran 2026-09-26 11:27–12:22 (+07:00) at commit `8047545`; the CLOB and
  bandwidth supplements ran 12:22–12:43 at `1a4720b`, which changes no fetch path.

## Method

### The harness

`crates/drivers/oracle-thin/tests/m5_6_fetch_benchmark.rs`, with `m5_6_support/relay.rs` and
`m5_6_support/sample.rs`. It is an `#[ignore]`d live test behind the crate's `oracle-it` feature.

- **Where, and why.** In the Oracle driver crate, not `db-core`. The cost being decided is the
  wire path: `oracledb`'s response parsing plus `oracle-thin`'s row-to-column conversion. Calling
  `oracle-thin` directly keeps `db-core`'s worker, event queue and compaction out of the
  per-round-trip number; their cost is known separately (ADR-0004 Table 2, M5.2's bench). The
  driver crate also has `oracledb` as a dev-dependency, which is the only way to measure
  `prefetch_rows`: the driver pins it to 0 and exposes no way to change it.
- **Why a test and not an example.** `tools/oracle-test-db/run-it.sh` is the only sanctioned way
  to put the test database's credentials into the environment, and it runs `cargo test`.
- **Why ignored.** A full matrix runs for about an hour, so the `db` stage of `tools/gates.sh`
  must never start it. It also does nothing unless `RELDEX_M56_OUT` names an output directory.
- **No production code changed** to build it. No dependency was added: the relay, the sampling
  and the statistics are standard library only. Three unit tests (the quadratic fit, the link
  parser, the shape table) run in the ordinary `oracle-it` test pass.

Run it with:

```bash
RELDEX_M56_OUT="$(cygpath -w "$PWD/out")" RELDEX_M56_QUIET_WAIT_S=300 \
  bash tools/oracle-test-db/run-it.sh m5_6_fetch_benchmark --release \
  -- --ignored --nocapture --test-threads=1 m5_6_fetch_benchmark_matrix
```

`RELDEX_M56_LINKS`, `_SHAPES`, `_RUNS`, `_SIZES`, `_CAP_MS` and `_EXTRAS` narrow it; the file's
header documents each. On Linux or macOS, `RELDEX_M56_OUT="$PWD/out"` is enough.

### One run

Every run is a **fresh child process** (the same test binary, running
`m5_6_fetch_benchmark_child`), so its CPU time and its peak memory are its own:

1. Connect, then seven `ping`s. Their median is the link's measured round-trip time.
2. Sample the process: CPU time, working set and private commit, each with its peak
   (PowerShell `Get-Process`, the instrument M5.1 and M5.2 used).
3. Print a mark and wait for the parent to answer. The parent snapshots the relay's counters
   while the connection is idle.
4. `execute` the shape's statement with `Statement::with_fetch_rows(n)`, then call
   `fetch_batch(n)` until the statement's rows are all fetched, or until 10 s have passed and at
   least five fetches were made. Each call is timed. Because the wire array size equals the batch
   size, **each `fetch_batch(n)` is exactly one round trip**.
5. Mark again (the parent snapshots the counters), and sample the process again.

The parent writes one row per run to `runs.csv`, then a median and a coefficient of variation
(sample standard deviation over mean, "CoV") per cell to `cells.csv`.

- **Interleaved.** The three runs of a cell are not consecutive: run 1 of every size, then run 2,
  then run 3. Drift in the machine's state therefore spreads over the sizes instead of biasing one
  of them.
- **Warm-up.** Before each link × shape group a warm-up child parses the statement once and
  measures the shape (declared and observed widths). Its numbers are not results.
- **Machine state.** Before each run the parent records which build tools are running
  (`tasklist`: cargo, rustc, cl, link, clippy-driver, ninja). Before each group it waits up to
  300 s for other workers' compilers and linkers to finish, and records the wait and the
  machine's CPU load (`groups.csv`).
- **Time to 100,000 rows.** `ms_per_100k_rows` is the run's wall time from `execute` to its last
  row, scaled to 100,000 rows. It is **measured** when the run fetched 100,000 rows and
  **extrapolated** from the run's own rate when the time cap stopped it (`capped_runs` in
  `cells.csv` says which). 75 of the 375 matrix runs were capped: small round trips on the 10 and
  40 ms links, and the 5,000-row mixed round trips.

### The shapes

Every value differs from the previous row's, because TTC compresses a value equal to the one
above it and would hide the cost (`phase-1-m5-1-data/README.md`). Every statement generates its
rows from `dual` with `CONNECT BY`, so **nothing is created in the database and nothing is left
behind**. The exact SQL is in `shapes.csv`.

| Shape | Columns | Rows | Declared width (B) | Store width (B, observed) | Wire (B/row) | Rows per round trip measured |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| `numbers10` | 10 `NUMBER`s | 100,000 | 442 | 117 | 67 | 50, 100, 250, 500, 1k, 2k, 5k, 10k, 20k |
| `text5date2` (mixed) | 5 `VARCHAR2(100)` + 2 `DATE` | 100,000 | 573 | 372 | 321 | 25, 50, 100, 250, 500, 1k, 2k, 5k |
| `wide4k` | `NUMBER` + 4 `VARCHAR2(4000)` | 2,000 | 16,077 | 16,041 | 16,046 | 1, 2, 5, 10, 25, 50, 100, 250 |
| `clob` (supplement) | `NUMBER` + a temporary CLOB (locator) | 5,000 | 365 | 328 | 54 | 50, 100, 250, 500, 1k, 2k |

The widths are the Result Store's: the declared width mirrors `db-core`'s
`declared_row_width` policy and sizes the first request; the observed width estimates what the
store accounts per row and sizes the rest (ADR-0004 RS2). Byte-budget equivalents of any size are
`rows × store width`: for example 1,000 `text5date2` rows are 363 KiB.

### The link

`m5_6_support/relay.rs` is a user-space TCP relay. The parent hosts it on an ephemeral loopback
port in front of the container's listener; the child connects to it instead. No admin right, no
Docker or network setting and no system setting is involved.

- **What it adds.** Each chunk read from one side is written to the other `RTT/2` later, in both
  directions, pipelined: a many-packet response is delayed once, as on a real long link. An
  optional rate limit (`rtt<ms>-<n>mbit`) adds serialization time per chunk.
- **What it does not model.** Loss, jitter, reordering, TCP slow start and the congestion
  window over a long path, and receive-window limits: the relay reads eagerly and buffers
  without bound, so the server never waits for a window a real bandwidth-delay product would
  impose. Both legs are loopback TCP. Every result over the relay is therefore a **lower
  bound** on what the same round-trip time costs on a real WAN.
- **What it measures.** It parses Oracle Net (TNS) framing as bytes pass, and counts bytes and
  packets per direction between the child's two marks. It also reads the SDU and the protocol
  version from the listener's `ACCEPT` packet.
- **Its own cost** is the `rtt0` link: the relay with no delay. It adds about 0.15 ms to a ping
  and 0.1–0.5 ms to a small round trip; on large round trips the difference is within run-to-run
  noise (tables below).

| Link | Injected RTT | Measured ping p50 (median of runs) | Range over 75 runs |
| --- | ---: | ---: | --- |
| `direct` (loopback) | — | 0.81 ms | 0.66–1.20 |
| `rtt0` | 0 | 0.96 ms | 0.82–1.68 |
| `rtt2` | 2 ms | 4.06 ms | 3.62–4.87 |
| `rtt10` | 10 ms | 12.31 ms | 11.92–51.08 |
| `rtt40` | 40 ms | 42.38 ms | 41.91–45.84 |

The 51 ms is one run's ping median: run 3 of `text5date2` at 100 rows on the 10 ms link started
while another worker's build had 14 `rustc` processes running (`runs.csv` `machine_state`).

## Results

The matrix: 3 shapes × 8–9 sizes (25 cells) × 5 links × 3 runs = **375 runs, all completed**.
Supplements: `clob` × 6 sizes × 3 links × 3 runs (54 runs) and two bandwidth-limited links ×
2 shapes × 8 sizes × 3 runs (96 runs). Every figure is a median over three runs; CoV in brackets.

### Per round trip: time (ms) against rows and packets

`numbers10`:

| Rows | Wire KB | Packets | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 50 | 3.4 | 1.0 | 1.05 (2%) | 1.20 (1%) | 4.19 (0%) | 12.63 (0%) | 42.82 (0%) |
| 100 | 6.7 | 1.0 | 1.39 (6%) | 1.55 (2%) | 4.49 (5%) | 12.83 (0%) | 43.02 (1%) |
| 250 | 16.6 | 3.0 | 2.62 (6%) | 2.67 (2%) | 5.58 (6%) | 13.91 (1%) | 44.19 (2%) |
| 500 | 33.1 | 5.0 | 4.47 (1%) | 4.92 (2%) | 7.37 (0%) | 15.77 (0%) | 45.95 (2%) |
| 1,000 | 66.1 | 9.0 | 10.22 (3%) | 10.53 (1%) | 13.22 (0%) | 21.02 (0%) | 52.17 (8%) |
| 2,000 | 132.3 | 17.0 | 27.90 (2%) | 26.58 (2%) | 30.51 (2%) | 37.87 (2%) | 67.89 (6%) |
| 5,000 | 331.5 | 41.1 | 148.60 (3%) | 141.33 (2%) | 152.04 (1%) | 156.36 (0%) | 189.39 (6%) |
| 10,000 | 665.4 | 82.5 | 557.94 (1%) | 551.75 (1%) | 568.30 (2%) | 578.56 (3%) | 618.04 (6%) |
| 20,000 | 1,340.8 | 166.0 | 2,126.38 (2%) | 2,109.22 (1%) | 2,221.55 (5%) | 2,245.09 (1%) | 2,251.66 (5%) |

`text5date2` (mixed):

| Rows | Wire KB | Packets | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 25 | 8.1 | 1.3 | 1.21 (6%) | 1.37 (2%) | 4.28 (2%) | 13.05 (1%) | 43.19 (0%) |
| 50 | 16.1 | 2.1 | 1.44 (11%) | 1.73 (10%) | 4.34 (1%) | 13.08 (2%) | 43.22 (0%) |
| 100 | 32.1 | 4.0 | 2.16 (8%) | 2.34 (1%) | 5.04 (2%) | 13.71 (13%) | 43.50 (9%) |
| 250 | 80.3 | 10.0 | 4.54 (2%) | 4.59 (3%) | 7.33 (3%) | 15.71 (9%) | 46.47 (4%) |
| 500 | 160.5 | 20.0 | 10.07 (3%) | 9.65 (2%) | 12.06 (3%) | 20.07 (7%) | 51.92 (2%) |
| 1,000 | 321.1 | 40.0 | 25.91 (10%) | 25.51 (2%) | 26.86 (4%) | 38.72 (4%) | 70.70 (8%) |
| 2,000 | 642.1 | 80.0 | 87.75 (5%) | 83.85 (0%) | 86.13 (2%) | 107.14 (13%) | 200.17 (31%) |
| 5,000 | 1,606.2 | 198.2 | 685.57 (10%) | 637.01 (2%) | 640.85 (2%) | 759.65 (7%) | 915.74 (20%) |

`wide4k`:

| Rows | Wire KB | Packets | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 16.1 | 2.0 | 1.30 (2%) | 1.51 (0%) | 4.60 (1%) | 12.96 (1%) | 43.11 (0%) |
| 2 | 32.1 | 4.0 | 1.74 (2%) | 1.96 (4%) | 4.74 (1%) | 13.08 (0%) | 43.19 (0%) |
| 5 | 80.3 | 10.0 | 3.21 (3%) | 3.30 (2%) | 6.00 (2%) | 14.28 (0%) | 44.62 (0%) |
| 10 | 160.5 | 20.0 | 5.46 (1%) | 5.78 (2%) | 8.34 (1%) | 16.53 (1%) | 46.76 (2%) |
| 25 | 401.2 | 50.0 | 14.58 (3%) | 14.71 (2%) | 17.78 (4%) | 25.16 (4%) | 56.15 (2%) |
| 50 | 802.3 | 99.0 | 29.48 (2%) | 30.35 (3%) | 34.22 (1%) | 41.57 (6%) | 72.89 (7%) |
| 100 | 1,604.6 | 198.0 | 120.80 (10%) | 116.21 (4%) | 129.21 (9%) | 145.85 (4%) | 166.11 (8%) |
| 250 | 4,011.4 | 495.0 | 848.09 (3%) | 850.46 (5%) | 882.68 (5%) | 900.14 (5%) | 923.89 (5%) |

Wire bytes and packets are the relay's counts (identical on every relayed link); the loopback
link has no relay, so it has none of its own.

### Throughput: rows per second

`*` is the best measured size on that link; `+` is within 10% of it.

| `numbers10` rows | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 50 | 44,648 (2%) | 39,249 (1%) | 11,811 (1%) | 3,939 (0%) | 1,162 (0%) |
| 100 | 67,678 (7%) | 61,362 (2%) | 21,653 (4%) | 7,762 (0%) | 2,312 (2%) |
| 250 | 92,596 (5%) | 91,144+ (3%) | 44,892 (8%) | 17,938 (1%) | 5,617 (13%) |
| 500 | 108,962* (1%) | 100,085* (3%) | 67,114 (1%) | 31,442 (0%) | 10,813 (14%) |
| 1,000 | 94,373 (3%) | 92,810+ (2%) | 74,680* (1%) | 46,723 (1%) | 18,896 (16%) |
| 2,000 | 70,490 (2%) | 73,954 (0%) | 63,929 (2%) | 52,170* (1%) | 28,839* (7%) |
| 5,000 | 33,344 (2%) | 35,104 (3%) | 32,286 (2%) | 31,405 (2%) | 25,840 (5%) |
| 10,000 | 17,774 (1%) | 18,137 (2%) | 17,559 (2%) | 17,039 (3%) | 16,033 (6%) |
| 20,000 | 9,566 (1%) | 9,640 (1%) | 9,146 (3%) | 9,051 (4%) | 9,152 (6%) |

| `text5date2` rows | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 25 | 18,868 (5%) | 17,261 (5%) | 5,645 (1%) | 1,914 (1%) | 576 (0%) |
| 50 | 31,576 (8%) | 27,726 (11%) | 11,205 (1%) | 3,801 (11%) | 1,149 (0%) |
| 100 | 44,346 (7%) | 41,317 (1%) | 19,679 (2%) | 7,212 (18%) | 2,284 (21%) |
| 250 | 53,727* (1%) | 52,189* (2%) | 33,694 (5%) | 15,817 (13%) | 5,342 (10%) |
| 500 | 48,846+ (4%) | 50,561+ (4%) | 40,699* (3%) | 24,396+ (7%) | 9,556 (2%) |
| 1,000 | 34,718 (7%) | 36,891 (6%) | 34,877 (4%) | 24,567* (5%) | 13,703* (7%) |
| 2,000 | 21,221 (3%) | 22,018 (0%) | 21,675 (3%) | 17,960 (11%) | 8,751 (38%) |
| 5,000 | 6,973 (10%) | 7,476 (1%) | 7,459 (1%) | 6,480 (6%) | 5,210 (22%) |

| `wide4k` rows | loopback | rtt0 | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 735 (3%) | 632 (1%) | 218 (1%) | 77 (1%) | 23 (0%) |
| 2 | 1,092 (2%) | 976 (9%) | 414 (1%) | 152 (1%) | 46 (0%) |
| 5 | 1,511 (4%) | 1,465 (2%) | 816 (2%) | 347 (1%) | 111 (0%) |
| 10 | 1,777* (3%) | 1,692* (2%) | 1,190 (2%) | 598 (1%) | 212 (2%) |
| 25 | 1,684+ (2%) | 1,666+ (1%) | 1,395* (2%) | 982+ (3%) | 439 (2%) |
| 50 | 1,521 (4%) | 1,442 (9%) | 1,278+ (6%) | 1,056* (7%) | 650* (4%) |
| 100 | 748 (4%) | 764 (2%) | 713 (6%) | 611 (5%) | 562 (8%) |
| 250 | 272 (2%) | 271 (2%) | 249 (1%) | 255 (8%) | 251 (3%) |

These are whole-run rates (`execute` to last row). The headline table and the decision use the
steady rate (rows per round trip over its median time), which leaves out the execute and the
slower first fetch. The two differ by up to 12%, most where a run has few round trips (the wide
shape's 2,000 rows).

**Time to 100,000 rows** (`ms_per_100k_rows` in `cells.csv`) follows directly. At the sizes the
default produces, measured: `numbers10` at 1,000 rows 1.06 s loopback, 2.14 s at 10 ms, 5.29 s at
40 ms; `text5date2` at 500 rows 2.05 s, 4.10 s and 10.5 s (the last extrapolated from capped
runs). At 100 rows a round trip, the wire
array the product uses today: `numbers10` 1.48, 12.9 and 43.2 s; `text5date2` 2.26, 13.9 and
43.8 s.

### Client CPU and memory

Per round trip, loopback. CPU is the child's own processor time (16 ms granularity, so small
round trips are averaged over the run); peak growth is the child's peak private bytes (and peak
working set) above what it had before the timed section. The harness drops each batch after
timing it, so this is the driver's transient cost of one round trip, not retention.

| Shape | Rows | CPU ms / round trip | CPU ÷ wall | Peak private growth | Peak working-set growth |
| --- | ---: | ---: | ---: | ---: | ---: |
| `numbers10` | 100 | 0.44 | 0.26 | 396 KiB | 500 KiB |
| `numbers10` | 1,000 | 9.22 | 0.85 | 2.6 MiB | 1.8 MiB |
| `numbers10` | 5,000 | 144.5 | 0.94 | 9.8 MiB | 6.7 MiB |
| `numbers10` | 20,000 | 1,993.8 | 0.95 | 38.5 MiB | 30.6 MiB |
| `text5date2` | 100 | 0.53 | 0.21 | 864 KiB | 816 KiB |
| `text5date2` | 500 | 7.42 | 0.76 | 2.5 MiB | 2.3 MiB |
| `text5date2` | 1,000 | 26.88 | 0.89 | 4.0 MiB | 3.6 MiB |
| `text5date2` | 5,000 | 670.8 | 0.94 | 14.5 MiB | 12.9 MiB |
| `wide4k` | 10 | 1.56 | 0.28 | 1.9 MiB | 1.7 MiB |
| `wide4k` | 100 | 118.0 | 0.90 | 11.0 MiB | 9.8 MiB |
| `wide4k` | 250 | 884.8 | 0.95 | 22.5 MiB | 19.6 MiB |

- **Large round trips are client-CPU-bound.** CPU ÷ wall reaches 0.94–0.96: the time is the
  client re-parsing, not the server or the network. The CPU fit has the same quadratic
  coefficient as the wall-time fit (0.077 against 0.076 ms/packet² for `numbers10`).
- **Memory follows rows per round trip** and is small at the default: at most about 2.6 MiB of
  transient growth per round trip for any measured shape.

### The O(packets²) effect

A least-squares fit of `time = a + b·P + c·P²` per link and shape (`fit.csv`; fitted relative,
so small and large round trips weigh alike; worst point within 29%):

| Shape | `c` loopback | `c` rtt0 | `c` 2 ms | `c` 10 ms | `c` 40 ms | Rows per 8 KB packet |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `numbers10` | 0.0756 | 0.0733 | 0.0790 | 0.0800 | 0.0802 | ~120 |
| `text5date2` | 0.0124 | 0.0118 | 0.0128 | 0.0170 | 0.0235 | ~25 |
| `wide4k` | 0.0023 | 0.0023 | 0.0028 | 0.0033 | 0.0035 | ~0.5 |

(ms per packet²). `c` does not depend on the link: the re-parse is client CPU. It depends on the
shape, because what is re-parsed is rows as well as bytes. A joint fit over every relayed cell,
`time = a + r·R + w·W + (P/2)·(α·R + β·W)` with `R` rows and `W` KB, gives **α ≈ 0.94–1.16 µs per
row** and **β ≈ 0.48–0.75 µs per KB** each time the response is re-read (median error 5–10%).
That predicts `c ≈ (α·rows per packet + β·8 KB)/2`: with α = 1 µs and β = 0.5 µs, 0.062, 0.015
and 0.0023 ms, close to the fits.

So a narrow row costs about 20 times more per wire byte than a wide one, and **neither a row
count nor a byte count alone bounds a round trip**: the budget bounds the bytes, and
`results.fetch_rows` bounds the rows.

### SDU

The relay read the SDU the listener accepted while the client asked for 8 KB (default), 64 KB
and 2 MB (`sdu.csv`): **the server accepted 8,192 bytes every time**. The round-trip times did
not move: `text5date2` at 1,000 rows took 25.9, 25.0 and 25.3 ms, at 5,000 rows 626, 648 and
651 ms. A larger SDU would reduce `P` and so the quadratic term, but this 19c configuration does
not grant one; raising it is a server-side setting (`DEFAULT_SDU_SIZE` in `sqlnet.ora`), outside
the product.

### Prefetch

`oracledb`'s other fetch knob, `prefetch_rows`, returns rows with the execute itself. The driver
pins it to 0 (U-3 containment: with prefetch on, values are decoded before the driver can refuse
a column that aborts the process). Measured through raw `oracledb` (`prefetch.csv`; time to the
first page, including the execute; medians of five):

| Link | 100 rows, prefetch 0 | 100 rows, prefetch 100 | 1,000 rows, prefetch 0 | 1,000 rows, prefetch 1,000 |
| --- | ---: | ---: | ---: | ---: |
| loopback | 3.8 ms | 2.7 ms | 50.1 ms | 48.1 ms |
| 2 ms | 9.9 ms | 5.7 ms | 53.3 ms | 51.3 ms |
| 10 ms | 26.8 ms | 13.8 ms | 71.7 ms | 55.9 ms |
| 40 ms | 86.7 ms | 44.2 ms | 133.6 ms | 88.9 ms |

Prefetch buys **exactly one round trip off the first page** and nothing after it. It stays off:
the U-3 containment is worth more than one round trip, and the saving is the same whatever the
budget.

### Supplement: LOB locators

`clob` (`clob-*.csv`): 5,000 rows of `NUMBER` + a temporary CLOB, fetched as locators. The wire
carries about 54 bytes a row, but the store charges a LOB cell 320 B (ADR-0004; M5.5 measures the
real figure), so the budget sizes these round trips about six times smaller than their wire bytes
suggest: 598 rows at the default.

| Rows | loopback | rtt0 | 10 ms |
| ---: | ---: | ---: | ---: |
| 100 | 1.77 ms | 2.79 ms | 14.01 ms |
| 500 | 5.83 ms | 6.74 ms | 17.87 ms |
| 1,000 | 10.99 ms | 10.73 ms | 25.03 ms |
| 2,000 | 20.74 ms | 24.32 ms | 39.74 ms |

At the default (598 rows) that is 90% of the best measured rate on loopback and 61% at 10 ms.
The LOB contents are a separate round trip each (M5.5), which dwarfs this.

### Supplement: bandwidth-limited links

The relay's serialization rate (`bandwidth-*.csv`): 10 ms with 100 Mbit/s, and 40 ms with
20 Mbit/s.

| Shape | Link | Best rows/s (rows/trip) | Default rows/s | % of best | 100-row wire rows/s |
| --- | --- | ---: | ---: | ---: | ---: |
| `text5date2` | 10 ms, 100 Mbit/s | 22,965 (993) | 18,870 | 82% | 6,450 |
| `text5date2` | 40 ms, 20 Mbit/s | 6,465 (2,005) | 4,687 | 72% | 1,788 |
| `wide4k` | 10 ms, 100 Mbit/s | 633 (50) | 414 | 65% | 578 |
| `wide4k` | 40 ms, 20 Mbit/s | 147 (250) | 98 | 67% | 145 |

A slow link favours larger budgets, as a long one does: the default reaches 82% and 72% of the
mixed shape's best here, where 384 KiB would reach 100% and 89% (`budget-candidates.csv`). It is
one more reason the budget can be set per profile.

## Decision

### The criteria

1. **Objective:** the mixed shape's throughput on loopback and at 10 ms round-trip time, together.
   "Together" is the worse of the two, as a share of that link's best; ties go to the geometric
   mean.
2. **Latency ceiling:** a round trip at the default stays under 50 ms p50 on loopback and 100 ms
   p50 at 10 ms, for every measured shape. It keeps "Stop fetching" (at most `fetches_in_flight`
   = 2 round trips) near 200 ms at 10 ms, and it bounds how far a future default may go.
3. **ADR-0004's caps:** the wide shape must not break the byte cap's bounded overshoot, which is
   `fetches_in_flight` × the budget; any budget here passes that.
4. **Narrow rows** must stay near their best through `results.fetch_rows`.

### The candidates

Every candidate at `results.fetch_rows` = 1,000, sized by the observed store width, as a share of
each link's best (`budget-candidates.csv`, which also has `fetch_rows` = 2,000):

| Budget | `text5date2` rows | loopback | 10 ms | worse of the two | `wide4k` rows | loopback | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 96 KiB | 264 | 99% | 64% | 64% | 6 | 89% | 34% | 19% |
| 128 KiB | 352 | 95% | 77% | 77% | 8 | 95% | 42% | 25% |
| 160 KiB | 440 | 92% | 89% | 89% | 10 | 100% | 50% | 31% |
| **192 KiB** | **529** | **88%** | **97%** | **88%** | **12** | **99%** | **56%** | **36%** |
| 224 KiB | 617 | 84% | 98% | 84% | 14 | 98% | 60% | 41% |
| 256 KiB | 705 | 80% | 98% | 80% | 16 | 97% | 65% | 45% |
| 384 KiB | 1,000 | 70% | 100% | 70% | 24 | 94% | 81% | 63% |
| 512 KiB | 1,000 | 70% | 100% | 70% | 32 | 93% | 89% | 76% |
| 1 MiB | 1,000 | 70% | 100% | 70% | 65 | 71% | 81% | 95% |

`numbers10` is bound by `results.fetch_rows` from 128 KiB up: 1,000 rows, 88% on loopback and 90%
at 10 ms, whatever the budget.

- **160 and 192 KiB tie on the worse link** (89% and 88%). 192 KiB has the better geometric mean
  (92.4% against 90.5%) and serves wide rows better (56% against 50% at 10 ms), so it is chosen.
- **The ceiling holds with room.** At 192 KiB the slowest round trip is 10.9 ms on loopback and
  21.2 ms at 10 ms. It would first bind at about 1 MiB (16 KB rows: 50.3 ms on loopback).
- **M5.2's 256 KiB placeholder** was close: 80% / 98%. It gave up 8 points of loopback for 1 point
  at 10 ms.

### `results.fetch_rows` = 1,000

With a 192 KiB budget, `results.fetch_rows` binds only rows narrower than about 197 bytes in the
store. For `numbers10` (117 B) it is the bound that matters:

| `fetch_rows` | loopback | 2 ms | 10 ms | 40 ms |
| ---: | ---: | ---: | ---: | ---: |
| 500 | 100% | 90% | 60% | 37% |
| **1,000** | **88%** | **100%** | **90%** | **65%** |
| 2,000 | 64% | 87% | 100% | 100% |

1,000 is the only value that keeps both loopback and 10 ms at or near 90%. The default and the
bounds (1–100,000) stay as they are.

### What the default costs, and what a user can do about it

- **Wide rows on a slow link.** At 10 ms a 16 KB row gets 56% of its best, and at 40 ms 36%, less
  than today's 100-row array (0.4×). The best budget grows with the round-trip time: for the mixed
  shape about 90 KiB on loopback, 180 KiB at 2 ms and 360 KiB at 10 ms; for 16 KB rows about
  160 KiB on loopback and 800 KiB from 2 ms up. A **profile for a distant server** is where a
  larger budget belongs: 512 KiB gives `wide4k` 89% at 10 ms and 76% at 40 ms, and keeps the
  mixed shape at 100% at 10 ms.
- **The first request is sized by the declared width** (ADR-0004 RS2), which overstates sparse
  text: `text5date2`'s first request is 343 rows, not 529. That costs one smaller round trip.

## The wire array: what changing it would do

The proposal changes what the store *asks for*. It does not change what `oracle-thin` puts on
the wire. The adapter's execute passes no fetch-size hint, so each wire round trip carries
`oracledb`'s default of **100 rows**, fixed at execute (ADR-0004 accepted limitation 12,
owner-review point (g)); a store request of 529 rows is served by six 100-row round trips.

What the product would gain if each round trip carried the budget's rows instead (the headline
table's last column):

- **Narrow and mixed rows on a network:** 3.4–6.1× at 10 ms, 4.3–8.2× at 40 ms. 100,000 rows at
  10 ms: 12.8 → 2.1 s (`numbers10`), 13.7 → 4.0 s (`text5date2`).
- **On loopback:** 1.1–1.4× for narrow and mixed rows; 2.2× for 16 KB rows, whose 100-row round
  trip (1.6 MB, 121 ms) is well past the knee of the quadratic.
- **Wide rows on a network:** no gain at 10 ms, a loss at 40 ms (0.4×), unless the profile raises
  the budget.

How it could be done:

1. **An upstream setter for the array size after execute (recommended).** The driver's execute
   is already a describe (prefetch 0), so the widths are known before the first fetch;
   `oracledb`'s fetch message reads the array size from the statement's options on every fetch
   (`messages/fetch.rs`), but `oracledb` 26.0.0-beta.3 has no public setter. With one, each
   store request becomes one round trip of exactly its rows, at no extra round trip. It goes to
   the owner with Issue J (owner-review point (e)).
2. **A width-blind hint at execute.** One number for every statement cannot work on 19c. Passing
   `results.fetch_rows` (1,000) makes a 16 KB-row round trip about 16 s (extrapolated from these
   measurements; ADR-0004 Table 3a measured 23 s). A compromise of 250 rows would take the mixed
   shape at 10 ms from 7,293 to 15,910 rows/s, but make a 16 KB-row round trip take 848 ms on
   loopback, 2.8 times slower than today's 100 rows.
3. **A describe before every execute.** One extra round trip on every query to help the ones
   that are large. Not recommended while option 1 is open.

Until the owner rules on (g), nothing changes on the wire, and `results.fetch_rows` is still not
passed as the hint. **A trap for M5.2 Stage B:** `StatementSettings::apply`
(`crates/workspace/src/connect.rs`) does call `Statement::with_fetch_rows` with the resolved
`results.fetch_rows`. The product path does not use it today; Stage B must not start using it
as it stands, or 1,000-row wire arrays arrive for every shape.

## Oracle 23ai (not measured)

No 23ai server was available. What the model predicts:

- A 23ai server (TNS protocol ≥ 319) marks end-of-response, so `oracledb` stops re-parsing from
  byte 0 on every packet (U-19). The `(P/2)·(α·R + β·W)` term should vanish, leaving a round
  trip linear in its rows and bytes.
- Throughput would then rise with round-trip size until the link, the server or memory limits
  it, and the best budget would be several times larger on every link. The latency ceiling, not
  the quadratic, would set the default: on loopback the linear part is about 3 µs per row and
  18 µs per KB (joint fit, `rtt0`), so a 1 MiB round trip of mixed rows would cost about 30 ms.
- A server configured with a larger SDU would also reduce `P`; this 19c server granted 8 KB
  whatever the client asked (SDU, above).
- **What to do:** re-run this harness against a 23ai container (`RELDEX_IT_*` pointing at it) and
  record the server version, as every row here records 19.3. The budget is a setting, so a
  23ai-specific default can be a profile's value or a later built-in change; the registry
  already allows up to 4 MiB.

## What was not measured

- **A real WAN.** The relay is a lower bound: no loss, jitter, slow start or receive-window
  limits, and both legs are loopback. The bandwidth supplement is a crude serialization model.
- **Oracle 23ai** (above), and any server version other than 19.3.
- **Mobile.** No device run; `SPEC.md` §25 and `AGENTS.md` require physical-device evidence
  before any mobile claim. The mobile default is the desktop one until then.
- **The product path end to end.** The harness calls `oracle-thin` directly; the store, the FFI
  and the grid are not in these numbers (ADR-0004 Table 2 and M5.7 cover them).
- **Budget-sized wire arrays through the product.** They cannot be built today (limitation 12);
  the "default" figures are the harness's round trips at the rows the store would ask for.
- **`fetches_in_flight` > 1 on a network.** Every run had one fetch outstanding. M5.7 measures
  pipelining on a real network.

## What this change records

- **Registry:** `results.round_trip_bytes` (new): `ByteLimit`, default 192 KiB, application /
  profile / worksheet, 16 KiB–4 MiB, "no limit" refused, next statement. `results.fetch_rows`:
  unchanged, its documentation now cites this measurement. ADR-0006 amendment "The round-trip
  budget (M5.6)".
- **`db-core`:** `DEFAULT_ROUND_TRIP_BYTES` 256 KiB → 192 KiB, pinned to the setting's default by
  `crates/workspace/tests/result_pipeline_defaults.rs`.
- **ADR-0004:** "As measured: M5.6", and notes at RS2, accepted limitation 12 and owner-review
  points (a) and (g).
- **Not changed:** the wire array, the hint, `prefetch_rows`, any production fetch path.
