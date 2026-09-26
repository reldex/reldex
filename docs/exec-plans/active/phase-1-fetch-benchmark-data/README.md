# M5.6 fetch-batch benchmark data

The raw numbers behind `docs/exec-plans/active/phase-1-fetch-benchmark.md`. Every table in that
document is built from these files. All of them were produced by the ignored live test
`crates/drivers/oracle-thin/tests/m5_6_fetch_benchmark.rs` against the local Oracle 19c container,
except the two derived files at the end.

| File | What it holds |
| --- | --- |
| `environment.csv` | Machine, toolchain, driver, database, negotiated SDU and protocol, the relay, the sampling instrument, machine state, repeats, time cap, and when each run happened |
| `runs.csv` | The matrix, one row per run: 3 shapes × 8–9 sizes × 5 links × 3 runs = 375 runs |
| `cells.csv` | The matrix, one row per cell: medians over the three runs, with coefficients of variation |
| `fit.csv` | Per link and shape, the fit `time = a + b·P + c·P²` of the per-round-trip p50 and of the client CPU per round trip against packets per round trip |
| `shapes.csv` | Per link and shape: the SQL, declared and observed row widths, the server banner, the SDU and protocol version the listener accepted |
| `groups.csv` | Per link × shape group: start time, machine CPU load, how long the parent waited for other workers' builds, the build tools running |
| `prefetch.csv` | `prefetch_rows` 0 against equal to the page, through raw `oracledb`: time to execute and to the first page of 100 and 1,000 rows, five repetitions per link |
| `sdu.csv` | The client asking for an SDU of 8 KB (default), 64 KB and 2 MB on the `rtt0` link: the SDU the listener accepted and the round-trip times |
| `clob-*.csv` | Supplement: `NUMBER` + a temporary CLOB fetched as a locator; same columns as the matrix files |
| `bandwidth-*.csv` | Supplement: `text5date2` and `wide4k` on two rate-limited relay links (10 ms at 100 Mbit/s, 40 ms at 20 Mbit/s) |
| `budget-candidates.csv` | Derived: for budgets of 32 KiB–1 MiB and `fetch_rows` bounds of 1,000 and 2,000, the rows per round trip the Result Store would ask for and the rate that implies on every link |
| `default-vs-today.csv` | Derived: the proposed default (192 KiB, 1,000 rows) against today's 100-row wire array, per shape and link |

## Columns

In `runs.csv` (and the supplements' `*-runs.csv`):

- `rows_per_fetch` is both the wire array size and the size of every `fetch_batch` call, so each
  fetch is exactly one round trip.
- `fetch_p50_ms`, `fetch_p90_ms`, `fetch_max_ms`, `fetch_mean_ms` are over the run's fetches;
  `steady_mean_ms` leaves out the first fetch; `first_fetch_ms` is the first alone.
- `execute_ms` is the `execute`, which on this driver is a describe (prefetch 0).
- `total_ms` runs from `execute` to the last row; `rows_per_s` is `rows / total_ms`;
  `ms_per_100k_rows` is `total_ms` scaled to 100,000 rows.
- `capped` is 1 when the 10 s time cap stopped the run (after at least five fetches). Its
  `ms_per_100k_rows` is then extrapolated from the run's own rate.
- `cpu_ms` is the child process's processor time over the timed section (16 ms granularity);
  `cpu_over_wall` is `cpu_ms / total_ms`.
- `peak_ws_growth_kib` and `peak_private_growth_kib` are the child's peak working set and peak
  private bytes above their values before the timed section. The harness drops each batch after
  timing it, so these are the transient cost of one round trip. `peak_is_bound` is 1 when the
  process's peak was still the one set before the timed section (by the connect), so the growth
  figure is an upper bound rather than a measurement.
- `ping_p50_ms`, `ping_min_ms`: seven `ping`s before the timed section.
- `wire_*` columns are the relay's counts for the timed section (empty on the `direct` link, which
  has no relay): bytes and TNS packets each way, the largest packet, the accepted SDU and protocol
  version, and `framing_lost` (1 if the relay lost track of TNS framing; always 0 here).
- `machine_state` is `tasklist`'s count of build tools at the start of the run.
- `connections` is how many connections the relay accepted for the run (1 on every run; the
  `direct` link reports 1 by definition).

In `cells.csv`: `fetch_p50_ms` is the median over runs of each run's p50; `*_cov` is the sample
standard deviation over the mean across the three runs; `capped_runs` says how many were capped.

## Derived files

`budget-candidates.csv` and `default-vs-today.csv` come from `cells.csv` (and the supplements'
cells) by one rule:

- rows per round trip = `clamp(budget / store_row_width_est, 1, fetch_rows)`, the Result Store's
  rule (ADR-0004 RS2) with the observed width (`first_request_rows` uses the declared width);
- the round trip's p50 at that size is interpolated linearly in log(rows)–log(ms) between the two
  measured sizes around it (extrapolated from the nearest two outside the measured range;
  `extrapolated` = 1 marks those);
- rate = rows / p50; best = the highest such rate over a fine grid of sizes within the measured
  range; `pct_of_best` = rate / best.

## Generated rows must differ from each other

Every shape makes each row's values different from the previous row's, because TTC compresses a
column value that repeats the one above it and would hide the fetch cost
(`phase-1-m5-1-data/README.md`). The statements generate rows from `dual` with `CONNECT BY`, so
the benchmark creates nothing in the database and leaves nothing behind.
