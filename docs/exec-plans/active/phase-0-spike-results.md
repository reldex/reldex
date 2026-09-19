# Phase 0 spike results — Oracle thin driver

Workstream A, ADR-0001 spikes S1–S5 (plus S7, S8 and S9), and the
evidence-gap spikes S10–S14. Everything below was run against the live Phase 0
test database on **2026-09-19**. Nothing here is projected, estimated or
inferred from documentation: each row names the test that produced it, and every
measurement says how it was taken.

Where something failed, it is written down as a failure.

> **Extended 2026-09-19 — the Phase 0 items that had no evidence at all.**
> `phase-0.md`'s exit assessment listed five: network loss, reconnect, NCLOB,
> EXPLAIN PLAN/DBMS_XPLAN, and metadata access as a capability in its own
> right; `SPEC.md` §8 also lists privileged connections, and `phase-0.md`'s
> Measurements section asked for fetch throughput and memory during a large
> fetch. Five new spikes now cover all of them — **S10** network loss and
> reconnect, **S11** NCLOB, **S12** developer features, **S13** privileged
> connections, **S14** large-result throughput and memory. Four new upstream
> defects came out of them (**U-15** to **U-18**), two drafted as issues **F**
> and **G**, and one new contract problem (**C-5**). Two driver fixes were made
> and are covered by tests. Issues F and G have not been posted to GitHub
> (A–E were submitted on 2026-09-20 — see §6).

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
| Container | `doctorkirk/oracle-19c:19.3` as `reldex-oracle19c`, listener `127.0.0.1:1521` (TCP) and `127.0.0.1:2484` (TCPS, added for S8), service name `RELDEX` |
| TCPS endpoint | TLS 1.2, `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`, server certificate signed by a throwaway private CA generated in the container (`tools/oracle-test-db/startup/10_enable_tcps.sh`) |
| Test user | `RELDEX_TEST` (credentials only via environment; see §7) |
| Privileged user | `SYS AS SYSDBA` over the listener, `remote_login_passwordfile = EXCLUSIVE` (S13 only; credentials only via environment) |
| Dead connection detection | `SQLNET.EXPIRE_TIME` is **not set** on this database — only the shipped sample `sqlnet.ora` mentions it. S10's row-lock measurements depend on this |
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

**Updated 2026-09-19 (S8).** TLS is now exercised: a TCPS listener was added to
the container and §3's S8 section records what happened. Two things changed in
the build picture as a result. `rustls` is now a **direct** dependency of
`reldex-driver-oracle-thin` — the same 0.23 crate with the same default
features that `oracledb` already pulled, so the dependency graph, the crate
count and the licence set above are unchanged — and the driver now installs the
process-wide crypto provider itself, guarded:

```rust
if rustls::crypto::CryptoProvider::get_default().is_none() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
```

The earlier decision to leave this to the application was reversed deliberately.
`aws-lc-rs` resolves as the default only while it is the single provider
compiled in; the moment a second one is enabled — a `ring` fallback for a
platform without a C toolchain, or a dependency that turns one on transitively —
the first handshake panics with "no process-level CryptoProvider available".
That is a run-time failure caused by a build-time change, in a code path that
only runs when a customer turns TLS on, and it is the kind of thing that is
found in the field rather than in CI. The guard keeps the host's choice intact
(`install_default` is only reached when nothing has installed one, and its
result is discarded so two threads racing is a no-op), and
`install_default_crypto_provider` is public so an application that would rather
decide explicitly still can.

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
| S8 TCPS | **Pass, with limits** | TLS 1.2 / `ECDHE-RSA-AES256-GCM-SHA384`, private CA trusted, certificate **and host name** verified, 39 ms handshake. The ADR-0001 kill criterion does not fire. No mutual TLS, no Oracle wallet, no DN matching — see S8 |
| S10 network loss / reconnect | **Pass, with two upstream gaps** | A dead socket is detected in microseconds and reported `Lost`; nothing ever reconnects by itself; an in-doubt commit is surfaced, not guessed. But a **black-holed** link never returns without a deadline (U-17), a connect cannot be bounded at all (U-15), and a dead client's row locks blocked a second session for the whole 20 s measured |
| S11 NCLOB | **Pass** | Thai and non-BMP text byte-exact through the lazy stream at six buffer sizes; `NULL` and `EMPTY_CLOB()` stay distinguishable; 1.2 M characters in 55 chunks |
| S12 developer features | **Pass, with one upstream blocker** | EXPLAIN PLAN, both `DBMS_XPLAN` entry points, `V$`, ten dictionary views, `LONG`/`LONG RAW` (exact, including 73 926 characters) and `DBMS_METADATA.GET_DDL` all work. `CREATE TRIGGER` with `:NEW` does **not** (U-18) |
| S13 privileged connection | **Pass** | `AS SYSDBA` over the listener in 119 ms through the existing `SessionRole` contract; no contract gap, no upstream gap |
| S14 large result | **Pass** | 1 000 000 rows streamed for **~1 MB** of working-set growth; 50 000–92 000 rows/s. Throughput is **not** monotonic in the batch size |
| S6 mobile cross-compile | **Pass** | `aarch64-linux-android`, `aarch64-apple-ios`, `aarch64-apple-ios-sim` all compile and link in CI (PR #3); no extra tools needed for `aws-lc-sys`; provider swap to `ring` confirmed unavailable without forking. Cross-compile evidence only — physical-device validation still open |

Test counts, as measured on **2026-09-19** after the review fixes and the
evidence-gap spikes:

| | |
|---|---|
| DB-free, `cargo test --workspace` | **271 pass**, 0 fail, 0 ignored (including doc tests) |
| DB-free, driver unit tests only | **69**, all pass |
| Opt-in, `--features oracle-it` | **96 integration tests** — **90 run and pass**, **6 are `#[ignore]`d** (all six run separately and pass too) |
| Per file | `s1_connect` 8, `s2_fidelity` 17 (14 + 3 ignored), `s3_session` 6, `s4_cancel` 7, `s5_plsql` 12, `s7_lob_and_s9_concurrency` 4, `s8_tcps` 9, `s10_network_loss` 13, `s11_nclob` 4, `s12_dev_features` 10 (7 + 3 ignored), `s13_privileged` 4, `s14_large_result` 2 |

(Every `s8_tcps` test skips itself, and says so, when the TLS listener is not
configured, so that file is green on a checkout with a plain TCP container too;
`s13_privileged` does the same when no SYSDBA credentials are configured.)

Six tests are `#[ignore]`d, for two different reasons. The first three **abort
the process on purpose**, each recording a defect that a default in this driver
now prevents, and each is the proof that the corresponding guard is load-bearing
rather than decorative; they do not report a failure, they end the process. The
last three are quarantined against a failure mode rather than a current
outcome: a `LONG` this upstream version cannot decode would abort (U-4) and take
every other result in `s12_dev_features` with it. All three passed when run.
Run any of them one at a time with `-- --ignored --exact <name>`.

| Ignored test | What it records | Why it is ignored |
|---|---|---|
| `a_named_time_zone_region_is_read_or_reported` | U-3: decoding a region-encoded `TIMESTAMP WITH TIME ZONE` hits a `todo!()` | aborts; prevented in normal use by the describe-time column refusal |
| `a_cached_cursor_makes_the_execute_fetch_rows_and_aborts` | U-3 again, on **re-execution**: a cached cursor takes the re-execute path, which ignores `prefetch_rows(0)`, so rows arrive and are decoded before any check | aborts; prevented by `Statement::exclude_from_cache()` |
| `binding_forty_digits_with_an_odd_index_aborts_upstream` | U-2: 40 digits with an odd, positive decimal-point index reads past the encoder's digit buffer | aborts; prevented by `binds::encoder_defect` |
| `a_long_column_from_the_dictionary_is_read_or_refused` | S12: `ALL_VIEWS.TEXT`, `ALL_TRIGGERS.TRIGGER_BODY`, `ALL_TAB_COLUMNS.DATA_DEFAULT` all read exact | quarantined against U-4; **passes** |
| `a_long_value_longer_than_one_packet_is_read_whole_or_visibly_short` | S12: a 73 926-character `LONG` arrives whole | quarantined against U-4; **passes** |
| `a_long_raw_column_is_read_or_refused` | S12: `LONG RAW` reads byte-exact | quarantined against U-4; **passes** |

The workspace total is quoted again above because it was measured on this
branch on 2026-09-19 (`cargo test --workspace`: 271 pass, 0 fail, 0 ignored).
It is a snapshot: other workstreams edit `db-core`, `drivers/mock` and
`db-driver-api`, so re-measure rather than cite it. The per-crate numbers are
what this document is accountable for.

> **Run S4, S10 and S14 with `--test-threads=1`.**
>
> - **S4**'s seven tests include three long cartesian joins, a `KILL SESSION`
>   and a 20-second PL/SQL sleep; in parallel against this single-instance
>   container they load the server enough that the deadline-recovery outcome
>   flips (U-6, load-dependent by construction).
> - **S10** deliberately leaves server sessions the database still believes in,
>   and two of its tests measure how long a row lock survives; in parallel they
>   would measure each other.
> - **S14** streams three million rows and samples the process's own working
>   set; anything else running in the process makes that number meaningless.
>
> ```text
> tools/oracle-test-db/run-it.ps1 s4_cancel        -- --test-threads=1
> tools/oracle-test-db/run-it.sh  s10_network_loss -- --test-threads=1
> ```
>
> (The PowerShell runner inserts cargo's `--` separator itself: Windows
> PowerShell 5.1 swallows a bare `--` before the script sees it, so the
> documented invocation used to fail there while working under `pwsh`.)

> **Corrected 2026-09-19.** This note used to say "serial runs are stable". That
> is true of the file as a whole and was **not** true of
> `a_deadline_on_a_plsql_sleep_destroys_the_session`, which asserted the
> `NetworkLost` branch of U-6 outright. Measured: three runs of that test on its
> own gave `NetworkLost` three times; two runs of the whole file serially gave
> `NetworkLost` once and `Timeout` once, because the earlier tests in the file
> leave the container busy. A flaky assertion is worse than either outcome — it
> teaches the reader to re-run — so the test (now
> `a_deadline_on_a_plsql_sleep_usually_destroys_the_session`) asserts the
> invariant that holds either way: the call returns after at least its own
> deadline, is classified `Timeout` **or** `NetworkLost`, and the session state
> it reports matches what the session actually does. It prints which branch it
> got. Three consecutive whole-file serial runs are green.

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
| A socket-level timeout during **connect** is `ErrorKind::Connection`, not `Timeout` | Upstream turns every `TimedOut` I/O error into `CallTimeoutExceeded` (U-16), so a 22-second TCP connect failure was reported as "the call timeout armed for this statement expired" — about a session that never existed, with a session state attached to it | `a_connect_that_times_out_in_the_socket_is_not_reported_as_a_call_timeout` (no database), `a_connect_to_an_unroutable_address_measures_the_operating_systems_patience` (live) |
| A missing bind value for a statement that declared **no** binds is `ErrorKind::Unsupported` with the cause and the workaround | Upstream's parser reads `:NEW` in a trigger body as a placeholder (U-18); passing its message on blamed the caller for something they did not write, and left them with nothing to do about it | `a_trigger_body_that_mentions_new_is_refused_with_the_reason` |

### S6 — mobile cross-compile — **Pass**

Run 2026-09-19, evidence PR [reldex/reldex#3](https://github.com/reldex/reldex/pull/3). ADR-0001's S6
kill criterion — "either target fails to build and no provider swap fixes it" — did **not** fire:
`aarch64-linux-android`, `aarch64-apple-ios` and `aarch64-apple-ios-sim` all **compile and link** on
ordinary GitHub-hosted runners, no local NDK or Xcode, and no extra tools (no `cmake`, no
`bindgen`/`libclang`) were needed for `aws-lc-sys`. A provider swap to `ring` is confirmed
unavailable without forking `oracledb` (feature unification pulls in `aws-lc-rs` regardless). This is
cross-compile-and-link evidence only — **not mobile support** — and does not change Phase 0 success
criterion 7 (`phase-0.md`), which still needs physical-device evidence. Full detail, per-target sizes
and next steps toward device validation:
[`phase-0-s6-mobile-cross-compile.md`](phase-0-s6-mobile-cross-compile.md).

### S8 — TCPS — **Pass, with limits**

ADR-0001's kill criterion for S8 is **"TCPS cannot be established without
Instant Client."** It does not fire. A pure-Rust session was established against
a TLS listener, with certificate and host-name verification **on**, trusting a
**private** CA, and the server confirmed the transport:

```text
SELECT sys_context('USERENV','NETWORK_PROTOCOL') FROM dual  ->  tcps
```

The server side had to be built first; it is not in the image. The whole of it
is `tools/oracle-test-db/startup/10_enable_tcps.sh`, mounted into the image's
`/opt/oracle/scripts/startup` hook so it re-applies on every container start,
writing `listener.ora` and `sqlnet.ora` through the symlinks that point into the
persisted volume. Port 2484 is published to `127.0.0.1` only, like 1521.

#### What was negotiated

Measured with `openssl s_client` **inside** the container, before the driver was
pointed at it, so the server side stands on its own evidence:

| | |
|---|---|
| Protocol | **TLS 1.2** (19.3 has no TLS 1.3) |
| Cipher suite | **`ECDHE-RSA-AES256-GCM-SHA384`** |
| Chain | verified against the test CA, `Verify return code: 0 (ok)` |
| Server key | RSA 2048, SHA-256 signature, CA RSA 3072 |
| Certificate names | `subjectAltName = DNS:localhost, DNS:reldex-oracle19c`; `extendedKeyUsage = serverAuth` |
| Client certificates | `SSL_CLIENT_AUTHENTICATION = FALSE` |

The version and cipher pins are enforced: with `SSL_VERSION = 1.2` and
`SSL_CIPHER_SUITES = (TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256)` in `listener.ora`, `s_client -tls1_1` is
refused and `AES256-SHA` — a suite in neither list — is refused. **In
`sqlnet.ora` alone they do nothing**: with the pins there and not in
`listener.ora`, TLS 1.1 completed a handshake with `ECDHE-RSA-AES256-SHA`. The
script writes both files; only `listener.ora` is load-bearing for the listener,
and the `sqlnet.ora` copy governs clients running inside the container.

That intersection is the substance of the spike. 19.3 offers TLS 1.0–1.2;
`rustls` speaks only 1.2 and 1.3, and for 1.2 only AEAD suites. The overlap is
TLS 1.2 with the two ECDHE-RSA GCM suites above, and it is not empty — but a
deployment that has hardened its listener down to TLS 1.0/1.1 or to CBC suites
(still common on 19c estates) will not talk to this driver at all, and the
failure will look like "stream operation failed" rather than like a negotiation
problem.

#### How a private CA is trusted — the finding that matters

This was the open question, because every enterprise TCPS deployment uses a
private CA, and the answer was not obvious from the documentation. Read from
`oracledb` 26.0.0-beta.3's source (`src/transport.rs`):

- `Transport::negotiate_tls` builds its `rustls::ClientConfig` from
  `CustomClientCertResolver::new()`, whose root store is
  **`webpki_roots::TLS_SERVER_ROOTS` and nothing else** — the public web PKI. No
  system trust store, no `rustls-native-certs`, no `SSL_CERT_FILE`.
- If `Config::set_wallet_location` was called, `populate()` reads exactly one
  file: **`<wallet_location>/ewallet.pem`**. Then (`transport.rs:484-495`):
  - if the file contains a private key, the certificates in it become a **client
    certificate** for mutual TLS, and none of them is trusted as a root;
  - if it contains **no** private key, every certificate in it is added to the
    root store.

So a private CA **can** be trusted, through the second branch: put the issuer's
PEM — public material — in a directory as `ewallet.pem` and point
`Config::set_wallet_location` at the directory. That is what the wrapper's
`oracle.wallet_dir` extension does, and `s8_tcps.rs` proves both directions of
it: with the test CA the session opens; with an unrelated CA, and with no wallet
at all, it fails with `invalid peer certificate: UnknownIssuer`.

**No upstream issue is needed for trust itself** — the mechanism exists and
works. What is worth reporting upstream is that it is undocumented, that the
file name is fixed and unrelated to any Oracle wallet format, and that one file
cannot do both jobs (U-12, U-13, Issue E).

#### Host-name verification

`rustls`'s default verifier is used unchanged, so the name is matched against
**`subjectAltName` only** — a common name of `localhost` is invisible to it —
and the name verified is whatever the descriptor's `HOST` says. Connecting to
the same socket by its numeric address, against a certificate whose SAN has no
IP entry, fails:

```text
invalid peer certificate: certificate not valid for name "127.0.0.1";
certificate is only valid for DnsName("localhost") or DnsName("reldex-oracle19c")
```

Oracle's own controls for this are **inert**. `SSL_SERVER_DN_MATCH` and
`SSL_SERVER_CERT_DN` are parsed out of a descriptor by
`config/connect_options.rs` and written into the `SECURITY` segment sent to the
server — and the TLS layer never reads either of them (U-14). There is
consequently no way to relax name checking and no way to match a DN instead of a
SAN. For Reldex that is the right default and is left as it is; it does mean a
customer whose server certificate carries only a CN, or who reaches the database
by an address the certificate does not name, cannot connect at all.

There is also no "insecure, no-verify" mode anywhere in the crate, so the
separation the task allowed for — using one to tell "handshake works" apart from
"trust works" — was neither possible nor needed: the `s_client` run had already
proven the handshake independently.

#### Measurements

Method: `tcps_connect_latency_is_measured_against_tcp`, ten connect/close cycles
per form, **interleaved** so a slow moment on the container lands on every
series, median of ten. Run through `run-it.sh` on the machine in §1.

| | Median |
|---|---|
| TCPS connect, `tcps://localhost:2484/RELDEX` | **2.2 s** |
| TCP connect, **same host name**, `localhost:1521/RELDEX` | **2.1 s** |
| TCP connect, numeric host, `127.0.0.1:1521/RELDEX` | **116.0 ms** |
| **TLS handshake overhead** (TCPS − TCP, same host name) | **38.9 ms** |
| Host-name resolution (same host name − numeric) | **2.0 s** |

The 2 s is **not** TLS. Resolving `localhost` costs that on this Windows host
whether the connection is encrypted or not — the same figure appears on a plain
`localhost:1521/RELDEX` — and it is roughly twenty times a whole plaintext
connect. The comparison is therefore made against the same host name; the
honest TLS cost is **≈ 39 ms**, one extra handshake round trip on top of a
116 ms connect. (The certificate has no IP SAN on purpose, which is what forces
the TCPS DSN to use a name; see `tools/oracle-test-db/README.md`.)

#### What the driver does now

- `Capabilities::tls()` is **true**.
- `TlsMode::Required` turns an `Endpoint::HostPort` into `tcps://host:port/service`,
  and **refuses** an `Endpoint::ConnectString` that does not itself say
  `(PROTOCOL=TCPS)`. Upstream negotiates TLS from the address, so a descriptor
  saying TCP under a profile that requires TLS would have opened a plaintext
  session silently; it is now `ErrorKind::Configuration`.
- `oracle.wallet_dir` (text) and `oracle.wallet_password` (secret) are the two
  new extension keys. The password must be an `ExtensionValue::Secret`; `Text`
  is refused, so it cannot reach a log through an ordinary `{:?}`.
- A TLS failure now carries the `rustls` reason. Upstream's `Display` appends
  its cause, and this driver's error mapping used to replace the whole string
  with "the network stream failed" — so every TLS failure read identically and
  an untrusted CA could not be told from a name mismatch or a dead socket. The
  upstream text is passed through instead; it contains no credential, and
  `s8_tcps.rs` asserts that the password appears in neither the `Display` nor
  the `Debug` of the failure.

#### Limits — what S8 does **not** prove

| | |
|---|---|
| **Mutual TLS (client certificates)** | Untested, and not combinable with a private CA: one `ewallet.pem` is read as a client certificate **or** as a set of roots, never both (U-13). The test listener runs `SSL_CLIENT_AUTHENTICATION = FALSE`. |
| **Oracle wallets** | Not read at all. `cwallet.sso`, `ewallet.p12`, `MY_WALLET_DIRECTORY` in a descriptor: none of them reach the TLS layer. A customer with an existing wallet must export a PEM (U-12). |
| **`SSL_SERVER_DN_MATCH` / `SSL_SERVER_CERT_DN`** | Parsed and sent to the server, never used by the client (U-14). |
| **TLS 1.3, and anything but the two GCM suites** | 19.3 has no TLS 1.3. A listener hardened to TLS 1.0/1.1 or to CBC suites cannot talk to this driver. |
| **Certificate revocation** | No CRL or OCSP anywhere in the crate. An expired certificate is caught; a revoked one is not. |
| **Native Network Encryption** | Still absent upstream (ADR-0001, §5 table), and unrelated to TCPS. |
| **Anything on a real network** | Loopback only. No latency, no MTU, no proxy (`HTTPS_PROXY` is parsed by upstream but untested here), no mobile. |

The server-side work produced two findings worth keeping, both recorded in
`tools/oracle-test-db/README.md`'s troubleshooting table because both cost real
time: Oracle 19.3 will **not** serve a PKCS#12 produced by `openssl pkcs12
-export`, however valid it is (`TNS-00540: SSL protocol adapter failure`, with
no further detail) — the wallet must be built by `orapki`, with the key pair
generated inside it and only the signed certificate imported; and OpenSSL 1.0.2
ignores `-subj` when the config names a `distinguished_name` section under
`prompt = no`, which quietly produced a CA and a server certificate with
identical subject DNs.

### S10 — network loss and reconnect — **Pass, with two upstream gaps**

Run 2026-09-19. `SPEC.md` §8 lists "network loss" and "reconnect"; §18 forbids
ever silently replacing a lost transactional session. Phase 0 had no evidence
for any of it.

#### Method — the link is simulated inside the test process

`tests/common/proxy.rs` is a standard-library TCP forwarding proxy: the session
connects to `127.0.0.1:<ephemeral>`, which forwards to the real listener, and
the test switches it between **forward**, **hard drop** (`FIN` both ways, at
once) and **black hole** (both sockets open, nothing moves), plus two
packet-triggered variants for killing a link at a chosen point inside a round
trip. Nothing touches Docker, the container or the host's network, so the
Phase 0 database survives the suite exactly as it was. The first test asserts
that the session really went through the proxy, because a listener that
redirected the client would make every other measurement meaningless — this one
does not redirect (direct handoff on Linux).

Two honesty notes about the instrument. A mode switch other than a hard drop is
noticed within 20 ms, so that is the error bar on "time to detect" for the
black-hole cases; a hard drop has none, because the switch shuts the sockets
down itself. And a call that may never return is run on its own thread behind a
30 s watchdog: the test reports "did not return" as a finding and abandons the
thread, rather than hanging the suite.

#### An idle session whose link dies

| Failure | Deadline | Time to detect | Reported as |
|---|---|---|---|
| hard drop, then `ping` | none | **332 µs – 568 µs** | `NetworkLost` / `SessionState::Lost` (`os error 10053`) |
| hard drop, then `execute` | none | **54 µs – 61 µs** | `NetworkLost` / `Lost` |
| black hole, then `ping` | none | **did not return within 30 s** | — (watchdog; thread abandoned) |
| black hole, then `execute` | 3 s | **6.0 s** | `NetworkLost` / `Lost` |

The first two are the good case and they are as fast as they can be: the socket
is already dead, so the failure is local. The third is **U-17**: nothing in
`oracledb` bounds a wait — the read timeout is `None`, no TCP keepalive is
enabled, and `EXPIRE_TIME` is parsed and then only written back into descriptor
text. The fourth is **U-6 on this path**: the deadline fired at 3 s, upstream's
recovery waited out the same read timeout a second time, and the session was
destroyed. Twice the deadline, and the connection gone. `Lost` is the honest
report of that, and it is what the driver gives.

#### Loss in the middle of something

| Where the link died | Result |
|---|---|
| mid-statement (20 s `DBMS_SESSION.SLEEP`) | detected **465 – 809 µs** after the cut; `NetworkLost` / `Lost` |
| mid-fetch (50-row batches over 50 000 rows) | the next `fetch_batch` fails `NetworkLost` / `Lost`; a second call on the same cursor is refused with "cursor used after a failed fetch; the only legal call was close"; `close()` succeeds |
| mid-LOB-stream (8 MB CLOB, 8 KiB caller buffer) | **13 653 further bytes** came out of the driver's staging buffer, then `NetworkLost` / `Lost`; a second read is refused with "LOB stream read after a failure; the only legal action was to drop it" |

The LOB number is worth keeping: a stream can deliver a little more data after
the link is gone, because the driver had already staged a server-side chunk. It
never reports a clean end of data, which is the failure that would matter — a
truncated value that looked complete.

#### An open transaction, and what it costs other people

With a transaction open and the link hard-dropped, the next statement fails
`NetworkLost` / `Lost`, and a **new** session sees only the committed row
**119 ms** later: the server noticed the closed socket and rolled the dead
session back.

Row locks are the part that affects other users, and the two failure shapes
differ completely:

| Failure | A second session's `SELECT … FOR UPDATE NOWAIT` |
|---|---|
| hard drop (server's socket closed too) | succeeds after **253.5 ms** |
| black hole (server's socket still open) | **still blocked after 20 s** (ORA-00054 throughout) |

`SQLNET.EXPIRE_TIME` is **not set** on the Phase 0 test database — only the
shipped sample `sqlnet.ora` mentions it — so nothing on the server side probes a
client that has gone quiet either. A Reldex user whose Wi-Fi drops mid-
transaction therefore blocks their colleagues indefinitely, and neither end of
this driver stack currently prevents that. See U-17 and §9.

#### A commit whose reply never arrives

The proxy forwards the commit request and destroys the link on the server's
reply. The server **committed** (the row is there, seen from another session);
the client is told `NetworkLost` / `Lost` and claims nothing about the commit.
That is the only honest answer available to it, and it is the one `SPEC.md` §18
requires: the in-doubt outcome is surfaced, not guessed.

> An earlier version of this test reported a **successful commit**. That was a
> race in the test proxy, not in the driver — the forwarding thread decided
> from a mode it had read before blocking in `read`, so the reply slipped
> through. It is recorded here because it is exactly the kind of bug that
> produces a false "everything is fine" result, and it was caught by the test
> failing its own assertion rather than by review.

#### Reconnect

After the loss, three `execute`s and a `ping` all fail with a non-usable
session, and **the proxy sees no second TCP connection**: nothing under the
contract re-opens a socket. A reconnect is the caller's own act, and produces a
different session — `v$session` SID/serial# went `4.2755` → `455.19711` — with
none of the old one's state: an `ALTER SESSION SET NLS_DATE_FORMAT` made on the
dead session was gone (`TO_CHAR` reverted to the instance default `19-SEP-26`).

#### Connect-time behaviour

| Endpoint | Asked for | Measured |
|---|---|---|
| `192.0.2.1:1521` (RFC 5737, discarded) | nothing | **22.0 s**, the operating system's own SYN budget |
| proxy that accepts and forwards nothing | `with_connect_timeout(2 s)` | **still outstanding after 30 s** |

Both are **U-15**: `tcp_connect_timeout` is a dead field upstream and no
descriptor timeout key is parsed, so nothing can bound a connect. The first also
produced **U-16**: upstream turns every `TimedOut` I/O error into
`CallTimeoutExceeded`, so the driver reported "the call timeout armed for this
statement expired" about a connection that never existed. That one is now
corrected in `error::map_connect` — a socket timeout during connect is an
`ErrorKind::Connection` that says the wait was the operating system's. The
wrapper ignoring `ConnectionParams::connect_timeout()` entirely is recorded as
**C-5** in §7; it is an owner decision, not something to change silently.

### S11 — NCLOB — **Pass**

Run 2026-09-19. Phase 0 had CLOB/BLOB streaming (S7) and NVARCHAR2 character
fidelity (S2) and inferred NCLOB from the two. It no longer has to.

| Check | Verdict | Evidence |
|---|---|---|
| Thai + non-BMP + mixed text through the lazy stream | **Pass** | 82 UTF-8 bytes round-tripped byte-exact against the server's own `DUMP`/`GETLENGTH`; the same text in a CLOB agrees |
| Arrives as a locator, not a materialised value | **Pass** | `ValueRef::Lob`, never a `Text` column |
| Reported kind | **Pass** | `LobKind::NationalCharacter` — the contract's third kind, not collapsed into `Character` |
| Chunk boundaries through a surrogate pair | **Pass** | a 400× repetition of 19 ASCII + `🐘` read back identical through buffers of **16, 17, 23, 64, 100 and 4096** bytes: 600 / 550 / 400 / 146 / 93 / 3 chunks. No boundary split a character; every read was valid UTF-8 |
| `NULL` vs `EMPTY_CLOB()` | **Pass** | `NULL` → `ValueRef::Null`; `EMPTY_CLOB()` → a locator whose **first** read returns 0 (zero chunks), which is a different thing and stays different |
| `size_hint` | **Pass, and `None` by design** | a national character LOB counts in UCS-2 units server-side, so the contract's byte hint stays `None` — the same rule S7 established for CLOB |
| 1 200 000-character NCLOB | **Pass** | 3 600 000 UTF-8 bytes in **55 chunks** of at most 64 KiB in **326 ms**; memory bounded by the caller's buffer |

Nothing here needed a driver change, and nothing new was found. Compared with
S7's CLOB numbers the shape is the same: the chunk count is set by the caller's
buffer and by upstream sizing its request in UCS-2 units, not by the object.

### S12 — developer features — **Pass, with one upstream blocker**

Run 2026-09-19. Workstream E. Permission failures are reported separately from
driver failures throughout; none occurred — `RELDEX_TEST` holds
`SELECT ANY DICTIONARY` and `SELECT_CATALOG_ROLE` (see
`tools/oracle-test-db/init/01_create_test_user.sh`), and that was enough for
everything below.

| Feature | Verdict | Evidence |
|---|---|---|
| `EXPLAIN PLAN … FOR` | **Pass** | classified `StatementKind::Other` — no cursor, transaction reported unpredictable, which is right: it writes rows into `PLAN_TABLE`. `PLAN_TABLE` is reachable through the public synonym with no extra grant; 3 rows written |
| `DBMS_XPLAN.DISPLAY` | **Pass** | 26 lines of real plan, `INDEX UNIQUE SCAN` chosen for the indexed predicate |
| `DBMS_XPLAN.DISPLAY_CURSOR` | **Pass** | 35 lines for the previous statement in the same session. Needs `V$SESSION`/`V$SQL`/`V$SQL_PLAN`; `SELECT ANY DICTIONARY` suffices. The test also treats DISPLAY_CURSOR's *diagnostic sentence* about a missing privilege as a permission finding, because it returns that instead of failing |
| `V$` queries | **Pass** | `v$version` → "Oracle Database 19c Enterprise Edition Release 19.0.0.0.0 - Production"; `v$session` own row; `v$parameter`; `v$instance` |
| Dictionary views | **Pass** | `ALL_OBJECTS`, `ALL_TABLES`, `ALL_TAB_COLUMNS`, `ALL_CONSTRAINTS`, `ALL_SOURCE`, `ALL_ERRORS`, `ALL_DEPENDENCIES`, `ALL_SYNONYMS`, `ALL_SEQUENCES`, `ALL_TRIGGERS` all queried against one fixture of each `SPEC.md` §16 object group |
| `LONG` columns | **Pass** | `ALL_VIEWS.TEXT`, `ALL_TRIGGERS.TRIGGER_BODY`, `ALL_TAB_COLUMNS.DATA_DEFAULT` — all three read **exact**, described as `SqlType::Text` with native type name `LONG`, Thai text in a column default included |
| A `LONG` larger than one packet | **Pass** | a view whose text is **73 926 characters** came back at exactly 73 926, tail intact. No silent truncation |
| `LONG RAW` | **Pass** | described `SqlType::Raw` / `LONG RAW`, read back byte-exact |
| `DBMS_METADATA.GET_DDL` | **Pass** | table (352 bytes) and package (126 bytes), delivered as **CLOB locators**, streamed through the same lazy path; a Thai column name survived |
| `CREATE TRIGGER` with `:NEW` | **Fail — upstream (U-18)** | see below |

**U-18 is the finding that matters.** `oracledb`'s SQL parser scans every
statement — DDL included — for `:name` and turns each hit into a bind
placeholder, so

```sql
CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.made := SYSDATE; END;
```

fails with "1 positional bind values are required but 0 were provided". An IDE
built on this crate cannot create a trigger, and `SPEC.md` §16 lists Triggers as
a first-class object group. The driver now reports it as
`ErrorKind::Unsupported` with a message naming the cause and the workaround
(wrap the DDL in `BEGIN EXECUTE IMMEDIATE q'[…]'; END;`, which the parser skips
because it is a quoted string) instead of passing on a `Configuration` error
that blames the caller for a placeholder they did not write. The session is
untouched. The workaround is proven in the same test and is what the rest of
S12's fixtures use.

#### Metadata throughput — a first data point for `SPEC.md` §19

`SELECT owner, object_name, object_type, status, created FROM all_objects ORDER
BY owner, object_name`, **63 414 rows**, `fetch_rows = 1000`, 64 batches:
**871 ms**, **≈ 72 800 rows/s**, first batch in 1.5–45 ms. Method: one session,
wall clock around `execute` plus every `fetch_batch`, batches dropped as they
arrive. One machine, one run, no comparative claim. `SPEC.md` §19 asks about
100 000+ objects; this is 63 % of that in under a second, which says the
dictionary read is not where an object browser will struggle.

#### The quarantine, and why it turned out to be unnecessary

The three `LONG` probes and the `LONG RAW` probe are `#[ignore]`d and were run
one at a time, because a decode upstream cannot do aborts the process (U-4) and
would have hidden every other result in the file. They all passed. They stay
`#[ignore]`d: the reason for the quarantine is the *failure mode*, not the
current outcome, and an upstream version that does abort on a `LONG` should cost
one deliberate run rather than the whole file.

```text
tools/oracle-test-db/run-it.ps1 s12_dev_features -- --ignored --exact <name>
```

### S13 — privileged connections — **Pass**

Run 2026-09-19. `SPEC.md` §8 lists "privileged connections where supported";
Phase 0 had only a `SYSTEM` account, which is an ordinary session that happens
to hold DBA privileges — not a privileged *connection*.

**No contract gap and no upstream gap.** `ConnectionParams::with_role`
(`SessionRole::{Normal, SysDba, SysOper}`) already expresses it, no extension
bag is needed, and `conn.rs`'s `build_config` maps the two privileged roles onto
`oracledb`'s `AUTH_MODE_SYSDBA` / `AUTH_MODE_SYSOPER`. Nothing had exercised the
path.

| Check | Result |
|---|---|
| `AS SYSDBA` over the listener | connects in **119.0 ms** (the same as an ordinary connect), authenticating against the password file — `remote_login_passwordfile = EXCLUSIVE` |
| The session's own account | `USER = SYS`, `SYS_CONTEXT('USERENV','ISDBA') = TRUE`, `CURRENT_SCHEMA = SYS` |
| Something only a privileged session sees | `v$instance` → `RELDEX/OPEN/ACTIVE` |
| Privilege does not leak | an ordinary `RELDEX_TEST` session open at the same time still reports `ISDBA = FALSE`; the two are different SIDs |
| `AS SYSOPER` | connects; the server reports `USER = PUBLIC`, `ISDBA = FALSE` — which is what SYSOPER is, and is recorded rather than asserted to be anything else |

The credentials come from `RELDEX_TEST_ORACLE_SYSDBA_USER` /
`..._SYSDBA_PASSWORD`, added to `tools/oracle-test-db/run-it.ps1` and `.sh` for
this spike; every test skips itself and says so when they are absent. Nothing is
printed, asserted on, or put in a failure message — a failed privileged connect
is reported by `ErrorKind` and native code only.

S13 lives in its own test binary on purpose: it is the only place that exercises
this authentication path, and U-4 means a panic anywhere inside a round trip
aborts the process. A separate binary confines that to this file's results.

### S14 — large-result throughput and memory — **Pass**

Run 2026-09-19. `phase-0.md`'s "Measurements" asked for fetch throughput and
memory during a large fetch; Phase 0 had LOB streaming numbers and nothing for
rows.

**1 000 000 rows**, three columns (NUMBER, VARCHAR2(40), DATE), generated
server-side by cross-joining a 1 000-row `CONNECT BY` with itself, streamed
through `fetch_batch` with **every batch dropped as it arrives**:

| `fetch_rows` | Batches | Rows/s | Time to first batch | Working-set growth |
|---|---|---|---|---|
| 100 | 10 000 | 52 300 – 59 400 | 5.0 ms | **+480 – 504 KB** |
| 1 000 | 1 000 | 50 500 – 92 100 | 14.0 ms | **+996 – 1 032 KB** |
| 10 000 | 100 | 14 800 – 17 700 | 0.53 – 1.1 s | **+1 204 – 6 772 KB** |

*Method:* one session; wall clock from before `execute` to after the last
`fetch_batch`; working set read from `tasklist` every 250 ms and once at the
end, growth = peak minus the sample taken before the execute. The instrument is
coarse and shared with S7: it is the operating system's view of the whole
process, sampled, so it includes allocator caching and cannot see an allocation
freed between samples. It is enough to separate "bounded" from "grows with the
row count" and not enough for an allocation figure. Ranges are two runs on one
machine; no comparative claim is made.

**The claim under test — `SPEC.md` §12's bounded memory — holds.** A driver
that materialised this result would need at least 56 MB of row data; the
measured growth is about **1 MB**, and the isolated comparison is flatter still:
10 000 rows and 1 000 000 rows at the same batch size cost **0 KB** and **52 KB**
of working-set growth respectively. Memory tracks the batch, not the result.

**Throughput is not monotonic in the batch size**, which is a finding rather
than a measurement: `fetch_rows = 10000` was consistently **3.5× slower** than
the best of the other two *and* cost the longest wait for the first row (up to
1.1 s, against 5 ms at 100). 100 and 1 000 are within run-to-run noise of each
other. The cause is not established here and this is not enough evidence to pick
a default — but it is enough to say that "bigger batches are faster" must not be
assumed when Reldex chooses one.

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

### U-12 — **The only way to trust a private CA is an undocumented file name**

Found in S8. `Transport::negotiate_tls` (`src/transport.rs:264-289`) trusts
`webpki_roots::TLS_SERVER_ROOTS` and nothing else, and the sole extension point
is `CustomClientCertResolver::populate` (`:428-498`), which opens exactly
`<wallet_location>/ewallet.pem` and adds its certificates to the root store when
the file holds no private key.

It works — that is the important part, and it is what Reldex uses. What is wrong
is everything around it:

- `Config::set_wallet_location`'s documentation says "the location to use for
  loading a wallet (ewallet.pem)" and nothing about trust, so the one mechanism
  that makes TCPS usable against any real enterprise deployment is discoverable
  only by reading `transport.rs`.
- The file name is fixed and cannot be an Oracle wallet. `cwallet.sso` and
  `ewallet.p12` — what `orapki` actually produces, and what every Oracle
  administrator has — are not read. Neither is the operating system trust store,
  nor `SSL_CERT_FILE`, nor `SSL_CERT_DIR`.
- A descriptor's `MY_WALLET_DIRECTORY` **is** parsed
  (`config/connect_options.rs:508`, into `ConnectOptions::wallet_location`) and
  is then only echoed back into the `SECURITY` segment sent to the server: the
  TLS layer reads `Config::wallet_location`, a different field. A connect string
  that names a wallet directory therefore looks as though it configured
  something and did not.

No workaround is needed and none was invented; the request is documentation plus
`ewallet.p12`/system-store support. Drafted as **Issue E**.

### U-13 — **One `ewallet.pem` cannot both trust a private CA and present a client certificate**

`populate` branches on whether the PEM contains a private key
(`transport.rs:484-495`): with a key, the certificates become a `CertifiedKey`
for mutual TLS and **none of them is added to the root store**; without one,
they are all added as roots. There is one wallet location, so the two are
mutually exclusive.

That combination — internal CA on the server, client certificate on the
connection — is ordinary in Oracle estates that use `SSL_CLIENT_AUTHENTICATION =
TRUE`. As written, such a deployment can have mutual TLS or a trusted private
issuer, not both, unless the internal CA happens to chain to a public root.
Untested here (the Phase 0 listener runs with client authentication off); the
branch is unambiguous in the source. Drafted as part of **Issue E**.

### U-14 — **`SSL_SERVER_DN_MATCH` and `SSL_SERVER_CERT_DN` are parsed, sent, and never used**

`config/connect_options.rs` parses both (`:502-507`), defaults
`ssl_server_dn_match` to `true` (`:544`) and writes them into the descriptor's
`SECURITY` segment (`:386-391`). Nothing in `transport.rs` reads either. Client
name verification is `rustls`'s default verifier against `subjectAltName`, with
the descriptor's `HOST` as the name, and cannot be influenced by these
parameters at all.

Two consequences: a connection that sets `SSL_SERVER_DN_MATCH=OFF` — the escape
hatch every other Oracle client offers — still verifies the name and fails; and
a server certificate that identifies itself only by DN, with no SAN, cannot be
accepted by any configuration. Reldex wants the strict behaviour, so this is
reported rather than worked around, but silently ignoring a security parameter
is worse than rejecting it: a caller that believes it turned verification off
should be told it did not. Drafted as part of **Issue E**.

### U-15 — **A connect cannot be bounded in time at all**

`client/mod.rs:616` opens the socket with a bare `TcpStream::connect(sock_addr)`
— no timeout, on either the first address or the one a redirect names
(`:635`). The configuration field that would supply one,
`ConnectOptions::tcp_connect_timeout` (`config/connect_options.rs:265`), is
**dead**: it is declared, defaulted to `None` (`:573`) and never read, and the
descriptor parser has no arm for `TRANSPORT_CONNECT_TIMEOUT`,
`TCP_CONNECT_TIMEOUT` or `CONNECT_TIMEOUT` (`:455-500` handles
`EXPIRE_TIME`, `RETRY_COUNT`, `RETRY_DELAY`, `SDU` and others, but none of the
timeouts). `RETRY_COUNT`/`RETRY_DELAY` *are* honoured, so a descriptor can ask
for the wait to be repeated but not for it to end.

Minimal reproduction, both measured by spike S10:

| Endpoint | What happened |
|---|---|
| `192.0.2.1:1521/RELDEX` (RFC 5737 TEST-NET-1, discarded not refused) | returned after **22.0 s** — Windows's own SYN retry budget, not anything the client chose |
| a proxy that completes the TCP handshake and then forwards nothing | **still outstanding after 30 s**, with `ConnectionParams::with_connect_timeout(2 s)` set |

*Evidence:* `a_connect_to_an_unroutable_address_measures_the_operating_systems_patience`,
`a_connect_into_a_black_hole_is_not_bounded_by_the_connect_timeout`.
*Can the wrapper guard it?* Not without doing the connect on a thread of its
own and abandoning it — see §7 C-5, which is an owner decision, not a silent
one. The wrapper currently **ignores** `ConnectionParams::connect_timeout()`,
which is the honest description of what it does but not an acceptable end
state. Drafted as **Issue F**.

### U-16 — **Every socket timeout is reported as "your call timeout expired"**

`error.rs:131`:

```rust
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        if e.kind() == std::io::ErrorKind::WouldBlock
            || e.kind() == std::io::ErrorKind::TimedOut
        {
            Error::new(ErrorKind::CallTimeoutExceeded, None)
        } else {
            Error::new(ErrorKind::StreamOperation, Some(Box::new(e)))
        }
    }
}
```

Two problems in four lines. The **classification** is wrong whenever no call
timeout was armed — a TCP connect that the operating system gave up on is not a
deadline the caller set — and the `None` **discards the cause**, so the
underlying `os error 10060` and its text are gone by the time any caller sees
it. Spike S10 hit both at once: connecting to `192.0.2.1` produced
`ErrorKind::Timeout` with the message "the call timeout armed for this statement
expired", about a session that never existed.

*Evidence:* `a_connect_to_an_unroutable_address_measures_the_operating_systems_patience`
(live), `a_connect_that_times_out_in_the_socket_is_not_reported_as_a_call_timeout`
(unit, no database).
*Mitigation, applied:* `error::map_connect` now reclassifies a
`CallTimeoutExceeded` raised during `connect` as `ErrorKind::Connection` with a
message that says the wait was the operating system's. The wrapper can only do
this at connect time — mid-session the two really are indistinguishable from
outside, because the kind is all that is left. Drafted as **Issue F**.

### U-17 — **Nothing detects a dead link: no keepalive, and `EXPIRE_TIME` goes nowhere**

`transport.rs:245-246` sets `set_nodelay(true)` and `set_read_timeout(None)` and
nothing else; no `SO_KEEPALIVE` is enabled anywhere in the crate.
`EXPIRE_TIME` — Oracle's own client-side dead-connection-detection knob — is
parsed (`config/connect_options.rs:465`) and then written back into the
descriptor text by `build_description_segment` (`:360`) and never acted on. So a
client whose link has gone silent has no mechanism of its own to notice, at any
layer.

Measured by spike S10 against a black-holed link (sockets open, nothing
forwarded):

| Call | Deadline armed | Result |
|---|---|---|
| `ping` | none | **had not returned after 30 s**; the watchdog gave up, the thread was abandoned |
| `execute` | 3 s | returned after **6.0 s** as `NetworkLost`/`Lost` — the deadline fired, upstream's recovery waited out the same read timeout again, and the session was destroyed (U-6 on this path) |

The practical consequence is not the client's alone. With no client-side
detection and `SQLNET.EXPIRE_TIME` unset on the server — which it is on the
Phase 0 test database; only the sample `sqlnet.ora` mentions it — a session
whose client has vanished keeps its row locks. S10 measured **20 s and still
held** when the server's socket stayed open, against **253.5 ms** when it was
closed. A Reldex user whose Wi-Fi drops mid-transaction blocks their colleagues
until something else times the session out.

*Evidence:* `an_idle_session_behind_a_black_hole_never_returns_without_a_deadline`,
`a_black_holed_call_with_a_deadline_comes_back_and_says_what_it_knows`,
`a_dead_sessions_row_locks_survive_exactly_as_long_as_the_server_believes_in_it`.
*Can the wrapper guard it?* Only by arming a deadline on every call, which costs
the session whenever it fires (U-6). Drafted as **Issue F**.

### U-18 — **`CREATE TRIGGER` is impossible: `:NEW` is parsed as a bind placeholder**

`statement/sql_parser.rs` scans the whole statement text for `:name` and calls
`statement.add_bind` for each hit (`:233`). `determine_statement_type`
(`statement/mod.rs:88-105`) sets `is_ddl` for `CREATE`/`ALTER`/`DROP`/… and the
scan carries on regardless — there is no branch that stops looking for binds in
DDL, and no option to turn the scan off. The server then asks for a value for a
placeholder the caller never wrote.

Minimal reproduction:

```sql
CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.made := SYSDATE; END;
```

→ `1 positional bind values are required but 0 were provided`
(`bind_params.rs:101`). Two `:NEW` references give `2 positional bind values are
required but 0 were provided`.

This is not an edge case: `:NEW` and `:OLD` are how a trigger body refers to the
row, so **an Oracle IDE built on this crate cannot create a trigger**, and
`SPEC.md` §16 lists Triggers as a first-class object group.

*Evidence:* `a_trigger_body_that_mentions_new_is_refused_with_the_reason`.
*Workaround, proven in the same test:* submit the DDL inside
`BEGIN EXECUTE IMMEDIATE q'[…]'; END;`. The parser skips quoted strings
(`sql_parser.rs:216-233`), so the body is invisible to it. The cost is that the
statement is then reported as PL/SQL rather than DDL and any error position
refers to the wrapper block.
*Mitigation, applied:* when the caller declared **no** binds and upstream
complains that bind values are missing, `error::explain_parsed_placeholders`
replaces the message with one naming the cause and the workaround, and reports
`ErrorKind::Unsupported` instead of `Configuration` — because blaming the caller
for a placeholder they did not write is not an honest report. The session is
untouched (`Usable`), which the test asserts. Drafted as **Issue G**.

---

## 6. Drafted upstream issues — **five submitted 2026-09-20; F and G await the owner's go-ahead**

> **Submitted 2026-09-20 by the owner's account (SupawitNu):** B → [oracle/rust-oracledb#21](https://github.com/oracle/rust-oracledb/issues/21) (NUMBER bind ×10), C → [#22](https://github.com/oracle/rust-oracledb/issues/22) (process aborts: NUMBER index OOB, region TSTZ `todo!()`, poisoned-mutex double panic), D → [#23](https://github.com/oracle/rust-oracledb/issues/23) (call-timeout recovery closes the connection; server-side cancel not observed), A → [#24](https://github.com/oracle/rust-oracledb/issues/24) (public break/interrupt API), E → [#25](https://github.com/oracle/rust-oracledb/issues/25) (TCPS trust / `SSL_SERVER_DN_MATCH`). Issues **F** and **G** below are drafted but **not yet submitted**, pending the owner's go-ahead.

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

### Issue E — the TLS trust story (U-12, U-13, U-14)

Lowest priority of the five: nothing here is broken in the sense that data is
wrong or a process dies, and the mechanism Reldex needs does exist. It is a
documentation and completeness report, and the third part is a genuine security
smell.

**Title:** `TCPS: trusting a private CA is undocumented and PEM-only; SSL_SERVER_DN_MATCH is parsed but never applied`

**Body:**

> Using 26.0.0-beta.3 against Oracle Database 19.3 EE with a TCPS listener whose
> certificate is signed by a private CA — the normal enterprise case — three
> things stood out, all read from the source and confirmed by experiment.
>
> **1. The only way to trust a private issuer is undocumented, and it is not an
> Oracle wallet.**
>
> `Transport::negotiate_tls` builds its root store from
> `webpki_roots::TLS_SERVER_ROOTS` alone. The single extension point is
> `CustomClientCertResolver::populate`, which reads
> `<wallet_location>/ewallet.pem` and — when that file contains no private key —
> adds its certificates to the root store. That works, and it is what we now do:
>
> ```rust
> let config = Config::default()
>     .set_connect_string("tcps://db.internal:2484/SVC")?
>     .set_wallet_location("/etc/reldex/ca")   // holds ewallet.pem = the CA's PEM
>     .set_credentials(user, password);
> ```
>
> But `set_wallet_location`'s documentation says only "the location to use for
> loading a wallet (ewallet.pem)", with nothing about trust, so this is
> discoverable only by reading `transport.rs`. And the file cannot be an Oracle
> wallet: `cwallet.sso` and `ewallet.p12` — what `orapki` produces and what every
> Oracle administrator already has — are not read, nor is the OS trust store, nor
> `SSL_CERT_FILE`. A descriptor's `MY_WALLET_DIRECTORY` is parsed into
> `ConnectOptions::wallet_location` and only echoed back to the server; the TLS
> layer reads `Config::wallet_location`, a different field, so a connect string
> that names a wallet directory appears to configure something and does not.
>
> Could the documentation state the trust behaviour and the exact file name, and
> could `ewallet.p12` (and ideally the platform trust store) be accepted?
>
> **2. A client certificate and a private CA cannot be used together.**
>
> `populate` branches on whether the PEM holds a private key: with one, the
> certificates become a client `CertifiedKey` and none is added to the root
> store; without one, they all are. With a single wallet location, a deployment
> using `SSL_CLIENT_AUTHENTICATION = TRUE` behind an internal CA can have mutual
> TLS or a trusted issuer, not both. A separate trust source — or simply adding
> the certificates to the root store in both branches — would resolve it.
>
> **3. `SSL_SERVER_DN_MATCH` and `SSL_SERVER_CERT_DN` are parsed and never
> applied.**
>
> Both are read from the descriptor in `config/connect_options.rs`,
> `ssl_server_dn_match` defaults to `true`, and both are written into the
> `SECURITY` segment sent to the server — but nothing in `transport.rs` reads
> either. Name verification is rustls's default verifier against
> `subjectAltName`, using the descriptor's `HOST`.
>
> We *want* the strict behaviour and are not asking for it to be relaxed. The
> problem is that a caller who sets `SSL_SERVER_DN_MATCH=OFF` is silently
> ignored: the parameter is accepted, has no effect, and the connection fails a
> name check the caller believes it disabled. Rejecting an unsupported security
> parameter would be much better than accepting it. (A server certificate
> identified only by DN, with no SAN, is likewise unusable at any setting.)
>
> Environment: `oracledb` 26.0.0-beta.3, `rustls` 0.23.45 / `aws-lc-rs`, Rust
> 1.98.1 MSVC, Windows 11, Oracle Database 19.3 EE, listener TLS 1.2 /
> `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`.

### Issue F — timeouts and dead links (U-15, U-16, U-17)

**Title:** `No way to bound a connect, no dead-connection detection, and every socket timeout is reported as a call timeout`

**Body:**

> **Summary**
>
> Three related gaps mean a client cannot control, or correctly diagnose, how
> long it waits for a database that has stopped answering.
>
> **1. A connect cannot be bounded.** `client/mod.rs:616` calls
> `TcpStream::connect(sock_addr)` with no timeout (and again at `:635` after a
> redirect). `ConnectOptions::tcp_connect_timeout`
> (`config/connect_options.rs:265`) is declared, defaulted to `None` and never
> read, and the descriptor parser has no arm for `TRANSPORT_CONNECT_TIMEOUT`,
> `TCP_CONNECT_TIMEOUT` or `CONNECT_TIMEOUT`. `RETRY_COUNT` and `RETRY_DELAY`
> *are* honoured, so a descriptor can ask for the wait to be repeated but not
> for it to end.
>
> Reproduction: connecting to `192.0.2.1:1521/svc` (RFC 5737 TEST-NET-1, which
> is discarded rather than refused) returned after 22.0 s on Windows — the
> operating system's SYN retry budget. Nothing in the crate shortens it.
>
> **2. Every socket timeout is reported as a call timeout, and the cause is
> discarded.** `error.rs:131`:
>
> ```rust
> if e.kind() == std::io::ErrorKind::WouldBlock
>     || e.kind() == std::io::ErrorKind::TimedOut
> {
>     Error::new(ErrorKind::CallTimeoutExceeded, None)
> }
> ```
>
> In the reproduction above, the caller receives `CallTimeoutExceeded` — "the
> call timeout exceeded" — for a connection that was never established and a
> call timeout that was never set, with the underlying `os error 10060` thrown
> away by the `None`. Two suggestions: only classify as `CallTimeoutExceeded`
> when a call timeout is actually armed, and keep the `io::Error` as the cause
> in both branches.
>
> **3. Nothing detects a link that has gone silent.** `transport.rs:245-246`
> sets `set_nodelay(true)` and `set_read_timeout(None)`; no `SO_KEEPALIVE` is
> set anywhere. `EXPIRE_TIME` is parsed (`config/connect_options.rs:465`) and
> written back into the descriptor text (`:360`), but nothing acts on it — in
> other Oracle drivers it enables client-side dead-connection detection.
>
> Reproduction: with a TCP proxy that keeps both sockets open and forwards
> nothing, `Connection::ping()` with no call timeout had not returned after
> 30 s. With a 3 s call timeout it returned after 6.0 s — the timeout fired,
> `recover_from_error` waited out the same read timeout again, and the
> connection was closed. So the only available mechanism costs the session
> (this is the same recovery path as the separate call-timeout report).
>
> **Why it matters for us**
>
> We are building a desktop database client. A user on flaky Wi-Fi is the
> ordinary case, not the edge case: today their UI has no bounded way to
> discover the session is gone, and the workaround (arm a call timeout on
> everything) destroys sessions.
>
> Environment: `oracledb` 26.0.0-beta.3, Rust 1.98.1 MSVC, Windows 11, Oracle
> Database 19.3 EE.

### Issue G — `CREATE TRIGGER` cannot be executed (U-18)

**Title:** `Bind placeholders are parsed inside DDL, so CREATE TRIGGER with :NEW/:OLD cannot be executed`

**Body:**

> **Reproduction**
>
> ```rust
> conn.statement("CREATE TABLE t (id NUMBER, made DATE)")?.execute(&[])?;
> conn.statement(
>     "CREATE TRIGGER t_bi BEFORE INSERT ON t FOR EACH ROW \
>      BEGIN :NEW.made := SYSDATE; END;",
> )?
> .execute(&[])?;
> ```
>
> → `Error: 1 positional bind values are required but 0 were provided`
>
> Two `:NEW` references give `2 positional bind values are required but 0 were
> provided`. The same DDL runs fine in SQL\*Plus.
>
> **Cause**
>
> `statement/sql_parser.rs` scans the statement text for `:name` and calls
> `statement.add_bind` for each hit (`:233`).
> `Statement::determine_statement_type` (`statement/mod.rs:88`) recognises
> `CREATE`/`ALTER`/`DROP`/… and sets `is_ddl`, but the bind scan continues
> regardless, and there is no option to turn it off. `:NEW` and `:OLD` are how
> a trigger body refers to the row being changed, so any trigger with a body is
> affected.
>
> **Suggested fix**
>
> Skip bind-placeholder detection when `is_ddl` is set — a DDL statement's text
> is sent to the server verbatim and cannot carry binds — or expose a way for
> the caller to say "this statement has no binds, do not scan".
>
> **Workaround, for anyone who finds this**
>
> Wrap the DDL in a PL/SQL block with a q-string, which the parser skips:
>
> ```sql
> BEGIN EXECUTE IMMEDIATE q'[CREATE TRIGGER … BEGIN :NEW.made := SYSDATE; END;]'; END;
> ```
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

### C-5 — `ConnectionParams::connect_timeout` is accepted and ignored — **open, needs the owner**

Found by spike S10. The contract has
`ConnectionParams::with_connect_timeout(Duration)` and documents it as "how long
the driver may spend establishing the connection". **This driver never reads
it.** A profile that asks for a 2-second connect timeout waits as long as the
operating system feels like — 22.0 s against an unroutable address, and still
outstanding after 30 s against a black hole (U-15).

The contract is not at fault and needs no change; the driver is, and its options
are not equal:

1. **Leave it.** Silently ignoring a caller's timeout is exactly the shape of
   failure the rest of this driver refuses to ship (`TlsMode::Required`, the
   NUMBER bind guard): the caller is given a promise nobody keeps.
2. **Refuse the parameter** (`ErrorKind::Unsupported`) when it is set, the way
   `TlsMode::Required` refuses a non-TCPS endpoint. Honest, and it turns an
   imported connection profile that merely carries a timeout into one that
   cannot connect at all.
3. **Honour it** by running `oracledb::connect` on a helper thread, returning
   `ErrorKind::Timeout` when the deadline passes, and closing the connection if
   it arrives afterwards. This is the only option that gives a UI a bounded
   connect. It costs a thread per connect and leaks one per timed-out connect
   until the attempt finishes, and it changes the threading behaviour of
   `connect()` for every caller.

Not decided here: option 3 is a production behaviour change in a crate under
review, and option 2 makes a working configuration stop working. Recommendation
is **3**, because a desktop client must be able to bound a connect and the leak
is bounded by the user's own retries — but it is the owner's call, and it should
be made alongside the upstream request (Issue F), which would remove the need
for it entirely.

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
> **they remain in git history** and history was not rewritten. On 2026-09-20,
> before this branch was first pushed, the lead **rotated** the SYS/SYSTEM and
> `RELDEX_TEST` passwords on the running container to random values stored only
> in the untracked `.env` (S1 re-run green with the new values), so the strings
> in history no longer open anything. What changed:
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
/ `_SYSTEM_PASSWORD` used by S4's privileged candidate, and `_SYSDBA_USER` /
`_SYSDBA_PASSWORD` added for S13) into the environment of
the `cargo test` child and clear them again afterwards. The integration tests
read those variables and nothing else; tests that need the privileged extras
skip themselves and say so when the variables are absent. Every test object is
created under a name unique to the process and dropped at the end.

No extra grants were needed: `RELDEX_TEST` could already read `V$SESSION`,
`V$INSTANCE`, `V$MYSTAT` and `USER_ERRORS`, and execute `DBMS_SESSION`,
`DBMS_OUTPUT`, `DBMS_APPLICATION_INFO`, `DBMS_LOB` and `UTL_RAW`. The grant
list in `init/01_create_test_user.sh` is therefore the same set the old `.sql`
hook granted; only where the password comes from changed. **S12 confirmed this
holds for the developer features too** — `PLAN_TABLE`, `DBMS_XPLAN.DISPLAY`,
`DBMS_XPLAN.DISPLAY_CURSOR`, `DBMS_METADATA.GET_DDL`, ten `ALL_*` views and
four `V$` views all worked on the existing `SELECT ANY DICTIONARY` /
`SELECT_CATALOG_ROLE` / `EXECUTE ON DBMS_XPLAN` grants, so the init script did
not have to change for this task either.

**S13 uses SYS's existing password.** `run-it.ps1` / `run-it.sh` now also set
`RELDEX_TEST_ORACLE_SYSDBA_USER=SYS` and `..._SYSDBA_PASSWORD` from the same
`ORACLE_PWD` value in the untracked `.env` that the `SYSTEM` variables already
used; no new secret was created, nothing new is tracked, and `run-it.ps1` clears
the new variable in its `finally` block alongside the others. The S13 tests read
it through `common::sysdba_params()` and never print, assert on, or quote it —
a failed privileged connect is reported by `ErrorKind` and native code only.

`a_wrong_password_is_an_authentication_failure_not_a_network_one` asserts that a
credential appears in neither the `Display` nor the `Debug` rendering of the
resulting error, and `the_password_never_reaches_a_message_or_a_debug_rendering`
asserts the same for connection parameters.

**Key material added by S8** (2026-09-19). The TCPS work generated a test CA, a
server key and a wallet password. None of it is tracked, and none of it is
printed:

- everything private lives **inside the container's persisted volume** at
  `/opt/oracle/oradata/dbconfig/RELDEX/wallet`, at mode 600 — the CA key, the
  server key, and a wallet password generated randomly at setup time into
  `.wallet-password`. The script never echoes any of them; the wallet is
  auto-login, so the listener needs no password at start-up;
- what reaches the host is **public**: the CA certificate, copied to
  `tools/oracle-test-db/wallet/ewallet.pem` (and an unrelated CA to
  `wallet-untrusted/ewallet.pem` for the negative test). `git check-ignore -v`
  confirms both are ignored, by `.gitignore:68 wallet/` and `.gitignore:64
  *.pem` respectively;
- the new `oracle.wallet_password` extension key takes an
  `ExtensionValue::Secret` and **refuses** `Text`, so a wallet password cannot
  reach a log through an ordinary `{:?}` of the parameters;
- `a_certificate_from_an_untrusted_issuer_is_refused_without_leaking_the_credential`
  asserts that the database password appears in neither the `Display` nor the
  `Debug` of a TLS failure — which matters more than it looks, because that
  failure path now passes upstream's own error text through (see S8).

Nothing in the wallet is a secret worth protecting — it belongs to a listener
bound to `127.0.0.1` on one developer machine — but it is generated randomly and
kept out of git regardless, because the alternative teaches the wrong habit.

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
| **S8** TCPS cannot be established without Instant Client | **GO** | The criterion does not fire: a pure-Rust TCPS session, TLS 1.2 / `ECDHE-RSA-AES256-GCM-SHA384`, certificate **and** host name verified against a private CA, `USERENV.NETWORK_PROTOCOL = tcps`, 39 ms of handshake on top of a 116 ms connect. Qualified by what is **not** covered: no mutual TLS, no Oracle wallet file, no DN matching, no revocation checking, and a 19c listener hardened below TLS 1.2 or to CBC suites cannot connect at all (U-12 to U-14) |

The four spikes added on 2026-09-19 have no ADR-0001 kill criterion of their
own. Their verdicts against `SPEC.md` §8's operations list:

| Item | Verdict | Basis |
|---|---|---|
| **network loss** (S10) | **GO, with two gaps** | A dead socket is noticed in microseconds and reported `NetworkLost`/`Lost`; loss mid-statement, mid-fetch and mid-LOB-stream all report and retire the handle; an open transaction dies and a fresh session sees only committed data 119 ms later; an in-doubt commit is reported as unknown, never as success. **But**: a black-holed link never returns without a deadline (U-17), the only deadline that ends it destroys the session (U-6), and a dead client's row locks blocked a second session for the whole 20 s measured because no dead-connection detection exists on either side |
| **reconnect** (S10) | **GO** | Nothing reconnects by itself — after the loss every call fails and the proxy saw no second TCP connection. A new `connect()` is a new server session (SID/serial# differ) with none of the old session's state. `SPEC.md` §18 is satisfied |
| **NCLOB** (S11) | **GO** | Thai and non-BMP text byte-exact through the lazy stream at six buffer sizes including ones that land inside a surrogate pair; `NULL` and `EMPTY_CLOB()` stay distinct; 1.2 M characters in 55 bounded chunks |
| **EXPLAIN PLAN / DBMS_XPLAN** (S12) | **GO** | Both `DBMS_XPLAN` entry points return real plans on the test user's existing grants |
| **metadata / dictionary access** (S12) | **GO, with one blocker** | Ten `ALL_*` views, four `V$` views, `LONG` and `LONG RAW` exact (including 73 926 characters), `DBMS_METADATA.GET_DDL` as a CLOB. **`CREATE TRIGGER` with `:NEW` is impossible (U-18)**, which `SPEC.md` §16 needs; a workaround exists and the driver now names it |
| **privileged connections** (S13) | **GO** | `AS SYSDBA` and `AS SYSOPER` over the listener through the existing `SessionRole` contract; no contract gap and no upstream gap |
| **large result** (S14) | **GO** | 1 000 000 rows for about 1 MB of working-set growth: `SPEC.md` §12's bounded-memory claim holds. Throughput 50 000–92 000 rows/s, and **not** monotonic in the batch size |

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
6. **How far TCPS may be advertised to customers.** S8 passed, and the driver
   now reports `tls = true` — but the honest scope is "one-way TLS 1.2 against a
   listener that offers an AEAD suite, trusting a PEM the user supplies". Three
   things a customer may reasonably expect are absent, and none of them is in
   this driver's gift to add: **mutual TLS combined with a private CA** (U-13),
   **reading an existing Oracle wallet** rather than an exported PEM (U-12), and
   **certificate revocation**. The recommendation is to describe the supported
   configuration precisely in `SPEC.md` §8 rather than to say "TCPS supported",
   and to treat U-12's `ewallet.p12` support as the upstream request that most
   affects real deployments. `SPEC.md` is not owned by this task.
7. **Whether `SSL_SERVER_DN_MATCH` being silently ignored (U-14) needs a
   Reldex-side guard.** A connection profile imported from another tool may
   carry it; upstream accepts and ignores it, so the session verifies the name
   anyway and fails where the user expected it to succeed. This driver could
   refuse a descriptor containing it, with a message saying why. Not done:
   `Endpoint::ConnectString` is deliberately opaque, and one more special case
   in it needs the owner's call.
8. **What to do about `connect_timeout` (C-5, U-15).** The contract offers it,
   the driver ignores it, and nothing else can bound a connect — 22.0 s against
   an unroutable address, unbounded against a black hole. Three options are set
   out in §7 C-5 (leave it, refuse it, or implement it on a helper thread); the
   recommendation is to implement it, because a desktop client must be able to
   bound a connect, but it changes `connect()`'s threading for every caller and
   is therefore not a change to make without the owner.
9. **Whether Reldex should arm a default deadline on every call, and what to
   tell the user about a silent link.** S10 measured the shape of the problem:
   with no deadline a black-holed link never returns (U-17); with one, the
   deadline costs the session more often than not (U-6). Neither is acceptable
   as a silent default. The realistic options are a long default deadline with
   an explicit "the connection stopped answering; the session is gone" in the
   UI, or no default and a user-visible cancel that cannot actually stop the
   call. Both need the UI to exist, so the decision can wait — but it must not
   be made by accident.
10. **Whether the Phase 0 test database should set `SQLNET.EXPIRE_TIME`, and
    whether Reldex should tell customers to.** It is unset today, which is why
    S10 measured a dead client holding a row lock for the full 20 s budget. This
    is a database-configuration recommendation Reldex may need to document
    (server-side dead connection detection is the only thing that protects other
    users from a Reldex client that vanished), not something the driver can fix.
11. **`CREATE TRIGGER` (U-18).** The driver now explains the failure and names
    the `EXECUTE IMMEDIATE q'[…]'` workaround, but it does **not** apply the
    workaround itself, because that silently changes a DDL statement into a
    PL/SQL block and moves any error position — which `SPEC.md` §24.14 cares
    about. Whether Reldex's editor should offer to rewrite the statement (with
    the rewrite visible to the user) is a product decision. Issue G is drafted.
12. **Which fetch batch size Reldex should default to.** S14 found throughput is
    **not** monotonic in the batch size — 10 000 rows per fetch was 3.5× slower
    than the best of 100 and 1 000, and cost up to 1.1 s before the first row
    appeared, which is the number a user actually feels. 100 and 1 000 are
    within run-to-run noise of each other. This is one machine and one run: it
    is enough to forbid assuming "bigger is faster", not enough to pick a
    number. A short follow-up measurement across row shapes and a real network
    should precede the choice.
