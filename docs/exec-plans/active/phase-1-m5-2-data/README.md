# M5.2 evidence data (ADR-0004, Stage A)

These files hold the bench re-run that compares the implemented Result Store against the M5.1
prototype, and the live 19c results. ADR-0004's section "As implemented: M5.2 Stage A" quotes
them. The M5.1 data they are compared with is in `../phase-1-m5-1-data/`.

| File | What it holds | Produced by |
| --- | --- | --- |
| `baseline-representation.csv`, `baseline-mock-path.csv` | The bench before any M5.2 change, on this machine today: the M5.1 prototype store | `crates/db-core/benches/result_store_shapes.rs` at `05bcc48` |
| `representation.csv`, `mock-path.csv` | The same bench measuring the implemented store: `ResultSegment::compact`, `accounted_bytes`, `value`, and a real `ResultStore` fed by `fetch_segment` | the bench at `7e1ac5b` |
| `representation-shrink-in-place.csv` | The rejected first implementation, which kept the driver's buffers and called `shrink_to_fit` | the bench on the branch before `a705941` |
| `comparison.csv` | The `store` rows of the three runs side by side (medians of 3) | `baseline` and `representation*.csv` |
| `live-19c.csv` | The five live tests on the local 19c container | `crates/reldex-core-poc/tests/m5_2_result_store_live.rs` |
| `environment.csv` | Machine, toolchain, commits, machine state and metric definitions | these runs |

Run the bench and the live tests with:

```bash
cargo bench -p reldex-db-core --bench result_store_shapes -- --csv "$PWD/representation.csv" --mock-csv "$PWD/mock-path.csv"
RELDEX_IT_PACKAGE=reldex-core-poc bash tools/oracle-test-db/run-it.sh m5_2_result_store_live -- --nocapture --test-threads=1
```

## Bytes per row against ADR-0004 Table 1

The implemented store costs what the prototype did. The largest change is +0.66%, well inside the
5% that would count as a regression. The extra bytes are a slightly larger per-segment header: the
`Arc`'s counters, a wider column header and the store's first-row index. They show most at 100 rows
per segment.

| Shape | Rows | Table 1 (M5.1) B/row | Implemented B/row | Change |
| --- | --- | --- | --- | --- |
| `numbers10` | 1,000,000 | 118.1 | 118.2 | +0.08% |
| `text5date2` | 1,000,000 | 415.5 | 415.6 | +0.02% |
| `s14` | 1,000,000 | 73.3 | 73.4 | +0.14% |
| `clob32k_inline` | 100,000 | 32,776.3 | 32,776.3 | 0 |
| `s14`, 100 rows per segment | 100,000 | 76.1 | 76.6 | +0.66% |
| `text5date2`, 100 rows per segment | 100,000 | 421.6 | 422.4 | +0.19% |

## Private bytes: why the store copies instead of shrinking in place

Private bytes over accounted bytes, at 1,000,000 rows (100,000 for `clob32k_inline`), medians of
3:

| Shape | M5.1 prototype (copies) | `shrink_to_fit` in place (rejected) | Implemented (copies, batch freed whole) |
| --- | --- | --- | --- |
| `numbers10` | +4.9% | +5.1% | +5.0% |
| `text5date2` | +3.2% | **+58.2%** | +3.2% |
| `s14` | +5.7% | +5.2% | +3.6% |
| `clob32k_inline` | +0.2% | +0.3% | +0.2% |

- **Shrinking in place.** On the Windows heap, `shrink_to_fit` gives back each text buffer's
  spare tail as a free fragment that sits between retained segments. The next batch's buffers grow
  by doubling, and they mostly cannot use those fragments. The accounted bytes do not change, so
  the byte cap cannot see this.
- **Copying, and freeing each column's original at once.** Measured ad hoc and not recorded in a
  CSV, this gave +14–17% on `text5date2`. The next column's exact copy lands inside the block just
  freed and splits it.
- **What the store does.** It copies every column that has spare capacity while the whole batch
  is still alive, and then frees the batch, so the copies sit side by side and the batch comes back
  as one region.

## Time

Other agents were building in other worktrees throughout. The representations whose code did not
change (`rows`, `batches`) moved by up to 30% between runs. Read the times as "no regression
visible", not as a speed-up.

| Shape, 1,000,000 rows | Append ms, prototype → implemented | Random read ns, prototype → implemented |
| --- | --- | --- |
| `numbers10` | 181.7 → 121.6 | 81.8 → 81.4 |
| `text5date2` | 216.2 → 195.6 | 177.7 → 166.4 |
| `s14` | 37.9 → 35.9 | 102.1 → 94.6 |
| `clob32k_inline` (100,000 rows) | 670 → 732 | 195 → 188 |

A read goes through `ResultSegment::value`, which builds a `CellValue`, where the prototype
matched on its storage directly. No cost from that is visible.

An earlier run checked every cell while the store was being built, and its random reads on 32 KiB
rows came out 15 times slower. That was the check, not the store. The bench now checks each
segment after everything else is measured.

**Through a real session (`mock-path.csv`, 1,000,000 rows, one fetch in flight, medians of 3):**

| Shape | Retained batches, total ms | Store, total ms | Fetch p50 µs (batches / store) | Store accounted bytes |
| --- | --- | --- | --- | --- |
| `s14` | 551.5 | 550.9 | 504 / 507 | 73,138,384 |
| `ids10` | 810.8 | 744.8 | 759 / 691 | 82,232,384 |

Compaction now runs on the worker, inside each fetch's latency, so the `compaction_ms` column is
empty for the store. The baseline's `store` rows compacted on the consumer thread. Its total times
(1,079 and 2,794 ms) came from a noisier moment of the machine, as its unchanged `batches` rows
(966 and 1,413 ms) show.

## Live, on 19c (`live-19c.csv`)

- **100,000 `CONNECT BY` rows**, with every column varying and every cell checked: 67.6 B/row
  accounted.
  - The first request was 63 rows, sized by declared widths: the text expression is described at
    4,000 bytes. Every later request was 1,000 rows.
  - The first run showed the lookup falling back to binary search, because the first segment
    differed from the rest. The first segment is now exempt from the uniform rule, and no second
    request is sent until the first is answered (`5a93636`). After that change the lookup is
    constant-time.
- **4 × `VARCHAR2(4000)` under a 16 MiB byte cap.** The store stopped at
  `AtLimit { Bytes, Unknown }` after 1,080 rows.
  - Requests were 16–17 rows. They vary with the observed width, so the lookup is a binary search
    here (accepted limitation 10).
  - The first page was 33 rows in 93 ms.
  - The statement carried no fetch-size hint, as `crates/ffi`'s execute does today. The wire array
    was therefore `oracledb`'s default of 100 rows, and two of the four columns repeat on every row
    (TTC compresses them). The cost of all-distinct wide rows under a 1,000-row array is in
    `../phase-1-m5-1-data/driver-fetch-probe.csv`.
- **`NUMBER`s.** Exact `NUMBER`s came back at scale 1 in the first segment and scale 2 after it,
  and they formatted identically to `Number`. `LEVEL/7` stayed a `Number`.
- **CLOBs.** CLOBs were read through handles on the worker, and NULL stayed NULL.
- **A typed `COMMIT`.** It ended an open result: `Ended { TransactionEnded }`, with the prefix
  readable and the LOB cells unavailable. The old LOB handle and the cursor both failed on the
  worker.
