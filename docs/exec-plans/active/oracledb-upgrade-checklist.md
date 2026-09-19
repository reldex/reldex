# Upgrading the pinned `oracledb` version

`crates/drivers/oracle-thin` wraps `oracledb`, pinned to an **exact** version
because the crate is pre-GA. That wrapper carries guards, refusals and
workarounds for eighteen upstream defects (`phase-0-spike-results.md` §5,
U-1…U-18). This is the procedure for moving the pin. Follow it in order.

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
| U-1 | a bound NUMBER below 0.1 with an odd leading-zero count is stored ×10 | `u1_a_bound_number_with_an_odd_leading_zero_count_is_still_stored_ten_times_too_large` | live | `binds.rs::encoder_defect` → `EncoderDefect::ScaledByTen` | [#21](https://github.com/oracle/rust-oracledb/issues/21) |
| U-2 | a 40-digit NUMBER at an odd, positive decimal-point index reads past the encoder's buffer | `u2_a_forty_digit_bind_with_an_odd_decimal_point_index_still_aborts_the_process` | live, child process | `binds.rs::encoder_defect` → `EncoderDefect::ReadsPastItsDigitBuffer` | [#22](https://github.com/oracle/rust-oracledb/issues/22) |
| U-3 | a named time-zone region hits a `todo!()` | `u3_a_named_time_zone_region_still_aborts_the_process` | live, child process | `cursor.rs::timestamp_with_time_zone_is_refused`, plus `prefetch_rows(0)` and `exclude_from_cache()` in `conn.rs`; opt-out `lib.rs::EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE` | [#22](https://github.com/oracle/rust-oracledb/issues/22) |
| U-4 | any panic inside a round trip becomes a process abort | *no canary of its own* — it is the **exit status** the U-2 and U-3 parents assert on. An ordinary exit code 101 from either child means U-4 is fixed, and both parents say so | live, child process | none is possible; U-2 and U-3 are refused rather than caught because of it | [#22](https://github.com/oracle/rust-oracledb/issues/22) |
| U-5 | `TIMESTAMP WITH TIME ZONE` is returned without applying its offset | `u5_a_timestamp_with_time_zone_is_still_returned_without_its_offset_applied` | live | `value.rs::to_timestamp`, `binds.rs::to_oracle_timestamp` | not submitted |
| U-6 | a fired call timeout can cost the session | **manual check** — load-dependent by construction, so no deterministic canary is possible. Re-run `run-it.ps1 s4_cancel -- --test-threads=1`; `a_deadline_stops_a_long_sql_statement_and_reports_the_session_honestly` accepts either documented outcome. Look for a `reset` that clears the socket read timeout in `client/mod.rs::recover_from_error` | — | none | [#23](https://github.com/oracle/rust-oracledb/issues/23) |
| U-7 | a server-side cancel is not observed by the client | **manual check** — needs a privileged session, a 20 s sleep and a "still blocked after N seconds" assertion; neither cheap nor deterministic. Re-run S4's privileged-cancel candidate | — | none | [#23](https://github.com/oracle/rust-oracledb/issues/23) |
| U-8 | the server's error code and error position are discarded | `u8_the_servers_error_code_and_position_are_still_only_in_the_message_text` | live | `error.rs` recovers the code by parsing `ORA-nnnnn` out of the message text | not submitted |
| U-9 | `oracledb::Error` does not implement `std::error::Error` | `u9_the_upstream_error_type_still_does_not_implement_the_standard_error_trait` | offline | `error.rs` preserves the upstream text in `NativeError` instead of `DbError::with_source` | not submitted |
| U-10 | no public break/interrupt API | **manual check** — look for an inherent `Connection::break_execution`, `cancel` or `interrupt` in the upstream docs and `connection/mod.rs`. A runtime probe is possible but not sound: a method whose signature differs from the probe's breaks the **build** instead of failing a test | — | `CancelKind::PreArmedDeadline` is all `conn.rs` can offer (ADR-0001 C1) | [#24](https://github.com/oracle/rust-oracledb/issues/24) |
| U-11 | minor API gaps | `u11_the_upstream_config_type_still_does_not_implement_debug` covers the `Config: Debug` bullet. **Manual check** for the rest: an `OracleNumber::from_digits` constructor and digit/exponent accessors, `DB_TYPE_*` becoming `static` rather than `const`, and the `db_type.rs` documentation typo | offline + manual | `binds.rs::to_oracle_number` goes through `Display`/`FromStr` (one allocation per numeric cell) because there is no other lossless route | not submitted |
| U-12 | a wallet directory named in a descriptor never reaches the TLS layer | `u12_a_wallet_directory_named_in_a_descriptor_still_never_reaches_the_tls_layer` | offline | `lib.rs::EXT_WALLET_DIR` — the wallet has to be named in an extension, not in the connect string | [#25](https://github.com/oracle/rust-oracledb/issues/25) |
| U-13 | one `ewallet.pem` cannot both trust a private CA and present a client certificate | **manual check** — the Phase 0 listener runs `SSL_CLIENT_AUTHENTICATION = FALSE`, so there is nothing to test against. Read the branch in `transport.rs::CustomClientCertResolver::populate`: with a private key the certificates become a `CertifiedKey` and **none** is added to the root store | — | none; documented as a limit in `lib.rs`'s transport-security section | [#25](https://github.com/oracle/rust-oracledb/issues/25) |
| U-14 | `SSL_SERVER_DN_MATCH` / `SSL_SERVER_CERT_DN` are parsed, sent, never used | `u14_ssl_server_dn_match_off_is_still_parsed_sent_and_ignored` | live (skips without the TCPS listener) | `descriptor.rs::guard` — refuses `SSL_SERVER_CERT_DN` unless `oracle.allow_unenforced_server_cert_dn` is set, warns on `SSL_SERVER_DN_MATCH`; its agreement test `the_guard_never_sees_less_than_upstream_keeps` must be re-run too, because it follows upstream's parser | [#25](https://github.com/oracle/rust-oracledb/issues/25) |
| U-15 | a connect cannot be bounded in time | `u15_a_connect_string_still_cannot_ask_for_a_bounded_connect` covers both routes into the parser. The **timings** (22 s to an unroutable address, >30 s into a black hole) are skipped: too slow and OS-dependent — S10 has them | offline | `connect_timeout.rs` — `conn.rs::connect` runs `oracledb::connect` on a helper thread and stops waiting at `ConnectionParams::connect_timeout()`, defaulting to `DEFAULT_CONNECT_TIMEOUT` (15 s); `EXT_CONNECT_TIMEOUT_UNBOUNDED` waives it. A late session is handed back through `Handoff` and **closed on that thread, never adopted**. Remove the guard only once upstream can both bound *and* cancel a connect: a bound alone would still leave the abandoned-attempt thread this works around (results file §7 C-5) | Issue F, drafted, **not submitted** |
| U-16 | every socket timeout is reported as "your call timeout expired" | `u16_a_socket_timeout_is_still_reported_as_an_expired_call_timeout` | offline | `error.rs::map_connect` reclassifies it to `ErrorKind::Connection` during connect | Issue F, drafted, **not submitted** |
| U-17 | nothing detects a dead link: no keepalive, `EXPIRE_TIME` goes nowhere | **manual check** — needs the S10 black-hole proxy and ≥ 30 s of waiting, so it is deliberately not a canary. Grep upstream for `SO_KEEPALIVE`/`set_keepalive` in `transport.rs`, and for any *read* of `Description::expire_time` beyond `build_description_segment` | — | none; a deadline on every call is the only lever and it costs the session (U-6) | Issue F, drafted, **not submitted** |
| U-18 | `CREATE TRIGGER` is impossible: `:NEW` is parsed as a bind placeholder | `u18_a_trigger_body_that_mentions_new_is_still_parsed_as_a_bind_placeholder`. It drives **upstream directly** (`raw_connect`), not this crate, so the automatic rewrite does not touch it and it still asserts the raw defect | live | `rewrite.rs` — `conn.rs::execute` rewrites any `CREATE … TRIGGER` whose text upstream would read a placeholder into as `BEGIN EXECUTE IMMEDIATE q'X…X'; END;`, **on by default**, with a `Warning` carrying the text sent; `EXT_REWRITE_TRIGGER_DDL = false` restores `error.rs::explain_parsed_placeholders`, which is still the guard for any other statement. `rewrite.rs::upstream_finds_a_bind_placeholder` is a literal transcription of `sql_parser.rs` and **must be re-derived** on an upgrade, like `classify::upstream_would_return_rows`. When the canary says FIXED, delete the rewrite rather than leaving it: it costs a 32767-byte limit and moves syntax-error positions (results file §5 U-18) | Issue G, drafted, **not submitted** |

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
