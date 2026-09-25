# Upgrading the pinned `oracledb` version

`crates/drivers/oracle-thin` wraps `oracledb`, pinned to an **exact** version
because the crate is pre-GA. That wrapper carries guards, refusals and
workarounds for nineteen upstream defects and gaps (`phase-0-spike-results.md`
§5, U-1…U-19; U-19 is a cost, not a guarded/refused defect — see its row
below). This is the procedure for moving the pin. Follow it in order.

## 1. Run the canaries before you change anything else

```text
cargo test -p reldex-driver-oracle-thin --test canary_upstream_offline
tools/oracle-test-db/run-it.ps1 canary_upstream_live -- --test-threads=1
```

(`run-it.sh` takes the same arguments on Linux/macOS.) The live target needs
the test database up: `docker start reldex-oracle19c`.

The two files are `crates/drivers/oracle-thin/tests/canary_upstream_offline.rs`
and `…/canary_upstream_live.rs`. A **canary asserts the defect is still
there**, so:

| Outcome | What it means | What to do |
|---|---|---|
| passes | the defect is still present | leave the guard alone |
| fails, message says **FIXED** | upstream fixed it | re-check by hand, then remove the guard the message names, update the canary, record it in ADR-0001 |
| fails, message says **CHANGED** | the behaviour moved but not to the fixed shape | re-derive the guard from the new upstream source before touching it |
| fails, message says **did not run** | a child canary failed before the dangerous call | fix that first; it says nothing about upstream |

The offline target also holds the **version tripwire**
(`the_pinned_oracledb_version_is_the_one_these_canaries_were_derived_from`).
It fails the moment `Cargo.lock` moves off the expected version, and its
message is the short form of this document.

The abort canaries (U-2, U-3) each run the dangerous call in a **child
process** — this same test binary, re-invoked with `--exact <child> --ignored`
and `RELDEX_CANARY_CHILD` set — and assert on how it died. The child prints
`CANARY-ARMED <name>` immediately before the call, and the parent refuses to
draw any conclusion without that line. Each takes about 1.2 s.

## 2. The map

| U | Defect | Canary | Where | Guard in this crate | Upstream |
|---|---|---|---|---|---|
| U-1 | a bound NUMBER below 0.1 with an odd leading-zero count is stored ×10 | `u1_a_bound_number_with_an_odd_leading_zero_count_is_still_stored_ten_times_too_large` | live | `binds.rs::encoder_defect` → `EncoderDefect::ScaledByTen` | [#21](https://github.com/oracle/rust-oracledb/issues/21) — **fixed on `main`**, commit `efcda45` (2026-09-19); maintainer reply 2026-09-20; not yet in our pinned beta.3 |
| U-2 | a 40-digit NUMBER at an odd, positive decimal-point index reads past the encoder's buffer | `u2_a_forty_digit_bind_with_an_odd_decimal_point_index_still_aborts_the_process` | live, child process | `binds.rs::encoder_defect` → `EncoderDefect::ReadsPastItsDigitBuffer` | not submitted; drafted as a new issue 2026-09-23 per the maintainer's invitation on [#22](https://github.com/oracle/rust-oracledb/issues/22) (2026-09-23); confirmed still present on `main` by reading the diff (results file §6) |
| U-3 | a named time-zone region hits a `todo!()` | `u3_a_named_time_zone_region_still_aborts_the_process` | live, child process | `cursor.rs::timestamp_with_time_zone_is_refused`, plus `prefetch_rows(0)` and `exclude_from_cache()` in `conn.rs`; opt-out `lib.rs::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE` | not submitted; drafted as a new issue 2026-09-23 per the maintainer's invitation on [#22](https://github.com/oracle/rust-oracledb/issues/22) (2026-09-23); confirmed still present on `main` (`todo!()` still at `timestamp.rs:236`) |
| U-4 | any panic inside a round trip becomes a process abort | *no canary of its own* — it is the **exit status** the U-2 and U-3 parents assert on. An ordinary exit code 101 from either child means U-4 is fixed, and both parents say so | live, child process | none is possible; U-2 and U-3 are refused rather than caught because of it | [#22](https://github.com/oracle/rust-oracledb/issues/22) — **the poisoned-lock double panic is fixed on `main`**, commit `6785e95` (2026-09-22: `ErrorKind::LockPoisoned`, `.lock()?` replacing `.lock().unwrap()`); maintainer reply 2026-09-23. The underlying U-2/U-3 panics are unaffected and still happen; only the second panic during unwinding is gone |
| U-5 | `TIMESTAMP WITH TIME ZONE` is returned without applying its offset | `u5_a_timestamp_with_time_zone_is_still_returned_without_its_offset_applied` | live | `value.rs::to_timestamp`, `binds.rs::to_oracle_timestamp` | not submitted; drafted as a new issue 2026-09-23 (results file §6); confirmed unchanged on `main`; not a duplicate of [#8](https://github.com/oracle/rust-oracledb/issues/8) (bind direction) or [#9](https://github.com/oracle/rust-oracledb/issues/9) (fixed on `main`, unrelated — session default time zone) |
| U-6 | a fired call timeout can cost the session [2026-09-24: only confirmed for server work that is *suspended* (`dbms_session.sleep`) — CPU-bound server work recovers cleanly instead; see the S4 addendum] | `scenario_1_control_a_sleep_10_dies_as_the_issue_describes` (`crates/drivers/oracle-thin/tests/probe_call_timeout_cpu_bound.rs`) — deterministic as of 2026-09-24: a session **suspended** on `dbms_session.sleep` reliably produces `NetworkLost`, independently confirmed unusable (`SELECT 1 FROM dual` fails too, not just the error's own claim). Load-dependent only in *how* it fails, not *whether*; `s4_cancel.rs`'s `a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly` remains the load-sensitive SQL-side companion and still accepts either documented outcome. See `client/mod.rs::recover_from_error`, which reads the reset reply with the expired socket timeout still armed | live | none | [#23](https://github.com/oracle/rust-oracledb/issues/23) — CPU-bound re-test posted 2026-09-24 (results file §6); S4 addendum, §4 |
| U-7 | a server-side cancel is not observed by the client [2026-09-24: narrowed — the client's own pre-armed-deadline interrupt *is* observed while the server is busy on CPU; the still-unobserved case is a request-on-demand cancel (`ALTER SYSTEM CANCEL SQL`), and a suspended/waiting server; see the S4 addendum] | `scenario_3_cpu_bound_plsql_loop` (same file) is the deterministic **contrast**, not a fix for U-7 itself: CPU-bound server work (no sleep, no I/O wait) reliably produces a plain `Timeout` with the session independently confirmed usable — the interrupt *is* serviced when the server is busy on CPU, just not when it is suspended. This narrows U-7 to session-level waits; it does not give the client an on-demand break for a statement that is neither suspended nor already at a check-point. `ALTER SYSTEM CANCEL SQL` (S4 candidate 2) remains the only way to stop a statement server-side on request, and it still is not observed by the client | live | none | [#23](https://github.com/oracle/rust-oracledb/issues/23) — CPU-bound re-test posted 2026-09-24 (results file §6); S4 addendum, §4 |
| U-8 | the server's error code and error position are discarded | `u8_the_servers_error_code_and_position_are_still_only_in_the_message_text` | live | `error.rs` recovers the code by parsing `ORA-nnnnn` out of the message text | not submitted — **already fixed on `main`**, commit `04b96be` (2026-09-14): `DbError` now exposes `.code()`/`.message()`/`.offset()` from the wire's `error_num`/`error_pos`. Not yet in our pinned beta.3; the guard stays until the pin moves and the canary confirms FIXED |
| U-9 | `oracledb::Error` does not implement `std::error::Error` | `u9_the_upstream_error_type_still_does_not_implement_the_standard_error_trait` | offline | `error.rs` preserves the upstream text in `NativeError` instead of `DbError::with_source` | not submitted; drafted as a new issue 2026-09-23 (results file §6); confirmed unchanged on `main` |
| U-10 | no public break/interrupt API | **manual check** — look for an inherent `Connection::break_execution`, `cancel` or `interrupt` in the upstream docs and `connection/mod.rs`. A runtime probe is possible but not sound: a method whose signature differs from the probe's breaks the **build** instead of failing a test | — | `CancelKind::PreArmedDeadline` is all `conn.rs` can offer (ADR-0001 C1) | [#24](https://github.com/oracle/rust-oracledb/issues/24) |
| U-11 | minor API gaps | `u11_the_upstream_config_type_still_does_not_implement_debug` covers the `Config: Debug` bullet. **Manual check** for the rest: an `OracleNumber::from_digits` constructor and digit/exponent accessors, `DB_TYPE_*` becoming `static` rather than `const`, and the `db_type.rs` documentation typo | offline + manual | `binds.rs::to_oracle_number` goes through `Display`/`FromStr` (one allocation per numeric cell) because there is no other lossless route | not submitted; drafted as one bundled new issue 2026-09-23 covering all four sub-items (results file §6); all four confirmed unchanged on `main` |
| U-12 | a wallet directory named in a descriptor never reaches the TLS layer | `u12_a_wallet_directory_named_in_a_descriptor_still_never_reaches_the_tls_layer` | offline | `lib.rs::EXT_WALLET_DIR` — the wallet has to be named in an extension, not in the connect string | [#25](https://github.com/oracle/rust-oracledb/issues/25) item 1 — maintainer reply 2026-09-20: `cwallet.sso`/`ewallet.p12` are a by-design won't-fix ("management has declared that `cwallet.sso` may not be used by open source drivers"); documentation update promised, not yet landed on `main` |
| U-13 | one `ewallet.pem` cannot both trust a private CA and present a client certificate | **manual check** — the Phase 0 listener runs `SSL_CLIENT_AUTHENTICATION = FALSE`, so there is nothing to test against. Read the branch in `transport.rs::CustomClientCertResolver::populate`: with a private key the certificates become a `CertifiedKey` and **none** is added to the root store | — | none; documented as a limit in `lib.rs`'s transport-security section | [#25](https://github.com/oracle/rust-oracledb/issues/25) item 2 — maintainer asked for clarification 2026-09-20 ("Can you clarify a bit more?"); clarification comment drafted 2026-09-23 (results file §6), grounded in S8 evidence; confirmed unchanged on `main` |
| U-14 | `SSL_SERVER_DN_MATCH` / `SSL_SERVER_CERT_DN` are parsed, sent, never used | `u14_ssl_server_dn_match_off_is_still_parsed_sent_and_ignored` | live (skips without the TCPS listener) | `descriptor.rs::guard` — refuses `SSL_SERVER_CERT_DN` unless `oracle.allow_unenforced_server_cert_dn` is set, warns on `SSL_SERVER_DN_MATCH`; its agreement test `the_guard_never_sees_less_than_upstream_keeps` must be re-run too, because it follows upstream's parser | [#25](https://github.com/oracle/rust-oracledb/issues/25) item 3 — maintainer reply 2026-09-20: "that should indeed be addressed. I'll take care of it." Confirmed not yet landed on `main` |
| U-15 | a connect cannot be bounded in time | `u15_a_connect_string_still_cannot_ask_for_a_bounded_connect` covers both routes into the parser. The **timings** (22 s to an unroutable address, >30 s into a black hole) are skipped: too slow and OS-dependent — S10 has them | offline | `connect_timeout.rs` — `conn.rs::connect` runs `oracledb::connect` on a helper thread and stops waiting at `ConnectionParams::connect_timeout()`, defaulting to `DEFAULT_CONNECT_TIMEOUT` (15 s) and capped at `MAX_CONNECT_TIMEOUT` (1 h); `EXT_CONNECT_TIMEOUT_UNBOUNDED` waives it. A late session is handed back through `Handoff` and **closed on that thread, never adopted**, including when the waiting frame unwinds. Creating a connection off its owning thread is ADR-0002 amendment H1/H2. Remove the guard only once upstream can both bound *and* cancel a connect: a bound alone would still leave the abandoned-attempt thread this works around (results file §7 C-5) | Issue F, drafted, **not submitted** — owner decision 2026-09-19 stands (`TASKS.md` line 175); refreshed against `main` 2026-09-23, confirmed still present (`client/mod.rs:620`/`:639`, `tcp_connect_timeout` still unread) |
| U-16 | every socket timeout is reported as "your call timeout expired" | `u16_a_socket_timeout_is_still_reported_as_an_expired_call_timeout` | offline | `error.rs::map_connect` reclassifies it to `ErrorKind::Connection` during connect | Issue F, drafted, **not submitted** — owner decision 2026-09-19 stands; refreshed against `main` 2026-09-23, confirmed still present (`error.rs:135-138`) |
| U-17 | nothing detects a dead link: no keepalive, `EXPIRE_TIME` goes nowhere | **manual check** — needs the S10 black-hole proxy and ≥ 30 s of waiting, so it is deliberately not a canary. Grep upstream for `SO_KEEPALIVE`/`set_keepalive` in `transport.rs`, and for any *read* of `Description::expire_time` beyond `build_description_segment` | — | none; a deadline on every call is the only lever and it costs the session (U-6) | Issue F, drafted, **not submitted** — owner decision 2026-09-19 stands; refreshed against `main` 2026-09-23, confirmed still present (`transport.rs:337-338`, no `SO_KEEPALIVE` anywhere in the crate, `expire_time` still write-only) |
| U-18 | `CREATE TRIGGER` is impossible: `:NEW` is parsed as a bind placeholder | `u18_a_trigger_body_that_mentions_new_is_still_parsed_as_a_bind_placeholder`. It drives **upstream directly** (`raw_connect`), not this crate, so the automatic rewrite does not touch it and it still asserts the raw defect | live | `rewrite.rs` — `conn.rs::execute` rewrites any `CREATE … TRIGGER` whose text upstream would read a placeholder into as `BEGIN EXECUTE IMMEDIATE q'X…X'; END;`, **on by default**, with a `Warning` carrying the text sent; `EXT_REWRITE_TRIGGER_DDL = false` restores `error.rs::explain_parsed_placeholders`, which is still the guard for any other statement. `rewrite.rs::upstream_finds_a_bind_placeholder` is a literal transcription of `sql_parser.rs` and **must be re-derived** on an upgrade, like `classify::upstream_would_return_rows`. When the canary says FIXED, delete the **wrapping** rather than leaving it: it costs a 32767-byte limit and moves syntax-error positions (results file §5 U-18). Keep `rewrite.rs::normalize` — the `/` and `CALL` terminator strip is about SQL\*Plus punctuation, not about upstream, and without it the server silently creates an `INVALID` trigger | Issue G, drafted, **not submitted** — owner decision 2026-09-19 stands; refreshed against `main` 2026-09-23, confirmed still present and unaffected by #1's fix (different `todo!()`s); `sql_parser.rs` has zero commits since beta.3 |
| U-19 | a multi-packet response is re-parsed from byte 0 on every packet against a pre-23ai server (`client/mod.rs::receive_response` + `response/mod.rs::add_packets` + `read_buffer.rs::from_packets`), so client CPU to fetch a batch is O(packets²), not O(bytes) — this is why S14 found throughput non-monotonic in batch size | none yet — this is a cost finding, not a value that can be asserted pass/fail offline or live without a benchmark harness; a canary would need a packet-count-controlled probe (wide rows or a large batch) and a regression threshold, not yet built | — | none possible — no guard/refusal changes the asymptotic cost, only upstream fixing the reassembly (incremental deserialization, or an end-of-response signal on pre-23ai). Reldex's own lever is the batch-size default ADR-0004 is choosing, informed by this cost | **not yet reported — draft ready**. Drafted as Issue J (results file §5 U-19, §6 "New draft — Issue J"), 2026-09-25; not submitted — owner review pending, per the same posting etiquette as Issues F and G |

### Relationship to the `#[ignore]`d spike repros

`s2_fidelity.rs` keeps `binding_forty_digits_with_an_odd_index_aborts_upstream`
and `a_named_time_zone_region_is_read_or_reported`. They are cited as evidence
in `phase-0-spike-results.md` §5 and stay there as the spikes' record; the U-2
and U-3 canaries now automate what they had to be run by hand for, so **nobody
needs to run them manually on an upgrade**. The third,
`a_cached_cursor_makes_the_execute_fetch_rows_and_aborts`, is *not* superseded:
it proves this wrapper's `exclude_from_cache()` containment rather than
upstream behaviour, and stays a manual repro.

## 3. After the canaries

1. Update `EXPECTED_ORACLEDB_VERSION` in `canary_upstream_offline.rs`.
2. Update both `oracledb = "=…"` requirements in
   `crates/drivers/oracle-thin/Cargo.toml` — the dependency and the
   dev-dependency move together.
3. Replace every `oracledb 26.0.0-beta.3` that appears in a refusal message or
   a doc comment under `crates/drivers/oracle-thin/src/`; the text reaches
   users.
4. Run the rest of the evidence: `cargo test --workspace`, then
   `run-it.ps1` for `s1_connect` … `s14_large_result` (S4 and S10
   single-threaded), including `s12b_trigger_rewrite`.
5. Record what moved in `docs/decisions/0001-database-driver-strategy.md`
   ("Follow-up work"), `phase-0-spike-results.md` §5, `TASKS.md` and
   `Task.html`. A guard removed without an ADR note is a guard nobody can
   explain later.
