# Phase 0 spike results — Oracle thin driver

Workstream A, ADR-0001 spikes S1–S5 (plus S7 and S9). Everything below was run
against the live Phase 0 test database on **2026-09-19**. Nothing here is
projected, estimated or inferred from documentation: each row names the test
that produced it, and every measurement says how it was taken.

Where something failed, it is written down as a failure.

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
`TlsMode::Required` rather than silently opening a plaintext connection.

---

## 2. Results at a glance

| Spike | Verdict | One-line summary |
|---|---|---|
| S1 connect / auth | **Pass** | Easy Connect and full descriptor both work; failures classify correctly |
| S2 type fidelity | **Partial — two critical upstream defects** | Reads are exact, including Thai and non-BMP text; *binding* a NUMBER is unsafe and is now refused for the affected values |
| S3 session / transaction | **Pass** | Auto-commit off, savepoints, DDL commit reporting and session isolation all behave |
| S4 cancellation | **Fail for Reldex's requirement** | No mechanism stops a statement *and* keeps the session, in the general case |
| S5 PL/SQL | **Partial** | Everything works except reading a REF CURSOR, which a contract gap blocks |
| S7 LOB streaming | **Pass** | 100 MB CLOB and BLOB streamed with +4.7 MB working set |
| S9 concurrency | **Pass** | 8 concurrent sessions, 400 inserts, 283 ms |
| S6, S8 | **Not run** | Out of scope for this workstream (S6 metadata) / no TLS listener (S8) |

Test counts: **52 unit tests** and **1 documentation test** that need no
database, and **40 integration tests** behind the `oracle-it` feature — 39 run
and pass, 1 is `#[ignore]`d because it aborts the process (U-3). Full opt-in
run: `s1_connect` 7, `s2_fidelity` 7 + 1 ignored, `s3_session` 6, `s4_cancel` 7,
`s5_plsql` 9, `s7_lob_and_s9_concurrency` 3.

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
| NUMBER, 38 / 39 / 40 significant digits | Pass | `number_values_survive_...`; all three round-trip exactly and match the server's own `TO_CHAR(v,'TM')` |
| NUMBER, `1/3` | Pass | `0.3333333333333333333333333333333333333333` (40 digits), identical to `TO_CHAR` |
| NUMBER, `0`, `1`, `-1`, `.5`, `-0.5`, `123.45`, `0.005`, `1E-129` | Pass | same test |
| NUMBER, `9.99…E125` (126 digits) and its negative | Pass **on read** | inserted as a literal, read back digit-for-digit |
| NUMBER, `9.99…E125` **bound** | **Refused** | upstream aborts the process (U-2); the driver refuses with `ErrorKind::Unsupported` |
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
| TIMESTAMP WITH TIME ZONE, numeric offset | Pass **after a driver fix** | `2026-09-19T13:45:30.123456789+07:00` |
| TIMESTAMP WITH TIME ZONE, **named region** | **Fail (upstream)** | aborts the process; see U-3 |
| RAW | Pass | `00FF107F80DEADBEEF` byte-exact |
| NULLs of 10 types | Pass | every nullable column reads back `ValueRef::Null` |
| `INTERVAL DAY TO SECOND`, `INTERVAL YEAR TO MONTH`, `ROWID`, `TIMESTAMP WITH LOCAL TIME ZONE` | Pass | rendered as `ColumnData::Unsupported` text (`P3DT0H0M0.000000000S`, `P2Y6M`, `AAAACPAABAAAAWRAAA`, `2026-…Z`) with the server's own type name; the columns either side of them still arrive |

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

### S5 — PL/SQL — **Partial**

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
| **REF CURSOR OUT bind** | **Partial — blocked by a contract gap** | the cursor is opened and described correctly (columns `ID`, `LABEL`, types right), but its rows cannot be read: see C-1 |

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

### S9 — concurrency — **Pass**

Eight threads, one connection each, 50 inserts and a commit and a read-back per
thread: **282.8 ms** wall clock for 400 inserts across 8 sessions, every
thread seeing exactly its own 50 rows.

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

*Tests:* `a_deadline_stops_a_long_sql_statement_and_the_session_survives`,
`a_deadline_on_a_plsql_sleep_destroys_the_session`.

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
*Mitigation:* `binds::to_oracle_number` refuses these values with
`ErrorKind::Unsupported` and a message telling the caller to write the value as
a literal. **This is a real functional limitation** — half of all decimals below
0.1 cannot be bound — but a database tool that stores the wrong number is worse
than one that says no.

### U-2 — **A large-magnitude NUMBER aborts the process** (critical)

`src/ora_type/number.rs:280-283` folds trailing zeros into `num_digits` without
bounding it to the 40-byte `digits` array; `to_buf` then indexes that array with
the inflated count at line 347.

Minimal reproduction: bind `9.9999999999999999999999999999999999999E125`
(a legal Oracle NUMBER) — or anything of magnitude ≥ 1E40.

```
panicked at src/ora_type/number.rs:347:29:
index out of bounds: the len is 40 but the index is 40
```

That panic then meets U-4 and the **process aborts** with
`STATUS_STACK_BUFFER_OVERRUN (0xC0000409)`.

*Mitigation:* `binds::to_oracle_number` refuses binds whose upstream digit count
would exceed 40. Reading such values is unaffected and exact (verified to 126
digits).

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

*Mitigation:* **none is possible.** The driver cannot know in advance whether a
`TIMESTAMP WITH TIME ZONE` column holds an offset or a region, and cannot
contain the panic. Any Reldex user who selects such a column kills the
application. The test that demonstrates it is `#[ignore]`d for exactly that
reason (`a_named_time_zone_region_is_read_or_reported` in `s2_fidelity.rs`);
run it alone with `--ignored`.

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

`crates/db-driver-api` is frozen for this workstream, so these are described
rather than changed.

### C-1 — a REF CURSOR cannot be consumed (blocking)

`ExecutionOutcome::out_values()` returns `&OutValues`, and `OutValues::named` /
`OutValues::positional` return `Option<&Value>`. A `Value::Cursor` holds a
`Box<dyn Cursor>`, and every useful method on `Cursor` needs ownership:
`fetch_batch(&mut self)` and `close(self: Box<Self>)`. There is no
`take_out_values`, no `out_values_mut`, and no `OutValues::take`. So a REF
CURSOR OUT bind can be opened and *described* but never read — which makes
`Capabilities::ref_cursor` unmeetable and `SPEC.md` §11 unimplementable.

*Smallest fix:* add `ExecutionOutcome::take_out_values(&mut self) -> OutValues`
(mirroring the existing `take_cursor`), or `OutValues::take_named(&mut self,
name: &str) -> Option<Value>`. The driver side is already implemented and works;
only the accessor is missing.
*Evidence:* `a_ref_cursor_out_bind_is_fetched_while_the_parent_connection_stays_usable`
in `s5_plsql.rs`, which documents the gap where the fetch would be.

### C-2 — ADR-0002's driver notes describe a different upstream version

The "notes for driver implementers" describe a structured upstream
`DbError { code, offset }`. In `26.0.0-beta.3` the public type is
`ErrorKind::DbError(String)` and the wire offset is discarded (U-8). The ADR
should name the version it describes, and `SPEC.md` §24.14's "highlight the
offending token" should be scoped to PL/SQL until U-8 is fixed upstream.

### C-3 — not a defect, but worth recording

`Capabilities::error_position` is reported `true` by this driver although a
position is only ever available for PL/SQL compilation errors. There is no
finer-grained flag. If the UI will behave differently for SQL and PL/SQL, the
capability may need splitting; if not, the current reporting is honest enough
and is documented in the crate.

---

## 8. Secret handling

No credential appears in any source file, test, script or this document.
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
`DBMS_OUTPUT`, `DBMS_APPLICATION_INFO`, `DBMS_LOB` and `UTL_RAW`. So
`tools/oracle-test-db/init/01_create_test_user.sql` is unchanged.

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
| **S3** Auto-commit cannot be turned off | **GO** | Off by default, proven against a second session; DDL's implicit commit is reported, not hidden |
| **S4** No way to stop a running statement and keep the session | **NO-GO as specified** | See §4. A pre-armed deadline is the only mechanism, and it costs the session whenever the server cannot answer the interrupt promptly. `SPEC.md` §24.8 needs upstream work |
| **S5** PL/SQL / OUT binds unusable | **GO, with one gap** | PL/SQL, OUT, IN OUT, DBMS_OUTPUT, compile-error reporting and error positions all work. REF CURSOR is blocked by C-1, a change inside Reldex's own contract, not upstream |
| **S7** LOBs cannot be streamed in bounded memory | **GO** | 200 MB streamed for 4.7 MB of working set |
| **S9** Concurrency | **GO** | 8 sessions, 400 inserts, 283 ms |

### What the owner has to decide

1. **Cancellation.** `SPEC.md` §24.8 cannot be met now. The options are: ship
   with a pre-armed deadline and tell the user so in the UI; wait for upstream
   (issues A and D in §6); or reopen ADR-0001's rejected alternatives. This
   report recommends the first, plus submitting the issues, and explicitly
   recommends **against** a fork (§4 candidate 4).
2. **U-3 has no mitigation.** Until upstream fixes the `todo!()`, selecting a
   `TIMESTAMP WITH TIME ZONE` column that holds a named region crashes the
   application. That is a shipping blocker on its own, independent of S4.
3. **C-1** needs a small, additive change to the frozen `db-driver-api` before
   REF CURSOR support can be finished.
4. **Whether to relax the NUMBER bind refusal** (U-1) if the owner would rather
   have the values with a documented risk. The recommendation is no.
