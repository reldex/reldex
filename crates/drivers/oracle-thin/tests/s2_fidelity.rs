//! Spike S2 — type fidelity (ADR-0001).
//!
//! Kill criteria: silent `NUMBER` precision loss, or corruption of Thai /
//! `NCHAR` data. Both are checked here by comparing what comes back against
//! what the server itself says is stored, with no `NLS_LANG` or other
//! environment variable set on the client.

#![cfg(feature = "oracle-it")]
#![allow(
    clippy::print_stdout,
    reason = "spike results are measurements a human reads from the test output"
)]

mod common;

use common::{
    connect, connect_decoding_timestamp_with_time_zone, exec, exec_quietly, measurement,
    observation, query, render, scalar, unique,
};
use reldex_db_driver_api::{
    Bind, ColumnData, DatabaseConnection, ErrorKind, Number, SessionState, SqlType, Statement,
    Timestamp, ValueRef,
};

/// Thai text, chosen because it exercises combining marks above and below the
/// base character as well as multi-byte UTF-8.
const THAI: &str = "ทดสอบภาษาไทย";
/// A non-BMP character: two UTF-16 code units, one Rust `char`, four UTF-8
/// bytes. This is the value that breaks a driver which sizes buffers in UCS-2
/// and splits a surrogate pair.
const EMOJI: &str = "🐘";

#[test]
fn number_values_survive_the_round_trip_without_silent_precision_loss() {
    let mut connection = connect();
    let table = unique("s2_num");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), v NUMBER)"),
    );

    // Values this driver can bind, as `(what we send, what Oracle stores)`.
    // Every one is also checked against the server's own `TO_CHAR` rendering,
    // so a difference between the two would fail rather than pass quietly.
    let cases: [(&str, &str); 11] = [
        ("0", "0"),
        ("1", "1"),
        ("-1", "-1"),
        (".5", "0.5"),
        ("-0.5", "-0.5"),
        ("123.45", "123.45"),
        // 38, 39 and 40 significant digits. Oracle's documented NUMBER
        // *precision* is 38, but its base-100 storage carries 40 digits and
        // this server returns all of them unchanged — which is exactly why the
        // contract's `Number` holds 40 rather than 38. Verified against the
        // server's own `TO_CHAR` below, so a future server that does round
        // will fail this test rather than pass it quietly.
        (
            "12345678901234567890123456789012345678",
            "12345678901234567890123456789012345678",
        ),
        (
            "123456789012345678901234567890123456789",
            "123456789012345678901234567890123456789",
        ),
        (
            "1234567890123456789012345678901234567891",
            "1234567890123456789012345678901234567891",
        ),
        // Even numbers of leading zeros after the point; the odd ones are
        // refused by this driver and covered by
        // `a_bound_number_is_never_silently_scaled_by_a_power_of_ten`.
        ("0.005", "0.005"),
        ("1E-129", ""),
    ];

    // Built rather than typed out: `1E-129` is 128 zeros then a one, and a
    // hand-counted literal would be testing the typist, not the driver.
    let tiny = format!("0.{}1", "0".repeat(128));
    let mut cases = cases;
    cases[10].1 = tiny.as_str();

    for (index, (sent, _expected)) in cases.iter().enumerate() {
        let value = Number::parse(sent).unwrap_or_else(|e| panic!("{sent}: {e}"));
        let statement = Statement::new(format!("INSERT INTO {table} (id, v) VALUES (:1, :2)"))
            .with_positional_binds(vec![
                Bind::input(i64::try_from(index).unwrap_or(0)),
                Bind::input(value),
            ]);
        connection
            .execute(&statement)
            .unwrap_or_else(|e| panic!("inserting {sent}: {e}"));
    }
    connection.commit().expect("commit");

    let batch = query(
        connection.as_mut(),
        &format!("SELECT id, v, TO_CHAR(v, 'TM') FROM {table} ORDER BY id"),
    );
    assert_eq!(batch.row_count(), cases.len());
    for (index, (sent, expected)) in cases.iter().enumerate() {
        let got = render(batch.value(index, 1).unwrap_or(ValueRef::Null));
        let expected_clean: String = expected.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            got, expected_clean,
            "sent {sent}: the driver produced {got}, the value stored is {expected_clean}"
        );
        // The server's own rendering of the same cell, as a second opinion that
        // does not go through the driver's NUMBER decoding at all.
        let server = render(batch.value(index, 2).unwrap_or(ValueRef::Null));
        if server.contains('E') {
            // `TO_CHAR(.., 'TM')` switches to scientific notation for very
            // small magnitudes; the exact comparison above already covers
            // those, so there is nothing left to cross-check here.
            continue;
        }
        assert_eq!(
            normalize(&server),
            normalize(&got),
            "sent {sent}: the driver says {got}, TO_CHAR says {server}"
        );
    }

    // The largest legal NUMBER cannot be *bound* on this upstream version (the
    // encoder panics, and the panic becomes a process abort), so it is written
    // as a literal and only read back. Reading it must still be exact.
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} (id, v) VALUES (90, 9.9999999999999999999999999999999999999E125)"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} (id, v) VALUES (91, -9.9999999999999999999999999999999999999E125)"
        ),
    );
    connection.commit().expect("commit");
    for (id, sign) in [(90, ""), (91, "-")] {
        let got = scalar(
            connection.as_mut(),
            &format!("SELECT v FROM {table} WHERE id = {id}"),
        );
        let server = scalar(
            connection.as_mut(),
            &format!("SELECT TO_CHAR(v, 'TM') FROM {table} WHERE id = {id}"),
        );
        // `TO_CHAR(.., 'TM')` renders this one in scientific notation, so the
        // cross-check is on the significant digits alone; the digit count below
        // is what pins the magnitude.
        assert_eq!(
            normalize(&got),
            normalize(server.split('E').next().unwrap_or(&server)),
            "9.99E125 read back wrong: driver {got}, server {server}"
        );
        assert!(got.starts_with(&format!("{sign}99999")), "got {got}");
        assert_eq!(
            got.chars().filter(char::is_ascii_digit).count(),
            126,
            "9.99E125 should be 126 digits long, got {got}"
        );
    }
    observation("9.99E125 and its negative read back digit-for-digit (inserted as literals)");

    observation(format!(
        "{} bound NUMBER cases round-tripped exactly, including 38-, 39- and \
         40-digit integers and 1E-129; each matched the server's own rendering",
        cases.len()
    ));

    // `1/3` has no finite decimal form; Oracle returns its own 38-digit
    // approximation, and the driver must reproduce it digit for digit.
    let third = scalar(connection.as_mut(), "SELECT 1/3 FROM dual");
    assert_eq!(
        third, "0.3333333333333333333333333333333333333333",
        "1/3 lost digits"
    );
    let server_third = scalar(connection.as_mut(), "SELECT TO_CHAR(1/3, 'TM') FROM dual");
    assert_eq!(normalize(&third), normalize(&server_third));
    observation(format!("1/3 = {third}"));

    // And the bind that would abort the process is refused with a clean error.
    let huge = Number::parse("9.9999999999999999999999999999999999999E125").expect("legal");
    let statement = Statement::new(format!("INSERT INTO {table} (id, v) VALUES (99, :1)"))
        .with_positional_binds(vec![Bind::input(huge)]);
    let error = match connection.execute(&statement) {
        Ok(_) => panic!("binding 9.99E125 must be refused, not attempted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), reldex_db_driver_api::ErrorKind::Unsupported);
    observation(format!("binding 9.99E125 -> {error}"));
    // The connection is still usable after the refusal.
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_bound_number_is_never_silently_scaled_by_a_power_of_ten() {
    // `oracledb`'s encoder tests `decimal_point_index % 2 == 1` to decide
    // whether the digit pairs need a leading zero. In Rust that test is false
    // for a *negative* odd index, which is exactly what a value below 0.1 with
    // an odd number of leading zeros produces — so the pairs are written one
    // base-100 place out and the stored value differs from the bound one by a
    // factor of ten. Found in spike S2; this is the regression guard.
    let mut connection = connect();
    let table = unique("s2_scale");
    exec(
        connection.as_mut(),
        &format!("CREATE TABLE {table} (id NUMBER(5), v NUMBER, sent VARCHAR2(200))"),
    );

    let probes = [
        "0.5", "0.05", "0.005", "0.0005", "0.00005", "0.123", "0.0123", "0.00123", "0.000123",
        "1E-3", "1E-4", "1E-129", "1E-130", "-0.05", "-0.005",
    ];
    let mut wrong = Vec::new();
    let mut refused = Vec::new();
    for (index, sent) in probes.iter().enumerate() {
        let value = Number::parse(sent).unwrap_or_else(|e| panic!("{sent}: {e}"));
        let canonical = value.to_string();
        let statement = Statement::new(format!(
            "INSERT INTO {table} (id, v, sent) VALUES (:1, :2, :3)"
        ))
        .with_positional_binds(vec![
            Bind::input(i64::try_from(index).unwrap_or(0)),
            Bind::input(value),
            Bind::input(canonical.clone()),
        ]);
        match connection.execute(&statement) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(
                    error.kind(),
                    reldex_db_driver_api::ErrorKind::Unsupported,
                    "{sent} failed for an unexpected reason: {error}"
                );
                observation(format!("{sent} refused: {}", error.message()));
                refused.push((*sent).to_owned());
                continue;
            }
        }
        let stored = scalar(
            connection.as_mut(),
            &format!("SELECT TO_CHAR(v, 'TM9') FROM {table} WHERE id = {index}"),
        );
        let agrees = scalar(
            connection.as_mut(),
            &format!(
                "SELECT CASE WHEN v = TO_NUMBER(sent) THEN 'same' ELSE 'DIFFERENT' END                       FROM {table} WHERE id = {index}"
            ),
        );
        observation(format!("bound {sent} -> stored {stored} ({agrees})"));
        if agrees != "same" {
            wrong.push(format!("{sent} was stored as {stored}"));
        }
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
    assert!(
        wrong.is_empty(),
        "bound NUMBER values were silently changed: {wrong:#?}"
    );
    // The refusals are the mitigation, and they must cover exactly the values
    // the upstream encoder gets wrong — no more, no less.
    refused.sort();
    assert_eq!(
        refused,
        vec![
            "-0.05".to_owned(),
            "0.000123".to_owned(),
            "0.0005".to_owned(),
            "0.0123".to_owned(),
            "0.05".to_owned(),
            "1E-130".to_owned(),
            "1E-4".to_owned(),
        ],
        "the set of refused values changed"
    );
}

/// `TO_CHAR(.., 'TM')` renders `.5` without the leading zero and uses `E` for
/// very small values; compare on the digits alone.
fn normalize(text: &str) -> String {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let digits = rest
        .replace(['.', '+'], "")
        .trim_start_matches('0')
        .trim_end_matches('0')
        .to_ascii_uppercase();
    format!("{sign}{digits}")
}

#[test]
fn character_data_round_trips_byte_exact_including_thai_and_a_non_bmp_character() {
    let mut connection = connect();
    let table = unique("s2_txt");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} (
                 id NUMBER(5),
                 c  CHAR(20 CHAR),
                 v  VARCHAR2(100 CHAR),
                 nv NVARCHAR2(100),
                 nc NCHAR(20)
             )"
        ),
    );

    let cases: [(&str, &str); 5] = [
        ("ascii", "hello"),
        ("thai", THAI),
        ("emoji", EMOJI),
        ("mixed", "a🐘ทb"),
        ("empty-ish", " "),
    ];
    for (index, (label, text)) in cases.iter().enumerate() {
        let statement = Statement::new(format!(
            "INSERT INTO {table} (id, c, v, nv, nc) VALUES (:1, :2, :3, :4, :5)"
        ))
        .with_positional_binds(vec![
            Bind::input(i64::try_from(index).unwrap_or(0)),
            Bind::input(*text),
            Bind::input(*text),
            Bind::input(*text),
            Bind::input(*text),
        ]);
        connection
            .execute(&statement)
            .unwrap_or_else(|e| panic!("inserting {label}: {e}"));
    }
    connection.commit().expect("commit");

    // The server's own opinion of each stored value, independent of the
    // driver's character decoding: the code point list and the byte length.
    let batch = query(
        connection.as_mut(),
        &format!(
            "SELECT id, c, v, nv, nc,
                    DUMP(v, 1016) AS v_dump,
                    LENGTHB(v) AS v_bytes,
                    LENGTH(v) AS v_chars
             FROM {table} ORDER BY id"
        ),
    );
    assert_eq!(batch.row_count(), cases.len());

    for (index, (label, text)) in cases.iter().enumerate() {
        let c = render(batch.value(index, 1).unwrap_or(ValueRef::Null));
        let v = render(batch.value(index, 2).unwrap_or(ValueRef::Null));
        let nv = render(batch.value(index, 3).unwrap_or(ValueRef::Null));
        let nc = render(batch.value(index, 4).unwrap_or(ValueRef::Null));
        let dump = render(batch.value(index, 5).unwrap_or(ValueRef::Null));
        let bytes = render(batch.value(index, 6).unwrap_or(ValueRef::Null));

        assert_eq!(v, *text, "VARCHAR2 corrupted for {label}");
        assert_eq!(nv, *text, "NVARCHAR2 corrupted for {label}");
        // CHAR and NCHAR are blank-padded by the server to their declared
        // length in characters. The driver must not strip that padding: it is
        // part of the stored value.
        assert_eq!(
            c,
            format!("{text}{}", " ".repeat(20 - text.chars().count())),
            "CHAR padding wrong for {label}"
        );
        // `NCHAR(20)` is twenty AL16UTF16 code units, not twenty characters:
        // the elephant occupies two of them, so it is padded with eighteen
        // spaces where `CHAR(20 CHAR)` pads with nineteen. That is the server's
        // rule and the driver must not normalise it away.
        assert_eq!(
            nc,
            format!("{text}{}", " ".repeat(20 - text.encode_utf16().count())),
            "NCHAR padding wrong for {label}"
        );
        assert_eq!(
            bytes,
            text.len().to_string(),
            "{label}: the server counts {bytes} bytes, Rust counts {}",
            text.len()
        );
        observation(format!("{label}: {v} ({bytes} bytes) DUMP={dump}"));
    }

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn temporal_values_round_trip_including_dates_the_gregorian_calendar_rejects() {
    // The offset-only form of `TIMESTAMP WITH TIME ZONE` decodes correctly, and
    // this is the test that proves it — so it opts in explicitly. The driver
    // refuses the type by default because a *named region* aborts the process
    // and the two forms are indistinguishable before the decode; see
    // `a_timestamp_with_time_zone_column_is_refused_before_anything_is_fetched`.
    let mut connection = connect_decoding_timestamp_with_time_zone();
    let table = unique("s2_tim");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} (
                 id  NUMBER(5),
                 d   DATE,
                 ts  TIMESTAMP(9),
                 tz  TIMESTAMP(9) WITH TIME ZONE
             )"
        ),
    );

    // Inserted as literals so the server, not the driver, decides what is
    // stored; the driver is only asked to read it back.
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} VALUES (
                 1,
                 TO_DATE('2026-09-19 13:45:30', 'YYYY-MM-DD HH24:MI:SS'),
                 TO_TIMESTAMP('2026-09-19 13:45:30.123456789', 'YYYY-MM-DD HH24:MI:SS.FF9'),
                 TO_TIMESTAMP_TZ('2026-09-19 13:45:30.123456789 +07:00',
                                 'YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM'))"
        ),
    );
    // A BC date and a Julian-calendar leap day that the proleptic Gregorian
    // calendar does not have. Oracle stores both; a driver that validates with
    // Gregorian rules would refuse to return them.
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} VALUES (
                 2,
                 TO_DATE('-0044-03-15', 'SYYYY-MM-DD'),
                 TO_TIMESTAMP('-0044-03-15 11:00:00', 'SYYYY-MM-DD HH24:MI:SS'),
                 NULL)"
        ),
    );
    exec(
        connection.as_mut(),
        &format!(
            "INSERT INTO {table} VALUES (
                 3,
                 TO_DATE('1500-02-29', 'YYYY-MM-DD'),
                 TO_TIMESTAMP('1500-02-29 00:00:01', 'YYYY-MM-DD HH24:MI:SS'),
                 NULL)"
        ),
    );
    connection.commit().expect("commit");

    let batch = query(
        connection.as_mut(),
        &format!(
            "SELECT id, d, ts, tz,
                    TO_CHAR(d, 'SYYYY-MM-DD HH24:MI:SS') AS d_text,
                    TO_CHAR(ts, 'SYYYY-MM-DD HH24:MI:SS.FF9') AS ts_text
             FROM {table} ORDER BY id"
        ),
    );
    assert_eq!(batch.row_count(), 3);

    for row in 0..batch.row_count() {
        let d = render(batch.value(row, 1).unwrap_or(ValueRef::Null));
        let ts = render(batch.value(row, 2).unwrap_or(ValueRef::Null));
        let tz = render(batch.value(row, 3).unwrap_or(ValueRef::Null));
        let d_text = render(batch.value(row, 4).unwrap_or(ValueRef::Null));
        let ts_text = render(batch.value(row, 5).unwrap_or(ValueRef::Null));
        observation(format!(
            "row {}: DATE={d} (server {d_text}) TS={ts} (server {ts_text}) TZ={tz}",
            row + 1
        ));
    }

    // Row 1: the ordinary case, exact to the nanosecond and with its offset.
    assert_eq!(
        render(batch.value(0, 1).unwrap_or(ValueRef::Null)),
        "2026-09-19T13:45:30"
    );
    assert_eq!(
        render(batch.value(0, 2).unwrap_or(ValueRef::Null)),
        "2026-09-19T13:45:30.123456789"
    );
    assert_eq!(
        render(batch.value(0, 3).unwrap_or(ValueRef::Null)),
        "2026-09-19T13:45:30.123456789+07:00"
    );

    // Row 2: 15 March 44 BC. Oracle's `SYYYY` writes the astronomical year, so
    // `-0044` is the year the driver must report.
    let bc = render(batch.value(1, 1).unwrap_or(ValueRef::Null));
    assert!(bc.starts_with("-0044-03-15"), "BC date came back as {bc}");

    // Row 3: 29 February 1500 exists in the Julian calendar Oracle uses before
    // 1582, and must survive rather than be rejected as invalid.
    let julian = render(batch.value(2, 1).unwrap_or(ValueRef::Null));
    assert!(
        julian.starts_with("1500-02-29"),
        "1500-02-29 came back as {julian}"
    );

    // Binding a timestamp back in must produce the same instant.
    let bound = Timestamp::new(2026, 9, 19, 13, 45, 30)
        .expect("valid")
        .with_nanosecond(123_456_789)
        .expect("valid");
    let statement = Statement::new(format!("INSERT INTO {table} (id, ts) VALUES (9, :1)"))
        .with_positional_binds(vec![Bind::input(bound)]);
    connection.execute(&statement).expect("bind a timestamp");
    connection.commit().expect("commit");
    let back = scalar(
        connection.as_mut(),
        &format!("SELECT ts FROM {table} WHERE id = 9"),
    );
    assert_eq!(back, "2026-09-19T13:45:30.123456789");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

/// U-3's mitigation: a `TIMESTAMP WITH TIME ZONE` column is refused on the
/// **describe**, before a single value has been decoded.
///
/// The defect it contains is two upstream defects compounding, and the second
/// is the serious one:
///
/// 1. `src/ora_type/timestamp.rs:238` is a bare `todo!()` for the
///    region-encoded form of `TIMESTAMP WITH TIME ZONE` (`buf[11] & 0x80`).
/// 2. That panic unwinds while the client mutex is held, poisoning it, and
///    `impl Drop for StatementHolder` (`src/statement/holder.rs:140`) then does
///    `self.client_ref.lock().unwrap()` — a panic during unwinding, which
///    aborts the process. So `catch_unwind` cannot contain it, and **no**
///    upstream panic can ever be contained by a wrapper.
///
/// Region and offset cannot be told apart before the decode — the flag is a bit
/// in the value's own wire bytes, which `oracledb` reads inside the round trip,
/// and the describe says only `TIMESTAMP WITH TIME ZONE`. So the refusal is per
/// column, and this test uses the named-region value that used to kill the
/// process: it now produces an ordinary error and leaves the session usable.
#[test]
fn a_timestamp_with_time_zone_column_is_refused_before_anything_is_fetched() {
    let mut connection = connect();
    let sql = "SELECT TO_TIMESTAMP_TZ('2026-01-01 00:00:00 Asia/Bangkok',
                                      'YYYY-MM-DD HH24:MI:SS TZR') AS tstz FROM dual";
    let error = match connection.execute(&Statement::new(sql)) {
        Ok(_) => panic!("a TIMESTAMP WITH TIME ZONE column must be refused, not fetched"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    assert_eq!(
        error.session_state(),
        SessionState::Usable,
        "refusing a column must not cost the session"
    );
    assert!(
        error.message().contains("TSTZ") && error.message().contains("TIMESTAMP WITH TIME ZONE"),
        "the refusal must name the column and the type: {error}"
    );
    observation(format!("named-region TSTZ column -> {error}"));

    // The same column inside a wider select list is refused too: the decode
    // happens for the whole row inside the upstream round trip, so there is no
    // per-column escape — reporting one cell as `Unsupported` would not help.
    let error = match connection.execute(&Statement::new(
        "SELECT 1 AS ordinary, SYSTIMESTAMP AS tstz, 2 AS after FROM dual",
    )) {
        Ok(_) => panic!("a TIMESTAMP WITH TIME ZONE anywhere in the select list must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");

    // The session is untouched: it is a refusal, not a failure.
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");
    // And the documented escape works without the driver rewriting anything.
    let text = scalar(
        connection.as_mut(),
        "SELECT TO_CHAR(TO_TIMESTAMP_TZ('2026-01-01 00:00:00 Asia/Bangkok',
                                        'YYYY-MM-DD HH24:MI:SS TZR'),
                        'YYYY-MM-DD HH24:MI:SS TZR') FROM dual",
    );
    assert_eq!(text, "2026-01-01 00:00:00 ASIA/BANGKOK");
    observation(format!(
        "the documented TO_CHAR escape reads it as text: {text}"
    ));
    connection.close().expect("close");
}

/// The same refusal on the OUT-bind path, which has no describe to inspect.
#[test]
fn a_timestamp_with_time_zone_output_bind_is_refused_before_the_statement_runs() {
    let mut connection = connect();
    let statement = Statement::new("BEGIN :1 := SYSTIMESTAMP; END;")
        .with_positional_binds(vec![Bind::output(SqlType::TimestampWithTimeZone)]);
    let error = match connection.execute(&statement) {
        Ok(_) => panic!("a TIMESTAMP WITH TIME ZONE output bind must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Unsupported, "{error}");
    observation(format!("TSTZ output bind -> {error}"));
    assert_eq!(scalar(connection.as_mut(), "SELECT 1 FROM dual"), "1");
    connection.close().expect("close");
}

/// **This test aborts the process on `oracledb` 26.0.0-beta.3.** It is what the
/// refusal above exists to prevent: it turns the guard off through the
/// documented extension and reads a named region anyway. It stays in the suite,
/// ignored by default, as an executable record of the defect and as the check
/// that will pass once upstream fixes it. Run it on its own:
///
/// ```text
/// cargo test -p reldex-driver-oracle-thin --features oracle-it ///     --test s2_fidelity -- --ignored --exact a_named_time_zone_region_is_read_or_reported
/// ```
#[test]
#[ignore = "aborts the process on oracledb 26.0.0-beta.3; this is what the default refusal prevents"]
fn a_named_time_zone_region_is_read_or_reported() {
    let outcome = std::panic::catch_unwind(|| {
        let mut connection = connect_decoding_timestamp_with_time_zone();
        let text = scalar(
            connection.as_mut(),
            "SELECT TO_TIMESTAMP_TZ('2026-09-19 13:45:30 Asia/Bangkok',
                                    'YYYY-MM-DD HH24:MI:SS TZR') FROM dual",
        );
        connection.close().expect("close");
        text
    });
    match outcome {
        Ok(text) => observation(format!("named region TSTZ -> {text}")),
        Err(payload) => {
            let detail = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("<non-string panic>");
            panic!(
                "UPSTREAM GAP: reading a TIMESTAMP WITH TIME ZONE carrying a named \
                 region panics inside oracledb: {detail}"
            );
        }
    }
}

#[test]
fn raw_bytes_and_nulls_of_every_type_survive() {
    // Opted in for the `tz` column: this test is about NULLs of every type,
    // including a NULL `TIMESTAMP WITH TIME ZONE`.
    let mut connection = connect_decoding_timestamp_with_time_zone();
    let table = unique("s2_raw");
    exec(
        connection.as_mut(),
        &format!(
            "CREATE TABLE {table} (
                 id NUMBER(5), n NUMBER, c CHAR(4), v VARCHAR2(20), nv NVARCHAR2(20),
                 d DATE, ts TIMESTAMP(6), tz TIMESTAMP(6) WITH TIME ZONE,
                 r RAW(16), bf BINARY_FLOAT, bd BINARY_DOUBLE
             )"
        ),
    );

    let bytes = vec![0x00_u8, 0xFF, 0x10, 0x7F, 0x80, 0xDE, 0xAD, 0xBE, 0xEF];
    let statement = Statement::new(format!("INSERT INTO {table} (id, r) VALUES (1, :1)"))
        .with_positional_binds(vec![Bind::input(bytes.clone())]);
    connection.execute(&statement).expect("bind RAW");
    // A row that is NULL in every nullable column.
    exec(
        connection.as_mut(),
        &format!("INSERT INTO {table} (id) VALUES (2)"),
    );
    connection.commit().expect("commit");

    let batch = query(
        connection.as_mut(),
        &format!("SELECT id, n, c, v, nv, d, ts, tz, r, bf, bd FROM {table} ORDER BY id"),
    );
    assert_eq!(batch.row_count(), 2);

    let raw = render(batch.value(0, 8).unwrap_or(ValueRef::Null));
    assert_eq!(raw, "00FF107F80DEADBEEF", "RAW bytes changed");

    // Every nullable column of row 2 must read back as NULL — not as an empty
    // string, not as zero, and not as a placeholder that escaped the mask.
    for column in 1..batch.column_count() {
        let value = batch.value(1, column).unwrap_or(ValueRef::Text("<none>"));
        assert!(
            value.is_null(),
            "column {column} of the all-NULL row read back as {}",
            render(value)
        );
    }
    observation("RAW round-tripped byte-exact; NULLs of 10 types all read back as NULL");

    exec_quietly(connection.as_mut(), &format!("DROP TABLE {table} PURGE"));
    connection.close().expect("close");
}

#[test]
fn a_type_the_contract_cannot_hold_becomes_text_instead_of_failing_the_batch() {
    let mut connection = connect();
    // `INTERVAL`, `ROWID` and `TIMESTAMP WITH LOCAL TIME ZONE` have no variant
    // in the contract. The rule (ADR-0002) is that the rest of the row must
    // still arrive: one odd column must not hide a whole table.
    let batch = query(
        connection.as_mut(),
        "SELECT 42 AS ordinary,
                INTERVAL '3' DAY AS ds,
                INTERVAL '2-6' YEAR TO MONTH AS ym,
                ROWID AS rid,
                CAST(SYSTIMESTAMP AS TIMESTAMP WITH LOCAL TIME ZONE) AS ltz,
                'after' AS still_here
         FROM dual",
    );
    assert_eq!(batch.row_count(), 1);
    assert_eq!(batch.column_count(), 6);

    assert_eq!(render(batch.value(0, 0).unwrap_or(ValueRef::Null)), "42");
    assert_eq!(
        render(batch.value(0, 5).unwrap_or(ValueRef::Null)),
        "after",
        "the column after the unsupported ones was lost"
    );

    for column in 1..=4 {
        let value = batch.value(0, column).unwrap_or(ValueRef::Null);
        let text = value
            .as_unsupported_text()
            .unwrap_or_else(|| panic!("column {column} is {value:?}, not Unsupported text"));
        assert!(!text.is_empty(), "column {column} rendered as empty text");
        observation(format!("unsupported column {column} -> {text}"));
    }

    // And the metadata says so honestly, with the server's own type name.
    let mut outcome = connection
        .execute(&Statement::new(
            "SELECT INTERVAL '3' DAY AS ds, ROWID AS rid FROM dual",
        ))
        .expect("select");
    let cursor = outcome.take_cursor().expect("cursor");
    for column in cursor.columns() {
        assert_eq!(column.sql_type(), SqlType::Unsupported);
        let native = column.native_type_name().unwrap_or("<none>");
        assert_ne!(
            native,
            "<none>",
            "no native type name for {}",
            column.name()
        );
        observation(format!("{} is reported as {native}", column.name()));
    }
    cursor.close().expect("close cursor");

    connection.close().expect("close");
}

#[test]
fn a_batch_is_column_oriented_and_honours_the_requested_size() {
    let mut connection = connect();
    let mut outcome = connection
        .execute(
            &Statement::new(
                "SELECT level AS n, 'row ' || level AS t FROM dual CONNECT BY level <= 250",
            )
            .with_fetch_rows(std::num::NonZeroUsize::new(50).expect("non-zero")),
        )
        .expect("select");
    let mut cursor = outcome.take_cursor().expect("cursor");

    let mut seen = 0_usize;
    let mut batches = 0_usize;
    loop {
        let batch = cursor
            .fetch_batch(std::num::NonZeroUsize::new(64).expect("non-zero"))
            .expect("fetch");
        if batch.row_count() == 0 {
            break;
        }
        assert!(batch.row_count() <= 64, "fetch_batch overshot its bound");
        batches += 1;
        // Column-oriented: the storage is one buffer per column, not per row.
        let column = batch.column(0).expect("first column");
        assert!(matches!(
            column.kind(),
            reldex_db_driver_api::ColumnKind::Number
        ));
        assert!(matches!(
            batch.column(1).map(Column::kind),
            Some(reldex_db_driver_api::ColumnKind::Text)
        ));
        seen += batch.row_count();
    }
    cursor.close().expect("close");
    assert_eq!(seen, 250);
    observation(format!("250 rows arrived in {batches} bounded batches"));
    connection.close().expect("close");
}

/// What the U-3 mitigation costs on the fetch path.
///
/// The driver asks `oracledb` for **zero** prefetched rows so the execute round
/// trip returns column metadata only and an undecodable select list can be
/// refused before any value is decoded. A query therefore pays one round trip
/// for the describe and another for the first batch, where it used to get the
/// first couple of rows with the execute. This measures what that is worth
/// against the Phase 0 container, next to a `ping` — one bare round trip — so
/// the two can be compared rather than asserted.
#[test]
fn describing_before_fetching_costs_about_one_extra_round_trip() {
    let mut connection = connect();
    let rounds = 20;
    let mut pings = Vec::with_capacity(rounds);
    let mut queries = Vec::with_capacity(rounds);
    let batch_size = std::num::NonZeroUsize::new(100).expect("non-zero");

    for _ in 0..rounds {
        let started = Instant::now();
        connection.ping().expect("ping");
        pings.push(started.elapsed());

        let started = Instant::now();
        let mut outcome = connection
            .execute(&Statement::new("SELECT 1 FROM dual"))
            .expect("select");
        let mut cursor = outcome.take_cursor().expect("cursor");
        assert_eq!(
            cursor.fetch_batch(batch_size).expect("fetch").row_count(),
            1
        );
        cursor.close().expect("close");
        queries.push(started.elapsed());
    }

    pings.sort_unstable();
    queries.sort_unstable();
    let ping = pings[pings.len() / 2];
    let query = queries[queries.len() / 2];
    measurement("s2.ping_median", format!("{ping:.1?}"));
    measurement("s2.one_row_query_median", format!("{query:.1?}"));
    observation(format!(
        "a one-row query (describe round trip + fetch round trip + close) took a \
         median {query:.1?} against a median {ping:.1?} for one bare round trip"
    ));
    // Not a performance assertion, a sanity bound: if the describe-first change
    // ever cost an order of magnitude rather than a round trip, this fails.
    assert!(
        query < ping * 20,
        "a one-row query took {query:.1?} against a {ping:.1?} round trip"
    );
    connection.close().expect("close");
}

use reldex_db_driver_api::Column;
use std::time::Instant;

/// Keeps the import list honest about the enum used in the assertions above.
const _: fn(&ColumnData) -> usize = ColumnData::len;

/// Keeps the connection trait object nameable in this file's signatures.
const _: fn(&mut dyn DatabaseConnection) -> &mut dyn DatabaseConnection = |c| c;
