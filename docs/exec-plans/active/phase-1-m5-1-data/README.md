# M5.1 evidence data (ADR-0004)

These are the raw numbers behind `docs/decisions/0004-result-store.md`. Every table in the ADR is
built from these files.

| File | What it holds | Produced by |
| --- | --- | --- |
| `representation.csv` | Bytes per retained row and append time for three result-store representations, at 1k/100k/1M rows; fetch-size sensitivity | `crates/db-core/benches/result_store_shapes.rs` (M5.1 run) |
| `mock-path.csv` | 1,000,000 rows through a real `db-core` session and worker with the mock driver | the same bench |
| `real-db-fetch-latency.csv` | 100,000-row queries at 100 / 1,000 / 10,000 rows per fetch, through `db-core` + `oracle-thin` against the local 19c container | a throwaway probe (M5.1 run; deleted after the run) |
| `driver-fetch-probe.csv` | Fetch cost against rows per fetch, measured at the driver level (release build, `oracle-thin` directly, no `db-core` worker or event queue). Also a wide-row shape (4 × `VARCHAR2(4000)`, about 16 KB per row) and a 2 MiB SDU variant | the independent review of PR #39 (2026-09-25) |
| `review-reproduction.csv` | The review's independent re-run of `representation.csv`, plus two small-result rows (50 rows of `s14`, 100 rows of `text5date2`) | the independent review of PR #39 |
| `environment.csv` | Machine, toolchain, database, machine state, metric definitions and the real-DB SQL | M5.1 run |

## Columns

In `driver-fetch-probe.csv`:

- `total_ms` is the wall time for the whole result.
- `batch_p50_ms` is the median time of one `fetch_batch`.
- `us_per_row` is `total_ms` divided by `rows_total`.
- `client_cpu_over_wall` is the probe process's CPU time divided by wall time. A value near 1
  means the client, not the server or the network, was the bottleneck.
- Rows labelled `*-warm` are a two-batch warm-up and are not results.

## Generated rows must differ from each other

A synthetic probe must make every row's values different. TTC, Oracle's wire protocol, compresses
a column value that repeats the previous row's value.

When the review generated wide rows with a constant `RPAD`, they came back at 16 KB/row in about
20 ms. The same row width with a distinct value on every row took 23 s for one 1,000-row fetch
(`driver-fetch-probe.csv`, `wide16k`).

A benchmark built on repeated values measures the compression and hides the fetch cost. This
applies to M5.6's fetch benchmark as well.
