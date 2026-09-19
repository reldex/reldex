# Phase 0 spike results — Oracle thin driver

Workstream A, ADR-0001 spikes S1–S5 (plus S7 and S9). Everything below was run
against the live Phase 0 test database on **2026-09-19**. Nothing here is
projected, estimated or inferred from documentation: each row names the test
that produced it, and every measurement says how it was taken.

Where something failed, it is written down as a failure.

> **Updated 2026-09-19, after the first run.** Two of the problems this
> document recorded have been fixed and the fixes re-verified against the same
> database: contract gap **C-1** (a REF CURSOR could not be consumed) is closed
> by an additive change to `db-driver-api`, so **S5 now passes in full**; and
> **U-3** (a named time-zone region aborts the process) has a mitigation after
> all — the driver refuses `TIMESTAMP WITH TIME ZONE` on the describe, before
> any value is decoded, so the crash becomes an ordinary error. The sections
> below say what changed and what it cost. Contract gap **C-2** is fixed in
> ADR-0002.

> **Updated again 2026-09-19, after an independent senior review.** The review
> found four must-fix defects in the driver and in this document. All four were
> verified against the source and against the live database before anything was
> changed, and all four were real:
>
> 1. **U-2's guard had a false negative** and the abort it was meant to prevent
>    was reproducible through it. The guard is now derived from the encoder and
>    proven complete over the whole shape space — see U-2.
> 2. **The U-3 containment was defeated on re-execution**, because a cached
>    server-side cursor takes a TTC path that ignores `prefetch_rows(0)`. Fixed
>    with `exclude_from_cache()` — see U-3.
> 3. **`(SELECT …)` in parentheses, `WITH` and comment-prefixed queries were
>    routed to the non-query path** and their rows discarded, because the
>    driver's first-keyword scan did not match upstream's. It now follows
>    upstream's rule exactly, with an independent transcription of it used as a
>    second opinion at run time; a mismatch is an internal error rather than
>    silently dropped rows.
> 4. **LOB streaming failures were reported as `DataConversion` with the session
>    `Usable`** even when the transport had gone. They are now mapped by cause
>    (`NetworkLost`/`Lost`, `Timeout`, closed connection, genuine decode
>    failure).
>
> Eleven smaller items were raised as should-fix; the ones that changed
> behaviour are recorded in place below. Two review findings were examined and
> **not** accepted: the claim that `SELECT 10/3 FROM dual` produces a bindable
> 40-digit/odd-index NUMBER (it produces 39 digits — see U-2), and the claim
> that ORA-00942 is mis-classified (see C-4).

---

## 1. Environment

| | |
|---|---|
| Machine / OS | Windows 11 Pro 10.0.26200, x86_64 |
| Toolchain | `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1`, MSVC target |
| Driver crate | `oracledb` **`=26.0.0-beta.3`** (exact pin) |
| TLS stack | `rustls 0.23.45` with the default `aws-lc-rs 1.18.1` provider |
| Database | Oracle Database 19c Enterprise Edition 19.0.0.0.0, Non-CDB, AL32UTF8 |
| Container | `doctorkirk/oracle-19c:19.3` as `reldex-oracle19c`, listener `127.0.0.1:1521`, service name `RELDEX` |
| Test user | `RELDEX_TEST` (credentials only via environment; see §7) |
| Date | 2026-09-19 |

### Why `=26.0.0-beta.3`

crates.io carries `26.0.0-beta.1` (2026-08-06), `beta.2` (2026-08-20) and
`beta.3` (2026-09-08) for Oracle's own `oracledb` crate; `beta.3` is the newest
and is what is pinned. (The unrelated `0.x` line is a different, community
crate and is not used.) The pin is **exact**, not a caret range, because the
crate is pre-GA and its documentation says the API is subject to change — and
because, as §5 shows, this wrapper works around behaviour that is an internal
detail rather than a promise. Upgrading is a deliberate act that must re-run
these spikes.

### Build notes

`aws-lc-rs` is the default `rustls` crypto provider and compiles C and assembly,
so it needs a C toolchain. The MSVC toolchain on this machine satisfied it and
**the build needed no intervention**: no provider override, no `ring` fallback,
no environment variables, no manual `cmake` setup. The dependency graph that
had to be built for it includes `aws-lc-sys 0.45.0`, `cc`, `cmake` and
`find-msvc-tools`, and those are the slow part of a cold build; a precise
cold-build time was **not measured** (the target directory is shared with
another workstream, so it could not be emptied). Incremental checks of this
crate alone are well under a second. The `ring` fallback was therefore
considered but not needed, and is **not** configured — it would be an unused
code path.

TLS itself is **not** exercised: the Phase 0 container has no TCPS listener, so
spike S8 has not run. The driver reports `tls = false` and refuses
`TlsMode::Required` rather than silently opening a plaintext connection. One
practical note for whoever runs S8: `rustls 0.23` needs a process-wide default
crypto provider, and `aws-lc-rs` installs one only when it is the single
provider compiled in. If a future dependency also pulls `ring`, connecting will
fail at run time with "no process-level CryptoProvider available" until
something calls `CryptoProvider::install_default`. The driver documents this and
does **not** install one itself — that is the application's choice to make once.

### Dependencies and licences

Measured on 2026-09-19 with `cargo tree -p reldex-driver-oracle-thin --target
x86_64-pc-windows-msvc` (`--edges normal` and `--edges normal,build`) and
`cargo metadata`, not from the crates.io pages:

| | |
|---|---|
| Third-party crates at **run time** | **55** |
| Additional crates needed only **to build** | **8** (`cc`, `cmake`, `jobserver`, `shlex`, `find-msvc-tools`, `dunce`, `fs_extra` — all pulled in by `aws-lc-sys`'s build script — plus `autocfg` for `num-traits`) |
| Third-party total | **63** (plus this crate and `reldex-db-driver-api`) |

(The review quoted 56 transitive crates. The measured run-time figure is **55**,
and 63 once the build-only crates are counted; the difference is which edges are
counted, not a disagreement about the graph.)

Every licence is permissive; there is no copyleft anywhere in the graph. The
distinct identifiers that appear are **MIT**, **Apache-2.0**, **ISC**,
**BSD-3-Clause** (`subtle`), **UPL-1.0** (`oracledb`, dual with Apache-2.0),
**CDLA-Permissive-2.0** (`webpki-roots`, which is a data set rather than code),
**BSL-1.0** (`whoami`, as one of three options), **CC0-1.0** and **MIT-0**
(`dunce`, build-only). 52 of the 63 are the plain `MIT OR Apache-2.0` dual
licence. The two compound strings belong to `aws-lc-rs` and `aws-lc-sys`, which
carry the licences of the vendored AWS-LC C sources.

Two consequences worth recording now rather than at release time:

- **A closed-source Pro edition is unobstructed, but attribution is not
  optional.** MIT, BSD-3-Clause, ISC, Apache-2.0 and UPL-1.0 all require their
  licence text and copyright notices to be reproduced in the distributed
  product. Reldex needs a generated third-party notices file (`cargo about` or
  equivalent) in its release build, covering the **transitive** set above and
  not just direct dependencies.
- **`aws-lc-sys` is the only C/assembly build in the graph**, and it is the only
  crate with a `links` key besides its own wrapper. It is what makes a cold
  build slow and what makes a C toolchain (and `cmake`) a prerequisite on every
  build machine. It arrives through `rustls`'s default provider, which arrives
  through `oracledb`. If pure-Rust builds ever matter more than the provider
  choice, this is the single edge to cut.

---

## 2. Results at a glance

| Spike | Verdict | One-line summary |
|---|---|---|
| S1 connect / auth | **Pass** | Easy Connect and full descriptor both work; failures classify correctly |
| S2 type fidelity | **Partial — three critical upstream defects** | Reads are exact, including Thai and non-BMP text; *binding* a NUMBER is unsafe and is now refused for the affected values, and `TIMESTAMP WITH TIME ZONE` is refused on the describe because one form of it aborts the process (U-3) |
| S3 session / transaction | **Pass** | Auto-commit off, savepoints, DDL commit reporting and session isolation all behave |
| S4 cancellation | **Fail for Reldex's requirement** | No mechanism stops a statement *and* keeps the session, in the general case |
| S5 PL/SQL | **Pass** | Everything works, REF CURSOR included, after the additive contract change C-1 |
| S7 LOB streaming | **Pass** | 100 MB CLOB and BLOB streamed with +4.7 MB working set |
| S9 concurrency | **Pass** | 8 concurrent sessions, 400 inserts, 283 ms |
| S6, S8 | **Not run** | Out of scope for this workstream (S6 metadata) / no TLS listener (S8) |

Test counts, as measured on **2026-09-19** after the review fixes:

| | |
|---|---|
| DB-free, `cargo test -p reldex-driver-oracle-thin -p reldex-core-poc` | **65 unit tests** + **1 documentation test**, all pass (1 further doc test is `no_run`/ignored by design). `reldex-core-poc` is a binary with no tests of its own |
| Opt-in, `--features oracle-it` | **54 integration tests** — **51 run and pass**, **3 are `#[ignore]`d** |
| Per file | `s1_connect` 8, `s2_fidelity` 17 (14 + 3 ignored), `s3_session` 6, `s4_cancel` 7, `s5_plsql` 12, `s7_lob_and_s9_concurrency` 4 |

The three ignored tests all **abort the process on purpose**, each recording a
defect that a default in this driver now prevents, and each is the proof that
the corresponding guard is load-bearing rather than decorative. Run them one at
a time with `-- --ignored --exact <name>`; they do not report a failure, they
end the process.

| Ignored test | What it records | What prevents it |
|---|---|---|
| `a_named_time_zone_region_is_read_or_reported` | U-3: decoding a region-encoded `TIMESTAMP WITH TIME ZONE` hits a `todo!()` | the describe-time column refusal |
| `a_cached_cursor_makes_the_execute_fetch_rows_and_aborts` | U-3 again, on **re-execution**: a cached cursor takes the re-execute path, which ignores `prefetch_rows(0)`, so rows arrive and are decoded before any check | `Statement::exclude_from_cache()` |
| `binding_forty_digits_with_an_odd_index_aborts_upstream` | U-2: 40 digits with an odd, positive decimal-point index reads past the encoder's digit buffer | `binds::encoder_defect` |

A workspace-wide count is deliberately **not** quoted here any more: another
workstream is editing `db-core`, `drivers/mock` and `db-driver-api` in parallel,
so a `cargo test --workspace` total taken from this branch would be stale the
day it was written. The per-crate numbers above are what this document is
accountable for.

> **Run S4 with `--test-threads=1`.** Its seven tests include three long
> cartesian joins, a `KILL SESSION` and a 20-second PL/SQL sleep; run in
> parallel against this single-instance container they load the server enough
> that the deadline-recovery outcome flips (see U-6, which is load-dependent by
> construction). Serial runs are stable. This is pre-existing and not caused by
> any change recorded here — see the control runs under U-6.
>
> ```text
> tools/oracle-test-db/run-it.ps1 s4_cancel -- --test-threads=1
> tools/oracle-test-db/run-it.sh  s4_cancel -- --test-threads=1
> ```
>
> (The PowerShell runner now inserts cargo's `--` separator itself: Windows
> PowerShell 5.1 swallows a bare `--` before the script sees it, so the
> documented invocation used to fail there while working under `pwsh`.)

---

## 3. Per-spike detail

### S1 — connect, authenticate, ping, close — **Pass**

*Kill criterion: no pure-Rust path can authenticate against Oracle 19c.* Not
triggered.

| Check | Verdict | Evidence |
|---|---|---|
| Easy Connect with a **service name** | Pass | `easy_connect_with_a_service_name_authenticates` — `127.0.0.1:1521/RELDEX` |
| Full TNS descriptor | Pass | `a_full_tns_descriptor_authenticates_too` |
| Wrong password → authentication error | Pass | `a_wrong_password_is_...` → `ErrorKind::Authentication`, ORA-01017, `SessionState::Usable`, and the password appears in neither `Display` nor `Debug` |
| Unreachable port → connection error | Pass | `an_unreachable_port_is_a_connection_failure` → `ErrorKind::Connection` after 3.0 s |
| `ping` | Pass | same tests |
| Close, then use a derived handle | Pass | `a_cursor_outliving_its_connection_reports_rather_than_panics` → `DriverInternal` + `SessionState::Lost`, no panic |

**Measurements** (`connection_latency_is_measured_over_ten_attempts`; ten
sequential connect/ping/close cycles, `Instant::now()` around each,
median of the sorted samples):

| | |
|---|---|
| connect, median | **119.5 ms** |
| connect, min / max | 118.6 ms / 123.7 ms |
| `ping`, median | **769 µs** |

Connect cost is dominated by the TTC handshake and O5LOGON authentication
round trips; it is the number that decides whether Reldex can open a session
per tab lazily or must pool. No comparison with any other driver is made.

> **Driver change this spike forced.** A refused TCP connection surfaces as
> `StreamOperation`, the same upstream kind as a mid-session drop, so the naive
> mapping called it `NetworkLost` — a *lost session* for a session that never
> existed. `error::map_connect` now reclassifies transport failures raised
> during `connect` as `ErrorKind::Connection`.

### S2 — type fidelity — **Partial**

*Kill criteria: silent NUMBER precision loss, or Thai/NCHAR corruption.* The
character criterion is **clear**. The NUMBER criterion **was triggered** — by
the upstream crate, on the bind path — and is now contained by refusing the
affected values rather than storing them wrongly.

| Check | Verdict | Evidence |
|---|---|---|
| NUMBER, 38 / 39 / 40 significant digits | Pass **as bound** | `number_values_survive_...`; the three literals used (`123…78`, `123…789`, `123…7891`) round-trip exactly and match the server's own `TO_CHAR(v,'TM')`. This is not a blanket "40 digits are fine": a **40**-digit value whose decimal point sits at an **odd, positive** index is refused on the bind, because that is the shape the upstream encoder reads past its buffer on (U-2). 38 and 39 digits are always safe |
| NUMBER, `1/3` | Pass | `0.3333333333333333333333333333333333333333` (40 digits, index 0 — even, so bindable), identical to `TO_CHAR` |
| NUMBER, `0`, `1`, `-1`, `.5`, `-0.5`, `123.45`, `0.005`, `1E-129` | Pass | same test |
| NUMBER, `1E39`, `9.99E39`, `1234567890…890` (40 digits, index 40) | Pass **as bound** | `a_forty_digit_bind_with_an_odd_decimal_point_index_is_refused_rather_than_aborting`, second half: magnitude alone is not what the guard keys on — `9.99E39` is larger than every refused value and binds exactly |
| NUMBER, `9.99…E125` (126 digits) and its negative | Pass **on read** | inserted as a literal, read back digit-for-digit |
| NUMBER, `9.99…E125` **bound**, and `1.234…891` (40 digits, index 1) | **Refused** | upstream aborts the process (U-2); the driver refuses with `ErrorKind::Unsupported` |
| NUMBER, `0.05`, `0.0005`, `0.0123`, `0.000123`, `1E-4`, `1E-130`, `-0.05` **bound** | **Refused** | upstream stores them ten times too large (U-1); the driver refuses |
| `CHAR(20 CHAR)` blank padding | Pass | padding preserved, not stripped |
| `VARCHAR2` / `NVARCHAR2`, Thai `ทดสอบภาษาไทย` | Pass | byte-exact; server `DUMP(v,1016)` confirms 36 bytes AL32UTF8 |
| `VARCHAR2` / `NVARCHAR2`, non-BMP `🐘` | Pass | 4 bytes, `f0,9f,90,98`, exact |
| Mixed `a🐘ทb` | Pass | 9 bytes, exact |
| `NCHAR(20)` padding with a non-BMP character | Pass | padded to 20 **UTF-16 code units** (18 spaces after the emoji), not 20 characters — the server's rule, preserved |
| DATE with time of day | Pass | `2026-09-19T13:45:30` |
| DATE, BC (`-0044-03-15`) | Pass | returned, not rejected |
| DATE, `1500-02-29` (Julian) | Pass | returned, not rejected |
| TIMESTAMP(9) | Pass | `2026-09-19T13:45:30.123456789` |
| TIMESTAMP WITH TIME ZONE, numeric offset | Pass **after a driver fix**, behind an opt-in | `2026-09-19T13:45:30.123456789+07:00` — read with the `oracle.allow_timestamp_with_time_zone` extension set, because the column is refused by default (U-3) |
| TIMESTAMP WITH TIME ZONE, **named region** | **Refused, cleanly** | the column is refused on the describe with `ErrorKind::Unsupported`, before any value is decoded; decoding one still aborts the process upstream, which is what the refusal prevents. See U-3 |
| TIMESTAMP WITH TIME ZONE, any form, by default | **Refused** | `a_timestamp_with_time_zone_column_is_refused_before_anything_is_fetched`; the session stays `Usable` and `TO_CHAR(c, '… TZR')` reads the value as text |
| RAW | Pass | `00FF107F80DEADBEEF` byte-exact |
| NULLs of 10 types | Pass | every nullable column reads back `ValueRef::Null` |
| `INTERVAL DAY TO SECOND`, `INTERVAL YEAR TO MONTH`, `ROWID`, `TIMESTAMP WITH LOCAL TIME ZONE` | Pass | rendered as `ColumnData::Unsupported` text (`P3DT0H0M0.000000000S`, `P2Y6M`, `AAAACPAABAAAAWRAAA`, and the LTZ as `YYYY-MM-DDThh:mm:ss.nnnnnnnnn` with no zone suffix) with the server's own type name; the columns either side of them still arrive |
| `XMLTYPE` (and, by the same route, `JSON`, `VECTOR`, object types, `BFILE`) | **Refused on the describe** | `a_column_this_upstream_cannot_decode_is_refused_before_the_first_batch`. These are *not* the "renders as text" case above: upstream's `DbValue::from_response` has no branch for them, so the **fetch** fails and there is nothing to render. Failing from `fetch_batch` would kill a result set after earlier batches had been handed to the caller, so the column is refused before the first batch. Session stays `Usable`; `XMLSERIALIZE` works immediately afterwards. (19c has no native `JSON` column type — JSON there is `VARCHAR2`/`CLOB`/`BLOB` with `IS JSON`, all of which work) |
| `TIMESTAMP WITH LOCAL TIME ZONE` renders **without** a `Z` | Pass **after a driver fix** | the first version of this document recorded `2026-…Z`. That was upstream's `Display`, which appends `Z` whenever the offset fields are zero — which is how this type always arrives, because the server normalizes it to the **database** time zone and sends no offset. `Z` would assert UTC on no evidence. `value::render_local_time_zone` now writes the bare civil fields; asserted by `a_type_the_contract_cannot_hold_becomes_text_instead_of_failing_the_batch` |

No `NLS_LANG` or other NLS environment variable was set on the client for any
of this. Correctness was established by comparing against the server's own
`DUMP`, `LENGTHB` and `TO_CHAR` output, not against the driver's own opinion.

> **Driver changes this spike forced.** (1) A `TIMESTAMP WITH TIME ZONE` is
> carried on the wire as **UTC** fields plus the offset, and `oracledb` returns
> both unchanged; pairing them as they stand named a different instant
> (`13:45:30 +07:00` came back as `06:45:30 +07:00`). `value::to_timestamp` now
> applies the offset, and `binds::to_oracle_timestamp` applies its inverse.
> (2) Precision and scale are no longer reported for character and binary
> columns, where Oracle sends `0, 0` and passing it through stated a decimal
> precision of zero rather than "none".
> (3) **The driver now describes before it fetches** and refuses a
> `TIMESTAMP WITH TIME ZONE` column, which is the U-3 mitigation; see U-3 for
> what it costs and why it is per-column.

### S3 — session and transaction semantics — **Pass**

*Kill criterion: auto-commit cannot be turned off.* Not triggered.

| Check | Verdict | Evidence |
|---|---|---|
| Auto-commit is off by default | Pass | `nothing_commits_until_it_is_asked_to`: an inserted row is invisible to a second session until `commit` |
| Uncommitted state persists across statements | Pass | two inserts, both invisible to the reader, both visible to the writer |
| `commit` publishes; `rollback` discards | Pass | same test |
| SAVEPOINT + ROLLBACK TO | Pass | `a_savepoint_can_be_rolled_back_to_...`: one statement undone, the transaction kept open (the reader still sees nothing) |
| Savepoint names cannot inject | Pass | `SavepointName::new("a; DROP TABLE x --")` is rejected by the contract |
| DDL commits and is reported | Pass | `ddl_commits_on_the_server_and_the_driver_says_so`: `CREATE INDEX` → `StatementKind::Ddl`, `committed_implicitly() == true`, the pending INSERT became visible |
| `ALTER SESSION` does **not** commit | Pass | same test → `StatementKind::SessionControl`, pending work still invisible |
| NLS setting does not leak between sessions | Pass | `session_state_does_not_leak_between_connections` |
| Global temporary table rows do not leak | Pass | same test |
| Package state does not leak | Pass | same test (counter is 2 in one session, 0 in the other) |
| Close over an open transaction rolls back | Pass | `closing_a_connection_with_an_open_transaction_rolls_back_rather_than_commits` |
| Transaction state is conservative | Pass | `Inactive` after connect / DDL / commit / rollback, `Unknown` after DML or a query |

### S4 — cancelling a running statement — **Fail for the requirement**

This is the spike the decision rests on. It is written up in full in §4.

### S5 — PL/SQL — **Pass**

| Check | Verdict | Evidence |
|---|---|---|
| Anonymous `BEGIN` / `DECLARE` block | Pass | `an_anonymous_block_runs_and_is_classified_as_plsql` |
| `CALL` | Pass | classified `PlSqlBlock`, OUT bind returned |
| OUT binds: NUMBER, VARCHAR2, DATE | Pass | `out_and_in_out_binds_carry_...` |
| IN OUT bind | Pass | `'in and out'` → `'in and out!'` |
| Positional OUT binds | Pass | `positional_out_binds_work_too`; input binds correctly produce no output slot |
| Repeated execution of an OUT-bind statement (upstream #17) | **Pass — no regression** | `an_out_bind_can_be_executed_repeatedly_without_losing_its_value`: six consecutive executions, every value correct |
| Stored procedure, standalone function, packaged function | Pass | `a_stored_procedure_a_function_and_a_package_can_all_be_called` |
| DBMS_OUTPUT round trip | Pass | `["first line", "ทดสอบภาษาไทย"]` — including Thai through an OUT bind |
| PL/SQL compile error detected | Pass | `CREATE PROCEDURE` with a bad body → `compiled_with_errors() == true`, `WarningKind::CompiledWithErrors` |
| `USER_ERRORS` lookup | Pass | `PLS-00201: identifier 'THIS_DOES_NOT_EXIST' must be declared` at line 1, column 34 |
| Error position for a failing block | Pass | ORA-06550 → `SqlPosition` line 2, column 3 |
| **REF CURSOR OUT bind** | **Pass**, after contract change C-1 | `a_ref_cursor_out_bind_is_fetched_while_the_parent_connection_stays_usable`: opened and described correctly (columns `ID`, `LABEL`, types right), then **fetched to exhaustion** — 5 rows in 3 batches of at most 2 — with the parent connection answering `SELECT 42 FROM dual` between every batch, then closed |
| REF CURSOR, taken twice | Pass | the second `take_named("rc")` returns `None` and the slot reads back as `Value::Taken`, never as SQL NULL: one live cursor cannot be owned twice |
| REF CURSOR after its connection closes | Pass | `a_ref_cursor_outliving_its_connection_reports_rather_than_panics`: `fetch_batch` returns `driver-internal: cursor was used after its connection was closed` with `SessionState::Lost`, and does not panic or touch the socket. `close` is a successful no-op, the same answer S1 records for a top-level cursor |
| REF CURSOR through `DatabaseSession` | Pass | `crates/db-core/tests/out_values.rs` (mock driver): the nested cursor stays on the session's worker thread and surfaces as an ordinary `ResultSetId` |

### S7 — large-object streaming — **Pass**

100 MB written server-side into a `CLOB` and a `BLOB`, then read through
`LobStream::read_chunk` with a 64 KiB caller buffer.

| | |
|---|---|
| Object size | 104 857 600 bytes each |
| CLOB: bytes read / chunks / time | 104 857 600 / 4 801 / **18.2 s** |
| BLOB: bytes read / chunks / time | 104 857 600 / 1 600 / **3.2 s** |
| Process working set before | 14 172 KB |
| Process working set peak | 18 840 KB |
| **Growth while streaming 200 MB** | **4 668 KB** |

*Method:* working set read from `tasklist /FI "PID eq <self>" /FO CSV /NH`
before the first read and every 200 chunks; peak is the maximum sample. The
test asserts growth stays under 32 MB, which it does by a wide margin.

The CLOB needs three times as many chunks as the BLOB for the same byte count
because `oracledb`'s character read sizes its request as `buf.len() / 3` UCS-2
units, so a 64 KiB buffer carries about 21 KiB of UTF-8 per round trip. That is
the cost of the safe path, not a defect. A large object is delivered as a
locator (`ValueRef::Lob`), never materialised into the batch, and an exhausted
stream keeps answering `0` rather than failing.

`a_lob_read_after_its_connection_closes_reports_rather_than_panics` confirms the
handle-lifecycle rule: after the connection closes, the stream reports
`driver-internal: LOB stream was used after its connection was closed` and does
not touch the socket.

> **Corrected 2026-09-19.** Every failure during a LOB read used to be reported
> as `ErrorKind::DataConversion` with the session `Usable`. That is a lie when
> the transport is gone — a dropped connection mid-stream told the caller the
> data was malformed and the session was fine, so a connection pool would hand
> the dead session straight back out. The cause is upstream's
> `Lob::io_error`, which collapses every error into
> `io::Error::other(text)`, losing the kind. `error::map_lob_read` now
> reclassifies by cause: an `ORA-`/`PLS-` code goes through the ordinary server
> mapping; upstream's fixed transport strings become `NetworkLost` with
> `SessionState::Lost`; "not connected to database" becomes a closed-connection
> error; a fired call timeout becomes `Timeout`; genuinely undecodable UTF-16
> stays `DataConversion`; anything unrecognised is an internal error rather
> than a wrong guess. *Evidence:*
> `a_lob_read_that_lost_the_connection_does_not_claim_the_session_is_fine`,
> `the_upstream_split_surrogate_failure_is_recognized`.

### S9 — concurrency — **Pass**

Eight threads, one connection each, 50 inserts and a commit and a read-back per
thread: **282.8 ms** wall clock for 400 inserts across 8 sessions, every
thread seeing exactly its own 50 rows.

### Review fixes with observable behaviour

Beyond U-2, U-3 and the LOB mapping (recorded in §5 and below), the review
produced these behaviour changes. Each has a test; most need no database.

| Change | Why | Evidence |
|---|---|---|
| The first SQL keyword is now found by **upstream's own rule** — skip comments, quoted text and any non-alpha character, then take the first maximal ASCII-alpha run | `(SELECT …)`, `WITH …`, and anything behind a comment or hint were classified as non-queries, sent to `execute_non_query`, and **their rows were discarded**. Upstream's `determine_statement_type` would have executed them as queries | `a_parenthesised_query_is_a_query_and_keeps_its_rows`, `comments_and_hints_before_the_first_keyword_are_skipped`, `the_first_keyword_is_not_confused_by_a_later_one` |
| A second, independent transcription of that rule (`classify::upstream_would_return_rows`) is checked against the classifier over a corpus, and again at run time before the discarding path is taken | The two implementations exist to disagree. A disagreement is now an internal error, never silently dropped rows | `the_classifier_and_the_upstream_rule_agree_on_every_statement`, `a_statement_that_returns_rows_is_never_sent_down_the_discarding_path` |
| Statement caching is **off** by default (`set_stmtcachesize(0)`), opt-in via the `oracle.statement_cache_size` extension, with the reason in the extension's documentation | A cached cursor defeats the U-3 containment (see U-3). Turning it on is a deliberate choice with a documented consequence | `the_statement_cache_is_off_unless_the_caller_turns_it_on` |
| A `DML … RETURNING` row is read from `returned_data()`, with `out_bind_data()` only as the PL/SQL fallback; more than one returned row is refused as `Unsupported` | The two upstream accessors carry different things, and reading the wrong one gave all-NULL outputs for DML RETURNING | `a_single_row_dml_returning_gives_back_its_values` |
| The number of OUT slots the server actually produced is probed and compared with what the caller declared, and a mismatch fails loudly | A bind the caller declared IN but the server treats as IN OUT shifts every later slot, so values would be read from the wrong variable | `an_in_bind_the_server_calls_in_out_fails_loudly_instead_of_shifting_slots` |
| Host and service name in an Easy Connect endpoint are validated (`[A-Za-z0-9._-]`, non-empty, ≤255) | A host field carrying `)` or `(` could smuggle a TNS descriptor fragment into the connect string | `a_host_or_service_that_could_carry_a_descriptor_is_refused` |
| Changing a statement's deadline while a result set is open produces a `Warning` on the outcome | The armed timeout is per **connection**, so a new deadline silently re-arms the socket under an open cursor | `changing_a_deadline_under_an_open_result_set_is_reported` |
| `CancelOutcome::NotInterruptible` reports `deadline_remaining: None` when the work spans several round trips, rather than a number it cannot honour | The armed value is per round trip; quoting it for a multi-batch fetch would be a promise the driver cannot keep. The contract documents `None` as "cannot measure" | `no_stop_time_is_promised_for_work_that_spans_several_round_trips` |
| `LobStream::size_hint` returns `Some` only for a binary LOB (`BLOB`) | The contract documents the hint in **bytes**; upstream counts a character LOB in UCS-2 units, which is wrong by up to 4× | crate docs + `reldex-core-poc` prints "(size not known in bytes)" |
| A LOB locator allocates its staging buffer on first read | A batch of unopened locators used to allocate one buffer each | `a_batch_of_unread_lob_locators_costs_almost_nothing` |
| Dropping a connection marks it closed, so handles that outlive it report instead of touching a dead socket | `close()` did this; `drop` did not | `a_handle_outliving_a_dropped_connection_reports_too` |

### S6, S8 — **Not run**

S6 (metadata queries) belongs to a later slice and was not attempted. S8 (TLS)
cannot be attempted: the Phase 0 container has no TCPS listener. The driver
reports `tls = false` accordingly.

---

## 4. S4 in full — can a running statement be stopped?

Four candidates, evaluated in the order ADR-0001 sets out.

### Candidate 1 — a deadline armed before the call (`set_call_timeout`)

*Works for SQL. Destroys the session for anything the server will not interrupt.*

| Case | Armed | Statement stopped after | Result |
|---|---|---|---|
| Long **SQL** (3-way cartesian join over `ALL_OBJECTS`) | 2.0 s | **2.5 s** (overshoot 534 ms) | `ErrorKind::Timeout`, `SessionState::NeedsValidation`, **session survives** — `ping` and a query both succeed |
| **PL/SQL** `DBMS_SESSION.SLEEP(20)` | 2.0 s | **4.0 s** | `ErrorKind::NetworkLost`, `SessionState::Lost`, **connection closed and gone** |

*Tests:* `a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly`
(renamed from `…_and_the_session_survives`, which asserted an outcome that is
load-dependent — see U-6),
`a_deadline_on_a_plsql_sleep_destroys_the_session`.

> *Note added 2026-09-19.* Since the U-3 mitigation the driver defers the fetch,
> so a query's rows — and therefore the moment its deadline fires — arrive on
> the first `fetch_batch` rather than on `execute` (the statement is still
> *executed* on `execute`; only the row transfer moves). The SQL case above is
> now driven through `run_to_first_batch`. The mechanism, the outcomes and the
> numbers are unchanged. Note also that which outcome the SQL case produces is
> **load-dependent** (see U-6): these measurements are from a serial run, and
> the test now accepts either documented outcome while requiring the driver to
> report it honestly.

The difference is explained by upstream's recovery path, and it is a defect
(U-6). On a read timeout, `Client::receive_data_packet` calls
`recover_from_error`, which sends a TTC **interrupt** marker and then calls
`reset` — and `reset` reads the reply *from the same socket with the same
timeout still armed*. When the server answers promptly (a SQL statement it can
interrupt) the reset completes inside the remaining window and the session is
fine. When the server will not answer until the call ends (a PL/SQL sleep) the
reset read times out too, `unrecoverable_error` closes the transport, and the
caller gets a lost connection instead of a timeout. The 4.0 s figure for the
2.0 s deadline is exactly this: one timeout for the call, one for the failed
reset.

A deadline also cannot be *changed* once the call is running, which is the
other half of candidate 1 and is measured rather than assumed:

*Test:* `asking_the_upstream_client_to_arm_a_deadline_mid_call_blocks_until_the_call_ends`.
`oracledb::Connection::set_call_timeout` takes `&self` and immediately does
`self.client_ref.lock().unwrap()`; `Client::perform_round_trip` holds that same
mutex for the whole request/response cycle. Called from a second thread 500 ms
into a 5-second statement, `set_call_timeout` **blocked for 4.8 s** — the entire
remainder of the call. There is no way to deliver anything to a running call
through the public API. (This is the one test that uses `oracledb` directly,
because the fact is invisible through this crate's own API.)

The driver therefore reports `CancelKind::PreArmedDeadline`, and
`CancelHandle::request_cancel` returns `CancelOutcome::NotInterruptible` with
the time left on the armed deadline. Measured while a statement was running,
that call answers in **at most 3.1 µs** and never touches the connection's lock
(`the_cancel_handle_answers_immediately_while_a_statement_is_running`).

### Candidate 2 — a privileged control session (`ALTER SYSTEM CANCEL SQL`)

*Works on the server. The client never finds out.*

*Test:* `a_privileged_cancel_reaches_the_server_but_not_the_client`. A second
connection as `SYSTEM` locates the working session by a unique
`DBMS_APPLICATION_INFO.SET_CLIENT_INFO` tag and issues
`ALTER SYSTEM CANCEL SQL 'sid, serial#, @inst, sql_id'`.

| | |
|---|---|
| `ALTER SYSTEM CANCEL SQL` returned in | **2.7 ms** |
| Server stopped the call (session left `ACTIVE` in `V$SESSION`) after | **507 ms** |
| The statement's own connection returned after | **23.0 s** — its 20-second safety deadline |
| What that connection reported | `NetworkLost`, session lost. **Never ORA-01013** |

So the cancel is delivered and honoured: half a second after the request, the
server has abandoned the statement. But the client sits in its socket read
until its own deadline expires and then declares the connection unrecoverable.
Without a pre-armed deadline it would wait forever. This is recorded as U-7;
the likely path is the marker handling in `Client::receive_packet`/`reset`,
which is stated here as a hypothesis, not a finding — what is measured is that
the server stopped and the client did not notice.

For contrast, `ALTER SYSTEM KILL SESSION '<sid,serial#>' IMMEDIATE` **is**
noticed: the control statement returns ORA-00031 ("session marked for kill"),
the server marks the session `KILLED` in 508 ms, and the client's call returns
after **5.2 s** with ORA-00028 `your session has been killed`, session `Lost`
(`killing_a_session_is_a_different_thing_with_different_consequences`). That is
not a cancel: it ends the session, rolls its transaction back and costs the
user everything uncommitted. Reldex must never offer it as "stop this
statement".

Candidate 2 therefore does not work either, and it additionally needs `ALTER
SYSTEM` — a privilege no ordinary Reldex user will have. It remains
interesting only as an opt-in escape hatch for a DBA, and only once U-7 is
fixed.

### Candidate 3 — what a public upstream break API would need

Reading `oracledb` 26.0.0-beta.3:

- `Client` already knows how to send a break: `constants::MARKER_TYPE_INTERRUPT`
  is defined and used by `recover_from_error`, and `_MARKER_TYPE_BREAK` (1) is
  defined but unused. The wire format is a one-packet marker message.
- What is missing is *reach*. `Connection` owns `Arc<Mutex<Client>>` and every
  public method locks it; `Client` owns the `Transport`, which owns the socket.
  There is no handle that can touch the socket while a round trip holds the
  mutex, and no public method that is documented to be callable concurrently.
- A break must be written on the socket **outside** the mutex, because the
  thread that would send it is by definition not the thread holding the lock.

The drafted issue text is in §6.

### Candidate 4 — a minimal fork exposing a break handle

*Assessment only. Nothing was vendored into this repository, and no fork was
built.*

The change is small in principle and awkward in practice:

- **What would have to change.** `Transport` would need to expose a cloned
  write half — `TcpStream::try_clone()` — captured at connect time into a
  `BreakHandle { socket: TcpStream }` that `Connection` can hand out. The handle
  would serialise a marker packet (packet type 12, one data byte
  `MARKER_TYPE_INTERRUPT`) and write it directly, taking no lock. The reading
  thread already copes with what comes back, because `receive_packet` handles an
  inbound marker by resetting.
- **Size.** Roughly 60–100 lines across `transport.rs`, `client/mod.rs` and
  `connection/mod.rs`, plus the marker-packet serialisation that already exists.
- **Why it is not small in practice.** TLS. The plaintext path is a `TcpStream`
  and can be `try_clone`d; the TCPS path is a `rustls::StreamOwned`, which owns
  the connection state and **cannot** be cloned or written to from a second
  thread without a lock over the TLS session. A break over TLS therefore needs
  either a second `Mutex` around only the TLS write half (which the round trip
  would have to be careful never to hold across a read) or a dedicated writer
  task. That is a real design change to the upstream crate, not a patch.
- **And it would not be enough on its own.** U-6 (recovery reads with the
  timeout still armed) and U-7 (a server-side cancel is not observed) would
  still need fixing, or a break would leave the session in the same unusable
  state a deadline does today.

A fork is therefore **not recommended**: it would carry the maintenance cost of
a private branch of a pre-GA Oracle crate, would not fully solve the problem,
and would have to be rebased onto every beta.

### S4 conclusion

**The driver reports `CancelKind::PreArmedDeadline` today, and `SPEC.md` §24.8
cannot be met with `oracledb` 26.0.0-beta.3 — not by this wrapper, not with the
privileged extra, and not by a small fork.** A deadline armed before a call is
the only mechanism that works at all, and it works only for statements the
server will interrupt promptly: a long SQL statement stops 0.5 s past its
deadline with the session intact, but a PL/SQL block the server will not
interrupt costs the connection, because upstream's recovery reads the reset
reply with the expired timeout still armed. A privileged `ALTER SYSTEM CANCEL
SQL` does stop the statement server-side in about half a second, but the client
never receives the ORA-01013 and hangs until its own deadline, so it is not a
cancel the application can observe; `KILL SESSION` is observed but destroys the
session and rolls the transaction back. The upstream crate has the interrupt
marker and uses it internally, but exposes no break API, and its single
round-trip mutex means one cannot be added from outside — measured: arming a
deadline mid-call blocked 4.8 s of a 5.3 s statement. Meeting §24.8 needs
**upstream work** — a public break handle plus fixes for U-6 and U-7 — and the
honest interim position is that Reldex tells the user up front that a running
statement can only be stopped by a limit set before it starts.

---

## 5. Upstream defects and gaps

Each entry names the source location in
`oracledb-26.0.0-beta.3`, a minimal reproduction, and what this driver does
about it.

### U-1 — **A bound NUMBER can be stored ten times too large, silently** (critical)

`src/ora_type/number.rs:314`:

```rust
if decimal_point_index % 2 == 1 {
    prepend_zero = true;
    ...
}
```

`decimal_point_index` is `i16`. For a value below `0.1` with an **odd** number
of leading zeros after the decimal point it is negative and odd, and in Rust
`-1 % 2 == -1`, not `1`. The branch is skipped, the base-100 digit pairs are
written one place out, and the server stores a different number. Nothing raises
an error.

Minimal reproduction (bind, then read back):

| Bound | Stored |
|---|---|
| `0.05` | `0.5` |
| `0.0005` | `0.005` |
| `0.0123` | `0.123` |
| `0.000123` | `0.00123` |
| `1E-4` | `0.001` |
| `1E-130` | `1E-129` |
| `-0.05` | `-0.5` |

Their even-leading-zero neighbours (`0.5`, `0.005`, `0.00005`, `0.123`,
`0.00123`, `1E-3`, `1E-129`, `-0.005`) are all stored correctly, which is what
makes this so easy to miss.

*Evidence:* `a_bound_number_is_never_silently_scaled_by_a_power_of_ten`.
*Mitigation:* `binds::encoder_defect` refuses these values with
`ErrorKind::Unsupported` and a message telling the caller to write the value as
a literal. It is one predicate covering both this defect and U-2 — an odd
decimal-point index either prepends the alignment zero (safe unless the digit
count reaches 40, which is U-2) or is silently skipped (this defect). **This is a real functional limitation** — half of all decimals below
0.1 cannot be bound — but a database tool that stores the wrong number is worse
than one that says no.

### U-2 — **A NUMBER of the wrong *shape* aborts the process** (critical)

> **Corrected 2026-09-19.** This section previously said "anything of magnitude
> ≥ 1E40", and the driver's guard was written from that description. Both were
> wrong: magnitude is not the trigger, and the guard had a false negative that
> aborted the process. What follows is derived from the encoder, not from
> examples.

`src/ora_type/number.rs:280-283` folds trailing zeros into `num_digits` without
bounding it to the 40-byte `digits` array; `to_buf` then walks base-100 **pairs**
over that array and can index one past its end (line 347/351).

Two inputs reach the out-of-bounds read:

1. `num_digits > 40` — the fold above, e.g. `1E40` or
   `9.9999999999999999999999999999999999999E125`; and
2. `num_digits == 40` with an **odd, positive** `decimal_point_index` — `to_buf`
   then prepends an alignment zero, so 40 digits occupy 41 positions and the
   last pair reads `digits[40]`. `1.234567890123456789012345678901234567891`
   is of magnitude **one** and aborts; `9.99E39` is far larger and is fine.

```
panicked at src/ora_type/number.rs:351:38:
index out of bounds: the len is 40 but the index is 40
```

That panic then meets U-4 and the **process aborts** with
`STATUS_STACK_BUFFER_OVERRUN (0xC0000409)`. Both cases were reproduced against
the live database before the guard was rewritten.

*Mitigation:* `binds::encoder_defect` refuses exactly the shapes above
(`ErrorKind::Unsupported`, session `Usable`), sharing its predicate with U-1's
odd-index case. `the_refusal_predicate_covers_every_shape_the_encoder_mishandles`
enumerates every (digit count 0..=130 × decimal-point index −129..=126) pair —
the whole space a `Number` can occupy — and checks the predicate against
line-by-line transcriptions of upstream's `from_str` and `to_buf`, so the
refusal is complete by construction rather than by example. Reading such values
is unaffected and exact (verified to 126 digits).

*Evidence:* `a_forty_digit_bind_with_an_odd_decimal_point_index_is_refused_rather_than_aborting`
(live, passing) and `binding_forty_digits_with_an_odd_index_aborts_upstream`
(`#[ignore]`d — it drives the upstream crate directly and ends the process,
which is the proof that the refusal is load-bearing).

*How reachable is it?* The 40-digit/odd-index class cannot come back from the
**server**: Oracle's NUMBER holds 20 base-100 pairs, and a value with an odd
decimal-point index spends one position on the same alignment zero, so the
server never returns 40 significant digits *and* an odd index together. Probing
`10/3`, `100/3`, `1/7`, `1000/7` and `2/3` confirms it — `10/3` comes back as 39
digits, and all five re-bind cleanly. The class is reachable from a value the
**user typed** or Reldex computed, which is exactly the path an IDE exposes.

### U-3 — **A named time-zone region aborts the process** (critical)

`src/ora_type/timestamp.rs:238`:

```rust
if buf[11] & 0x80 != 0 {
    todo!();
}
```

The high bit marks a region-encoded time zone. Minimal reproduction:

```sql
SELECT TO_TIMESTAMP_TZ('2026-09-19 13:45:30 Asia/Bangkok',
                       'YYYY-MM-DD HH24:MI:SS TZR') FROM dual
```

```
panicked at src/ora_type/timestamp.rs:238:17: not yet implemented
```

and then, via U-4, the process aborts.

#### Mitigation (revised 2026-09-19): refuse the column on the describe

The first version of this section said "none is possible". That was wrong about
the *timing*, and the correction matters: the panic can be avoided even though
it cannot be contained.

**What was checked, and ruled out.** Reading `oracledb` 26.0.0-beta.3's source:

| Escape considered | Available? |
|---|---|
| A per-column fetch-type override / define-as-string | **No.** `Metadata::requires_define()` is hard-coded to the LOB family (`BLOB`, `CLOB`, `JSON`, `VECTOR`) and `define_metadata()` only rewrites those to `LONG`/`LONG RAW`. `Metadata`'s fields are private and there is no public setter, so a caller cannot ask for a column to arrive as anything else |
| An output type handler | **No.** There is no such hook. The whole public statement surface is `exclude_from_cache`, `fetch_array_size`, `fetch_lobs`, `prefetch_rows` |
| A parse-only describe | **No.** `ExecuteMessage`'s `parse_only` flag (which would set `TTC_EXEC_OPTION_DESCRIBE`) is always `false` and has no public setter |
| A session setting that makes the server send offsets | **No.** The region-or-offset choice is a property of the **stored value** — the high bit of `buf[11]` — decided when the row was written. `ALTER SESSION SET TIME_ZONE` governs `TIMESTAMP WITH LOCAL TIME ZONE`, not what a stored `TIMESTAMP WITH TIME ZONE` sends |
| Telling region from offset before decoding | **No.** The flag is in the value's own wire bytes, which `oracledb` reads inside the round trip; the column's describe metadata says only `TIMESTAMP WITH TIME ZONE`. The two forms can occur in the same column, in adjacent rows |
| **Detecting the column from the describe, before fetching** | **Yes** — this is what is implemented |

**What is implemented.** The driver asks `oracledb` for **zero prefetched rows**
(`Statement::prefetch_rows(0)`) on every query, **and takes every statement out
of the statement cache** (`Statement::exclude_from_cache()`). Both are needed,
and the second was missing in the first version of this fix:

- without `prefetch_rows(0)`, the execute round trip carries rows (default 2)
  and `oracledb` decodes them before the wrapper sees anything, so the abort
  happens before any check could run;
- without `exclude_from_cache()`, `prefetch_rows(0)` stops being enough on the
  **second** execution of the same SQL. A cached statement keeps its server-side
  `cursor_id`, so `write_reexecute` takes the short TTC path, which does not
  carry the prefetch setting — the server sends rows with the re-execute and
  `oracledb` decodes them. Reproduced against the live database: the first
  execute described one column and decoded nothing; the second panicked in
  `ora_type/timestamp.rs:238` (`not yet implemented`), then again in
  `statement/holder.rs:140` on the poisoned mutex, and the process ended with
  `STATUS_STACK_BUFFER_OVERRUN (0xC0000409)`. This is recorded as the
  `#[ignore]`d `a_cached_cursor_makes_the_execute_fetch_rows_and_aborts`, and
  the containment is
  `re_executing_a_refused_query_with_a_longer_bind_is_still_refused` (four
  alternating executions of the same SQL, each still refused).

With both in place the execute is a describe: the column metadata arrives, no
value has been decoded, and `OracleCursor::new` refuses a
`TIMESTAMP WITH TIME ZONE` column with
`ErrorKind::Unsupported`, naming the column and the documented `TO_CHAR(c,
'… TZR')` escape. The session is untouched — `SessionState::Usable` — because
nothing failed; a column was declined. A nested cursor from an OUT bind never
prefetches at all, so it is covered by the same check. The OUT-bind path has no
describe to inspect, so a bind *declared* `TIMESTAMP WITH TIME ZONE` is refused
before the statement runs.

**Why the refusal is per column, not per value.** It has to be. The decode
happens inside `oracledb`'s response deserialization for the whole row, before
the wrapper is given anything, so there is no point at which one cell could be
reported as `Unsupported` while its neighbours arrive — the way an `INTERVAL`
column is handled (ADR-0002 M1). By the time a per-value decision were possible
the process is already gone.

**The trade-off, stated plainly.** The offset-only form of the type decodes
correctly and is proven to (the S2 row above). Refusing the column therefore
gives up a capability that works, for values that are indistinguishable from
ones that do not. That is the right side to be on for a database IDE — a clean
error is recoverable, a process abort loses the user's uncommitted work in
every other open worksheet as well — but it is a real loss and it is not
pretended otherwise. A caller that knows its data holds only offsets can set
the connection extension `oracle.allow_timestamp_with_time_zone`
(`EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE`) and get the old behaviour, including the
abort. Reldex itself must not set it by default.

**What it costs.** One extra round trip on a *small* result: a one-row query
takes a **1.4 ms** median against a **641 µs** median `ping` — that is, exactly
one bare round trip more than before (`describing_before_fetching_costs_about_one_extra_round_trip`;
20 iterations, median of the sorted samples; re-measured after
`exclude_from_cache` was added, previously 1.3 ms against 634 µs — the
difference is inside the run-to-run spread). A large result pays **nothing**:
`fetch_array_size` still sizes every fetch, so the same number of batches
crosses the wire either way.

It also moves *where a query's rows are produced*, and it is worth being
precise about what does **not** change. The execute message still carries
`TTC_EXEC_OPTION_EXECUTE`, so the server still **executes the statement** on the
`execute` call: the cursor is opened, the plan runs, and a statement that fails
at run time still fails there. The statement is not executed twice. What is
deferred is only the *fetch* — with `prefetch_rows(0)` the execute response
carries no rows, so the work of materialising and sending them lands on the
first `fetch_batch`, and so does any deadline armed through
`Statement::with_deadline`. For the UI that is an improvement (the grid's
columns are known immediately). For spike S4 it means a long-running query has
to be driven to its first batch to be observed blocking at all, which is what
`run_to_first_batch` in `s4_cancel.rs` now does; §4's findings and measurements
are unchanged.

*Evidence:* `a_timestamp_with_time_zone_column_is_refused_before_anything_is_fetched`
and `a_timestamp_with_time_zone_output_bind_is_refused_before_the_statement_runs`
in `s2_fidelity.rs`, both passing. The test that demonstrates the abort,
`a_named_time_zone_region_is_read_or_reported`, is still in the suite and still
`#[ignore]`d — it now turns the guard off deliberately, so it records both the
upstream defect and what the default prevents. Run it alone with `--ignored`.

*Still an upstream bug.* This is containment, not a fix: the type remains
unreadable on `26.0.0-beta.3`, and issue C in §6 should still be submitted.

### U-4 — **Any panic inside a round trip becomes a process abort** (critical)

`src/statement/holder.rs:139-140`:

```rust
impl Drop for StatementHolder {
    fn drop(&mut self) {
        let mut client = self.client_ref.lock().unwrap();
```

A panic raised while the client mutex is held poisons it. Unwinding then drops
the `StatementHolder`, whose `Drop` calls `.lock().unwrap()` on the poisoned
mutex and panics *during* unwinding — which aborts. `catch_unwind` cannot save
the caller. This is what turns U-2 and U-3 from statement failures into
application crashes, and it means **no wrapper can ever contain an upstream
panic**.

*Suggested fix:* `let mut client = match self.client_ref.lock() { Ok(g) => g, Err(p) => p.into_inner() };`
— or skip the cleanup when the lock is poisoned.

### U-5 — **`TIMESTAMP WITH TIME ZONE` is returned without applying its offset**

`OracleTimestamp::from_buf` reads the wire's UTC date and time fields and the
zone offset, and `Display` prints them together — so a value stored as
`2026-09-19 13:45:30 +07:00` renders as `2026-09-19 06:45:30+07:00`, which is a
different instant. The write path is symmetric, so an `oracledb`-only
round trip is self-consistent and the error only shows against the server's own
rendering.

*Evidence:* `temporal_values_round_trip_including_dates_the_gregorian_calendar_rejects`.
*Mitigation:* the driver applies the offset on read and its inverse on write
(`value::to_timestamp`, `binds::to_oracle_timestamp`), so Reldex reports
`2026-09-19T13:45:30.123456789+07:00`, which is what the server holds.

### U-6 — **A fired call timeout can cost the session**

`Client::recover_from_error` (`src/client/mod.rs:214`) sends an interrupt marker
and calls `reset`, which reads from the socket **with the expired timeout still
armed**. If the server does not answer within another full timeout the reset
fails and `unrecoverable_error` closes the transport. See §4 candidate 1: a
2-second deadline on `DBMS_SESSION.SLEEP(20)` returns after 4.0 s with the
connection gone.

*Suggested fix:* clear (or temporarily raise) the socket read timeout for the
duration of the recovery exchange.
*Mitigation:* none available in the wrapper; the resulting error is mapped
honestly as `NetworkLost` / `SessionState::Lost`.

*Added 2026-09-19: which way it goes is **load-dependent**, by construction.*
The outcome turns on whether the server answers the interrupt marker inside the
remaining timeout window, so the same statement can end as `Timeout` with the
session intact or as `NetworkLost` with the session gone depending on how busy
the server is. Under the parallel S4 run against this single-instance
container — three cartesian joins, a `KILL SESSION` and a 20-second sleep at
once — both S4 deadline tests have been observed flipping. Serial runs
(`--test-threads=1`) are stable and are what §4's measurements were taken from.
Reldex cannot rely on a fired deadline leaving the session usable.

*Measured 2026-09-19, because the flip first looked like a regression.* The SQL
deadline test began failing with `NetworkLost` where it had asserted `Timeout`,
and the obvious suspect was this task's `exclude_from_cache()` change. It is
not. Control runs of the same test, parallel, against the same container:

| Statement cache | Runs | Passes |
|---|---|---|
| 20 (the pre-change default, cache on) | 4 | 3 |
| 0 (cache off — the change) | 6 | 3 |

Both configurations flip, so the cause is the load the parallel S4 run puts on
a single-instance container, exactly as this defect predicts — it is
**pre-existing**, not caused by any change recorded here. The test was
therefore renamed to
`a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly` and now
accepts **either** documented outcome while asserting that the driver reports
it honestly: `Timeout` must come with `SessionState::NeedsValidation` and a
session that still answers, `NetworkLost` with `SessionState::Lost`. It has
since passed 8 consecutive runs, and the full serial S4 suite passes (7/7).
Asserting one branch of a genuinely load-dependent defect would be asserting
the weather.

### U-7 — **A server-side cancel is not observed by the client**

After `ALTER SYSTEM CANCEL SQL`, the server ends the call in ~0.5 s but the
client stays blocked in its read and never reports ORA-01013. See §4 candidate
2 for the measurement.

### U-8 — **The server's error code and error position are discarded**

`src/response/error_info.rs` parses the error number only to build a message
string, and does `resp.read_ub2()?; // error position` — reading the offset off
the wire and throwing it away. The public `ErrorKind::DbError(String)` carries
neither.

*Consequence:* the ORA code has to be recovered by parsing `ORA-nnnnn` back out
of the message text, and a character offset for a plain SQL error is
unavailable at any price. Only the `line n, column m` that ORA-06550 puts in
its own message text can be recovered, which happens to be the case
`SPEC.md` §24.14 needs — but `SPEC.md`'s "highlight the offending token in the
worksheet" is not achievable for ordinary SQL errors with this version.

*Note for ADR-0002:* its "notes for driver implementers" describe a structured
`DbError { code, offset }`. That is upstream `main`, not `beta.3`; the ADR
should say which version it is describing.

### U-9 — **`oracledb::Error` does not implement `std::error::Error`**

So it cannot be attached with `DbError::with_source`. The upstream text is
preserved in `NativeError` instead.

### U-10 — **No public break/interrupt API**

See §4 candidate 3 and the drafted issue in §6.

### U-11 — minor

- `OracleNumber`'s digits and exponent are private, so the only lossless route
  in and out is its `Display`/`FromStr`. That is one string allocation per
  numeric cell on both paths. A `from_digits(negative, digits, exponent)`
  constructor and matching accessors would remove it.
- `Config` does not implement `Debug`, so a `Result<Config, _>` cannot be
  `expect_err`ed in tests.
- `DB_TYPE_*` are `const`, not `static`, so `&DB_TYPE_NUMBER` may be a distinct
  promoted temporary on each use; type dispatch must compare by value, never by
  pointer.
- Typo in the public documentation: "Repreents a database type" (`db_type.rs:34`).

---

## 6. Drafted upstream issues — **for the owner to submit; nothing has been posted**

### Issue A — the one ADR-0001 asks for

**Title:** `Add a public break/interrupt API so a running call can be cancelled from another thread`

**Body:**

> **What I am trying to do**
>
> I am building a desktop database client on top of `oracledb`. A user must be
> able to press "stop" on a statement they started by mistake, and the session
> has to remain usable afterwards — the same thing `OCIBreak`/`OCIReset` does
> for OCI clients, and what `python-oracledb`'s `Connection.cancel()` exposes
> for the thin driver.
>
> **What is possible today**
>
> As far as I can tell from 26.0.0-beta.3, the only lever is
> `Connection::set_call_timeout`, and it has to be armed **before** the call.
> It cannot be used as a cancel, because it takes the same mutex the round trip
> holds:
>
> ```rust
> // src/connection/conn_impl.rs
> pub fn set_call_timeout(&self, duration: Option<Duration>) -> Result<(), Error> {
>     self.client_ref.lock().unwrap().set_call_timeout(duration)
> }
> ```
>
> Measured against Oracle 19.3: with one thread inside
> `execute("BEGIN DBMS_SESSION.SLEEP(5); END;")`, a second thread calling
> `set_call_timeout` 500 ms later blocked for **4.8 s** — the whole remainder of
> the statement.
>
> **What I think is needed**
>
> The crate already knows how to send a break: `MARKER_TYPE_INTERRUPT` is
> defined and `Client::recover_from_error` uses it, and `_MARKER_TYPE_BREAK` is
> defined but unused. What is missing is a way to reach the socket while a round
> trip is in progress. Something like:
>
> ```rust
> let handle = conn.break_handle();      // Send + Sync, cheap to clone
> // ... from any other thread, while a call is running:
> handle.interrupt()?;                   // writes the marker, takes no lock
> ```
>
> `Connection::cancel()` with the same semantics as `python-oracledb` would
> suit me equally well. The important properties are that it (a) does not take
> the client mutex, (b) leaves the session usable, and (c) surfaces ORA-01013 to
> the thread that was blocked in the call.
>
> I appreciate the TLS path makes this harder than a `TcpStream::try_clone`,
> since `rustls::StreamOwned` cannot be written from two threads without
> synchronisation. I would be glad to know whether you would rather have a
> dedicated writer or a second mutex limited to the write half, and I am happy
> to help test.
>
> **Related, because a break alone would not be enough for me**
>
> Two behaviours would leave the session unusable even once a break exists; I
> have filed them separately: the call-timeout recovery path re-reads with the
> expired timeout still armed, and a server-side `ALTER SYSTEM CANCEL SQL` is
> never observed by the client.
>
> Environment: `oracledb` 26.0.0-beta.3, Rust 1.98.1 MSVC, Windows 11, Oracle
> Database 19.3 EE.

### Issue B — the data-corruption one (submit first)

**Title:** `Binding a NUMBER with an odd number of leading zeros stores a value ten times too large`

**Body:**

> Binding `0.05` stores `0.5`. There is no error.
>
> ```rust
> conn.execute("CREATE TABLE t (v NUMBER)", &[])?;
> let n: OracleNumber = "0.05".parse()?;
> conn.execute("INSERT INTO t VALUES (:1)", &[&n])?;
> // SELECT TO_CHAR(v, 'TM9') FROM t  ->  .5
> ```
>
> Also wrong: `0.0005` → `0.005`, `0.0123` → `0.123`, `0.000123` → `0.00123`,
> `1E-4` → `0.001`, `1E-130` → `1E-129`, `-0.05` → `-0.5`.
> Correct: `0.5`, `0.005`, `0.00005`, `0.123`, `0.00123`, `1E-3`, `1E-129`,
> `-0.005`.
>
> The pattern is an **odd** number of leading zeros after the decimal point. I
> think the cause is in `OracleNumber::to_buf`:
>
> ```rust
> // src/ora_type/number.rs
> let mut decimal_point_index = self.decimal_point_index;   // i16
> if decimal_point_index % 2 == 1 {
>     prepend_zero = true;
>     ...
> }
> ```
>
> For those values `decimal_point_index` is negative and odd, and in Rust
> `-1 % 2 == -1`, so the test is false and the digit pairs are written one
> base-100 place out. `decimal_point_index.rem_euclid(2) == 1` (or
> `decimal_point_index & 1 != 0`) would match the intent.
>
> Environment: `oracledb` 26.0.0-beta.3, Rust 1.98.1, Oracle Database 19.3 EE.

### Issue C — the two aborts

**Title:** `Panics while the client mutex is held abort the process (index out of bounds on large NUMBER, todo!() on region-encoded TIMESTAMP WITH TIME ZONE)`

**Body:**

> Two inputs panic, and because of a third issue each panic takes the whole
> process down rather than failing the statement — so a caller cannot contain
> them with `catch_unwind`.
>
> **1. Large NUMBER bind — index out of bounds**
>
> ```rust
> let n: OracleNumber = "9.9999999999999999999999999999999999999E125".parse()?;  // a legal NUMBER
> conn.execute("INSERT INTO t VALUES (:1)", &[&n])?;
> // panicked at src/ora_type/number.rs:347:29:
> // index out of bounds: the len is 40 but the index is 40
> ```
>
> `FromStr` adds trailing zeros into `num_digits` (`src/ora_type/number.rs`,
> the `!decimal_point_detected` branch) without bounding it to
> `digits.len() == ORA_NUM_MAX_DIGITS`, and `to_buf` then indexes `digits` with
> that count.
>
> **2. Region-encoded TIMESTAMP WITH TIME ZONE — `todo!()`**
>
> ```rust
> conn.query("SELECT TO_TIMESTAMP_TZ('2026-09-19 13:45:30 Asia/Bangkok',
>                                    'YYYY-MM-DD HH24:MI:SS TZR') FROM dual", &[])?;
> // panicked at src/ora_type/timestamp.rs:238:17: not yet implemented
> ```
>
> **3. Why either one aborts**
>
> ```rust
> // src/statement/holder.rs
> impl Drop for StatementHolder {
>     fn drop(&mut self) {
>         let mut client = self.client_ref.lock().unwrap();
> ```
>
> The first panic poisons the client mutex; unwinding drops the
> `StatementHolder`, whose `Drop` unwraps the poisoned lock and panics again —
> a panic during unwinding, so the runtime aborts
> (`STATUS_STACK_BUFFER_OVERRUN` on Windows). Handling the poisoned case, e.g.
> `Err(poisoned) => poisoned.into_inner()`, would at least let the panic reach
> the caller.
>
> Environment: `oracledb` 26.0.0-beta.3, Rust 1.98.1 MSVC, Windows 11, Oracle
> Database 19.3 EE.

### Issue D — the recovery and cancel-observation defects

**Title:** `A fired call timeout closes the connection when the server cannot answer immediately; a server-side cancel is never observed`

**Body:**

> **1. Recovery reads with the expired timeout still armed**
>
> ```rust
> conn.set_call_timeout(Some(Duration::from_secs(2)))?;
> conn.execute("BEGIN DBMS_SESSION.SLEEP(20); END;", &[])
> // returns after ~4s with UnableToRecover; the connection is closed
> ```
>
> With a long *SQL* statement instead, the same code returns `CallTimeoutExceeded`
> after ~2.5 s and the session stays usable — the difference is whether the
> server answers the interrupt marker quickly. In `Client::recover_from_error`,
> `reset()` reads the reply from the same socket while the read timeout that
> just fired is still set, so when the server is busy the reset read times out
> too and `unrecoverable_error` closes the transport. Clearing or temporarily
> raising the timeout around the recovery exchange would fix it.
>
> **2. A server-side cancel is not observed**
>
> With a long SQL statement running and no call timeout, a DBA session issuing
> `ALTER SYSTEM CANCEL SQL 'sid, serial#, @inst, sql_id'` ends the call on the
> server — `V$SESSION` leaves `ACTIVE` within ~0.5 s — but the calling thread
> stays blocked and never receives ORA-01013. With a call timeout armed it
> eventually fails with `UnableToRecover` instead. `ALTER SYSTEM KILL SESSION`
> *is* observed (ORA-00028 after ~5 s), so the socket itself is fine.
>
> Environment: `oracledb` 26.0.0-beta.3, Rust 1.98.1 MSVC, Windows 11, Oracle
> Database 19.3 EE.

---

## 7. Contract problems found in `reldex-db-driver-api`

The first run of these spikes found two; both are now **fixed**, with the lead's
approval for the one that changed the contract. C-3 remains a note.

### C-1 — a REF CURSOR could not be consumed (blocking) — **fixed**

`ExecutionOutcome::out_values()` returned `&OutValues`, and `OutValues::named` /
`OutValues::positional` returned `Option<&Value>`. A `Value::Cursor` holds a
`Box<dyn Cursor>`, and every useful method on `Cursor` needs ownership:
`fetch_batch(&mut self)` and `close(self: Box<Self>)`. There was no
`take_out_values`, no `out_values_mut`, and no `OutValues::take`. So a REF
CURSOR OUT bind could be opened and *described* but never read — which made
`Capabilities::ref_cursor` unmeetable and `SPEC.md` §11 unimplementable. The
driver side was already implemented and working; only the accessor was missing.

*Fixed* by a small additive change to `db-driver-api`, recorded as ADR-0002
amendment **P1**:

```rust
impl OutValues {
    pub fn take_named(&mut self, name: &str) -> Option<Value>;
    pub fn take_positional(&mut self, index: usize) -> Option<Value>;
}
impl ExecutionOutcome {
    pub fn take_out_values(&mut self) -> OutValues;      // mirrors take_cursor
    pub fn out_values_mut(&mut self) -> &mut OutValues;  // mirrors RowBatch::column_mut
}
enum Value { /* … */ Taken }                             // + Value::is_taken()
```

A consumed slot becomes `Value::Taken`, never `Value::Null` — the rule
ADR-0002 M6 established for `Column::take_lob`, applied to output binds — and
`take_out_values` leaves a same-shaped container of `Taken` slots rather than
`OutValues::None`, so the outcome still reports which binds it had. Taking twice
returns `None`, so one live cursor cannot be owned twice. Zero production
dependencies, as before.

*Evidence:* `a_ref_cursor_out_bind_is_fetched_while_the_parent_connection_stays_usable`
and `a_ref_cursor_outliving_its_connection_reports_rather_than_panics` in
`s5_plsql.rs`, both passing against the live database; four new unit tests in
`db-driver-api`; `crates/db-core/tests/out_values.rs` for the path through
`DatabaseSession`.

### C-2 — ADR-0002's driver notes described a different upstream version — **fixed**

The "notes for driver implementers" described a structured upstream
`DbError { code, offset }`. In `26.0.0-beta.3` the public type is
`ErrorKind::DbError(String)` and the wire offset is discarded (U-8).

*Fixed* in ADR-0002: the section now names the pinned version, every note is
marked *confirmed* or *corrected* against it, the "re-verify on upgrade" marker
is kept, and three further notes are added that a driver implementer needs
(prefetch decodes rows during `execute`; a panic inside a round trip aborts the
process; the `OracleNumber` encoder is unsafe for two families of value). Two
notes besides the `DbError` one were wrong and are corrected: `Connection::execute`
does **not** reject a query, it silently discards its rows; and a split surrogate
pair in `Lob::read` cannot be repaired by "re-joining halves", because the whole
read fails and returns nothing.

`SPEC.md` §24.14's "highlight the offending token" still needs scoping to PL/SQL
until U-8 is fixed upstream — that file is not owned by this task; see §9.

### C-3 — not a defect, but worth recording

`Capabilities::error_position` is reported `true` by this driver although a
position is only ever available for PL/SQL compilation errors. There is no
finer-grained flag. If the UI will behave differently for SQL and PL/SQL, the
capability may need splitting; if not, the current reporting is honest enough
and is documented in the crate.

### C-4 — `ErrorKind` has no "object does not exist", so ORA-00942 is `Syntax`

Raised in review as a mis-classification; recorded here as a **contract gap
instead**, deliberately, because the alternatives are worse.

`ORA-00942: table or view does not exist` is mapped to `ErrorKind::Syntax`. The
contract documents that kind as an error where the statement could not be
"parsed or compiled (**syntax or semantic**)", and name resolution is exactly
the semantic half of compilation — Oracle raises ORA-00942 at parse time, before
execution, and the statement never runs. The remaining candidates are
`ErrorKind::Other`, which discards the one useful fact (the statement was
rejected, not the data), and inventing a kind, which `db-driver-api` is frozen
against in Phase 0. The native `ORA-00942` is preserved on the error either way,
so a UI that wants to say "table not found" has everything it needs.

*What the contract should grow later:* an `ObjectNotFound` kind (or a
`Syntax { resolution: bool }` refinement). Until then this mapping is a
documented choice, asserted by a unit test with the reasoning in a comment
beside it (`error.rs`), not an oversight.

---

## 8. Secret handling

> **Corrected 2026-09-19.** This section previously claimed "no credential
> appears in any source file, test, script or this document". That was **not
> true when it was written**, and the claim is recorded here rather than quietly
> deleted. Two tracked files carried working local passwords:
> `tools/oracle-test-db/init/01_create_test_user.sql` had a literal
> `IDENTIFIED BY "<value>"`, and `.env.example` shipped defaults that worked
> against the image. Both were throwaway values for a container bound to
> `127.0.0.1`, and neither is used anywhere else — but they were tracked, so
> **they remain in git history** and history was not rewritten. What changed:
>
> - the `.sql` hook is gone, replaced by `init/01_create_test_user.sh`, which
>   takes `RELDEX_TEST_USER` / `RELDEX_TEST_PWD` from the **container's
>   environment** (passed through by `compose.yaml` from the untracked `.env`),
>   refuses to run rather than invent a password, refuses a value containing a
>   quote, ampersand, semicolon or whitespace because it interpolates into SQL
>   text, and feeds `sqlplus` on **stdin** so the password is never on a command
>   line where `ps` could read it;
> - it is idempotent (`CREATE USER` or `ALTER USER … IDENTIFIED BY`, then
>   re-grants), so a password change takes effect by re-running the hook
>   against the running container — no `down -v`, no DBCA, no data loss:
>   `docker exec -e RELDEX_TEST_PWD=… reldex-oracle19c bash
>   /opt/oracle/scripts/setup/01_create_test_user.sh`. **Verified that way**
>   against the live container for this task, which is also how the tests below
>   were re-run;
> - `.env.example` now carries `CHANGE_ME_local_only` placeholders that are
>   deliberately **invalid for the image** (no digit), so an unedited copy fails
>   loudly at container start instead of creating a database with a published
>   password;
> - `tools/oracle-test-db/README.md` documents all of the above, including a
>   history note.
>
> `git check-ignore tools/oracle-test-db/.env` confirms the real file is
> ignored, and it is the only place a value lives.

`tools/oracle-test-db/.env` stays untracked and is read only by
`tools/oracle-test-db/run-it.ps1` / `run-it.sh`, which export
`RELDEX_TEST_ORACLE_DSN`, `_USER`, `_PASSWORD` (and the optional `_SYSTEM_USER`
/ `_SYSTEM_PASSWORD` used by S4's privileged candidate) into the environment of
the `cargo test` child and clear them again afterwards. The integration tests
read those variables and nothing else; tests that need the privileged extras
skip themselves and say so when the variables are absent. Every test object is
created under a name unique to the process and dropped at the end.

No extra grants were needed: `RELDEX_TEST` could already read `V$SESSION`,
`V$INSTANCE`, `V$MYSTAT` and `USER_ERRORS`, and execute `DBMS_SESSION`,
`DBMS_OUTPUT`, `DBMS_APPLICATION_INFO`, `DBMS_LOB` and `UTL_RAW`. The grant
list in `init/01_create_test_user.sh` is therefore the same set the old `.sql`
hook granted; only where the password comes from changed.

`a_wrong_password_is_an_authentication_failure_not_a_network_one` asserts that a
credential appears in neither the `Display` nor the `Debug` rendering of the
resulting error, and `the_password_never_reaches_a_message_or_a_debug_rendering`
asserts the same for connection parameters.

---

## 9. Go / no-go per kill criterion

| ADR-0001 kill criterion | Verdict | Basis |
|---|---|---|
| **S1** No pure-Rust path can authenticate against 19c | **GO** | Both connect forms authenticate; 119 ms median |
| **S2** Silent NUMBER precision loss | **CONDITIONAL GO** | Reads are exact to 40 digits. Writes are **not** safe upstream (U-1): the driver refuses the affected values rather than corrupting them, so nothing is silent — but binding a decimal below 0.1 with an odd leading-zero count is impossible until upstream fixes it |
| **S2** Thai / NCHAR corruption | **GO** | Byte-exact for Thai, non-BMP and mixed text, in `VARCHAR2`, `NVARCHAR2`, `CHAR`, `NCHAR` and `DBMS_OUTPUT`, with no NLS environment set |
| **S2** `TIMESTAMP WITH TIME ZONE` (U-3) | **CONDITIONAL GO** (was: shipping blocker) | The type is no longer readable at all by default — the column is refused before anything is decoded, so the process cannot be killed by it. `TO_CHAR(c, '… TZR')` reads the value as text meanwhile. This is containment, not support: the type stays unusable until upstream fixes the `todo!()` |
| **S3** Auto-commit cannot be turned off | **GO** | Off by default, proven against a second session; DDL's implicit commit is reported, not hidden |
| **S4** No way to stop a running statement and keep the session | **NO-GO as specified** | See §4. A pre-armed deadline is the only mechanism, and it costs the session whenever the server cannot answer the interrupt promptly. `SPEC.md` §24.8 needs upstream work |
| **S5** PL/SQL / OUT binds unusable | **GO** | PL/SQL, OUT, IN OUT, DBMS_OUTPUT, compile-error reporting and error positions all work, and REF CURSOR is fetched to exhaustion with the parent connection usable throughout, after contract change C-1 (ADR-0002 P1) |
| **S7** LOBs cannot be streamed in bounded memory | **GO** | 200 MB streamed for 4.7 MB of working set |
| **S9** Concurrency | **GO** | 8 sessions, 400 inserts, 283 ms |

### What the owner has to decide

1. **Cancellation.** `SPEC.md` §24.8 cannot be met now. The options are: ship
   with a pre-armed deadline and tell the user so in the UI; wait for upstream
   (issues A and D in §6); or reopen ADR-0001's rejected alternatives. This
   report recommends the first, plus submitting the issues, and explicitly
   recommends **against** a fork (§4 candidate 4).
2. ~~**U-3 has no mitigation.**~~ **Resolved as far as it can be.** Selecting a
   `TIMESTAMP WITH TIME ZONE` column no longer crashes the application: it is
   refused on the describe, before any value is decoded. What remains for the
   owner is to accept the price — the type is unreadable as a typed value until
   upstream fixes the `todo!()`, including the offset-only form that works —
   and to decide whether Reldex's UI should offer the
   `oracle.allow_timestamp_with_time_zone` escape hatch at all. The
   recommendation is **no**: the opt-in exists for tests and for a caller that
   controls its own data, not for an IDE that opens arbitrary databases.
3. ~~**C-1** needs a small, additive change to the frozen `db-driver-api`.~~
   **Done** (lead-approved); see §7 and ADR-0002 amendment P1. REF CURSOR works
   end to end, including through `DatabaseSession`.
4. **Whether to relax the NUMBER bind refusal** (U-1) if the owner would rather
   have the values with a documented risk. The recommendation is no.
5. **`SPEC.md` §24.14 should scope "highlight the offending token" to PL/SQL**
   until U-8 is fixed upstream (C-2). Not changed here: `SPEC.md` is not owned
   by this task.
