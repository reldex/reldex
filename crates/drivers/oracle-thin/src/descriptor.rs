//! What this driver reads out of a connect descriptor before it opens a socket.
//!
//! One thing is read today, and it is read because ignoring it would be a
//! silent security downgrade: Oracle's two server-certificate parameters,
//! `SSL_SERVER_CERT_DN` and `SSL_SERVER_DN_MATCH`.
//!
//! `oracledb` 26.0.0-beta.3 parses both out of a descriptor
//! (`config/connect_options.rs:502-507`), defaults `SSL_SERVER_DN_MATCH` to
//! true (`:544`) and writes both into the `SECURITY` segment it sends to the
//! server (`:386-391`) — and **nothing in its TLS layer ever reads either of
//! them** (upstream gap U-14, spike S8). What is verified instead is `rustls`'s
//! default: the certificate chain, its validity dates, the `serverAuth`
//! extended key usage, and the **host name** taken from the descriptor's `HOST`
//! matched against the certificate's `subjectAltName`. None of that can be
//! turned off, and this driver would not offer a way if it could.
//!
//! The two parameters therefore mean opposite things to this driver:
//!
//! - `SSL_SERVER_DN_MATCH` asks for the server's identity to be checked, which
//!   already happens unconditionally and by a stricter mechanism, or asks for
//!   the check to be skipped, which cannot be honoured. Either way the session
//!   that opens is at least as safe as the one the profile asked for, so the
//!   parameter is **accepted and reported**, never refused.
//! - `SSL_SERVER_CERT_DN` asks for the server certificate's full distinguished
//!   name to be **pinned**. Nothing here pins it. Opening the session anyway
//!   would hand the caller a weaker guarantee than the one they configured,
//!   without saying so, which is precisely the shape of failure `SPEC.md` §2
//!   rules out. It is **refused**, unless the caller opts in and accepts that
//!   the pin is not applied.
//!
//! # The one shape this cannot see
//!
//! A `tnsnames.ora` **alias whose entry is plaintext**. Neither source reaches
//! it: the connect string is just the alias name, and upstream's
//! `build_description_segment` emits the `SECURITY` segment only when an
//! address in the description says `TCPS`, so `get_connect_descriptor` renders
//! the entry with `SSL_SERVER_CERT_DN` (and any `SSL_SERVER_DN_MATCH=OFF`)
//! stripped out. A TCPS alias is seen; a TCP one is not.
//!
//! It is left open rather than worked around, because closing it would mean
//! parsing `tnsnames.ora` in this driver — a second, divergent implementation
//! of the thing whose divergence this module exists to prevent — and because
//! what is lost is small: the entry is plaintext, so there is no TLS session
//! whose guarantee could be weaker than advertised. A descriptor written out in
//! full is covered whatever its protocol. `descriptor::tests` pins the
//! limitation explicitly, so a future upstream that renders the segment
//! unconditionally turns it into a failing test rather than a surprise.

use reldex_db_driver_api::{DbError, DbResult, ErrorKind, Warning, WarningKind};

use crate::conn::EXT_ALLOW_UNENFORCED_SERVER_CERT_DN;

/// The descriptor key that pins the server certificate's distinguished name.
const CERT_DN_KEY: &str = "ssl_server_cert_dn";

/// The descriptor key that asks for distinguished-name matching.
const DN_MATCH_KEY: &str = "ssl_server_dn_match";

/// What a connect descriptor asks for by way of Oracle's own server-certificate
/// checks.
///
/// Every field is "the descriptor asked for this", never "this is in force":
/// none of them is in force, which is the whole point of the type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerCertificateRequest {
    /// `SSL_SERVER_CERT_DN` was given a non-empty value.
    pub(crate) pins_certificate_dn: bool,
    /// `SSL_SERVER_DN_MATCH` was present and asks for matching to be **on**.
    pub(crate) asks_dn_match_on: bool,
    /// `SSL_SERVER_DN_MATCH` was present and asks for matching to be **off**.
    pub(crate) asks_dn_match_off: bool,
}

impl ServerCertificateRequest {
    /// Whether the descriptor mentioned either parameter at all.
    pub(crate) const fn is_empty(self) -> bool {
        !self.pins_certificate_dn && !self.asks_dn_match_on && !self.asks_dn_match_off
    }

    /// Everything either source found.
    pub(crate) const fn merged_with(self, other: Self) -> Self {
        Self {
            pins_certificate_dn: self.pins_certificate_dn || other.pins_certificate_dn,
            asks_dn_match_on: self.asks_dn_match_on || other.asks_dn_match_on,
            asks_dn_match_off: self.asks_dn_match_off || other.asks_dn_match_off,
        }
    }

    /// Reads the parameters out of the connect string the caller supplied.
    ///
    /// The scan is deliberately dumb: a case-insensitive search for the key
    /// anywhere in the text, with no quote tracking and no parenthesis depth. A
    /// descriptor is free text — arbitrary whitespace, newlines, quoted values,
    /// nested `(SECURITY=(…))`, several `ADDRESS`es or a whole
    /// `DESCRIPTION_LIST` — and a scanner clever enough to skip a quoted region
    /// is also a scanner that can lose its place and walk past the occurrence
    /// that matters. The two directions are not symmetric: a `SSL_SERVER_CERT_DN`
    /// that slips through unnoticed is the failure this guard exists to prevent,
    /// while a descriptor refused because another parameter's *value* happens to
    /// spell the key costs the caller an edit and an error message that says
    /// exactly what was found.
    pub(crate) fn scan_connect_string(text: &str) -> Self {
        // ASCII lowering leaves byte lengths — and therefore every offset —
        // untouched, so the copy can be scanned as if it were the original.
        let lowered = text.to_ascii_lowercase();
        let mut found = Self::default();
        for value in values_of(&lowered, CERT_DN_KEY, CERT_DN_TERMINATORS) {
            // "Non-empty" has to mean what upstream means by it, and upstream
            // means `Node::has_value` — `!value.is_empty()` on the value **as
            // stored**, which for a quoted value is untrimmed. So
            // `(SSL_SERVER_CERT_DN=" ")` is a pin upstream keeps and forwards,
            // and an earlier version of this scanner trimmed it away and let
            // the session open. `value_at` reproduces upstream's storage; the
            // emptiness test is applied to it unchanged.
            found.pins_certificate_dn |= !value.is_empty();
        }
        for value in values_of(&lowered, DN_MATCH_KEY, DN_MATCH_TERMINATORS) {
            if value.is_empty() {
                // `process_child_nodes` skips a node whose value is empty, so
                // upstream never sees this one and keeps its default. Warning
                // about it would be warning about something nobody set.
                continue;
            }
            // Upstream's own reading of the value, transcribed: `Node::as_bool`
            // trims, lower-cases and accepts `on`, `yes` and `true`, and calls
            // **everything else** false — `no`, `off`, `false`, and any typo.
            // Mirroring it means the driver warns about what upstream would
            // have done, not about what Oracle documents. The `trim` is
            // upstream's and belongs here rather than in `value_at`, because
            // the `SSL_SERVER_CERT_DN` reading above must *not* trim.
            if matches!(value.trim(), "on" | "yes" | "true") {
                found.asks_dn_match_on = true;
            } else {
                found.asks_dn_match_off = true;
            }
        }
        found
    }

    /// Reads the parameters back out of the descriptor **upstream** rebuilt.
    ///
    /// This is the second source, and it exists for the case the first cannot
    /// see: `Config::set_connect_string` resolves a `tnsnames.ora` alias when
    /// the connect string is not a descriptor at all, so the parameters may come
    /// from a file whose text this driver never holds. `get_connect_descriptor`
    /// renders whatever upstream parsed, alias included.
    ///
    /// Its rules are **not** the caller's text's rules, because upstream's
    /// rendering is normalized rather than quoted:
    ///
    /// - `(SSL_SERVER_CERT_DN=…)` appears only when upstream parsed one, so its
    ///   presence is a reliable positive.
    /// - `(SSL_SERVER_DN_MATCH=ON)` appears on **every** TCPS descriptor,
    ///   because upstream defaults the flag to true — so its presence says
    ///   nothing about what the caller wrote and must never be read as a
    ///   request. Its *absence* from a `SECURITY` segment does mean something:
    ///   the segment is emitted for TCPS addresses and always carries the flag
    ///   unless it was explicitly set to a non-true value. So an emitted
    ///   `(SECURITY=…)` without the flag is the caller asking for matching off.
    pub(crate) fn scan_upstream_descriptor(text: &str) -> Self {
        let lowered = text.to_ascii_lowercase();
        let pins_certificate_dn = values_of(&lowered, CERT_DN_KEY, CERT_DN_TERMINATORS)
            .into_iter()
            .any(|value| !value.is_empty());
        let asks_dn_match_off = lowered.contains("(security=") && !lowered.contains(DN_MATCH_KEY);
        Self {
            pins_certificate_dn,
            asks_dn_match_on: false,
            asks_dn_match_off,
        }
    }

    /// Turns what the descriptor asked for into a refusal, or into the warnings
    /// that say what was ignored.
    ///
    /// `allow_unenforced_pin` is [`EXT_ALLOW_UNENFORCED_SERVER_CERT_DN`].
    pub(crate) fn guard(self, allow_unenforced_pin: bool) -> DbResult<Vec<Warning>> {
        let mut warnings = Vec::new();
        if self.pins_certificate_dn {
            if !allow_unenforced_pin {
                return Err(DbError::new(ErrorKind::Configuration, refusal()));
            }
            warnings.push(Warning::new(WarningKind::Informational, pin_not_applied()));
        }
        if self.asks_dn_match_off {
            warnings.push(Warning::new(
                WarningKind::Informational,
                DN_MATCH_CANNOT_BE_DISABLED,
            ));
        }
        if self.asks_dn_match_on {
            warnings.push(Warning::new(
                WarningKind::Informational,
                DN_MATCH_ALREADY_STRICTER,
            ));
        }
        Ok(warnings)
    }
}

/// Where a **bare** `SSL_SERVER_CERT_DN` value is taken to end.
///
/// Parentheses only, which is upstream's own rule (`parse_descriptor_value`
/// reads `parse_token(|ch| ch != ')')`). Nothing else is added here on purpose:
/// every extra terminator can only shorten the value, and the one thing that
/// must never happen to this parameter is a non-empty pin being shortened to
/// nothing. `(` is included because a value that *starts* with one is a
/// container node, which upstream rejects outright with
/// `InvalidDescriptorNode` — so that shape cannot connect either way.
const CERT_DN_TERMINATORS: &[char] = &['(', ')'];

/// Where a bare `SSL_SERVER_DN_MATCH` value is taken to end.
///
/// The same, plus the Easy Connect Plus separators. Upstream's Easy Connect
/// parser has no query-parameter arm at all, so
/// `tcps://db:2484/SVC?ssl_server_dn_match=on&retry_count=3` never reaches it
/// as a parameter — but this scanner still reads the text, and without `&` it
/// read the value as `on&retry_count=3`, decided that was not `on`, and told a
/// user who had written ON that verification could not be switched off. The
/// extra terminators are safe **here** and not for the pin above, because this
/// parameter is classified rather than tested for emptiness: shortening it can
/// only change which of two warnings is emitted, never whether a session opens.
const DN_MATCH_TERMINATORS: &[char] = &['(', ')', '&', ';'];

/// Every value the key is given in this text, in the order they appear.
///
/// The text must already be ASCII-lowercased. A hit with no `=` after it is not
/// a parameter in any descriptor grammar — upstream's parser requires
/// `(key=value)` — so it is skipped rather than counted.
fn values_of<'a>(text: &'a str, key: &str, terminators: &[char]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        rest = &rest[at + key.len()..];
        if let Some(after) = rest.trim_start().strip_prefix('=') {
            values.push(value_at(after, terminators));
        }
    }
    values
}

/// The value that starts at `text`, exactly as upstream would store it.
///
/// Transcribed from `connect_string_parser.rs`'s `parse_descriptor_value`,
/// because the callers compare against `Node::has_value` and `Node::as_bool`
/// and a scanner that is merely *reasonable* here is a scanner that disagrees
/// with upstream about whether a pin exists:
///
/// - **`"` is the only quote.** A single quote is an ordinary character, so
///   `(SSL_SERVER_CERT_DN='CN=db')` is the bare value `'cn=db'` — non-empty,
///   and upstream keeps it. Treating `'` as a delimiter turned `''CN=db,O=x''`
///   and `''` into empty values and let both through silently.
/// - **A quoted value is not trimmed.** `parse_delimited_text` returns the
///   inner text verbatim, and `has_value` tests *that*, so `" "` is a pin
///   upstream forwards. Trimming it here made it vanish.
/// - **A bare value is trimmed**, which upstream does do
///   (`token.unwrap_or_default().trim()`), so `(SSL_SERVER_CERT_DN=   )` really
///   is empty and really is dropped by `process_child_nodes`.
fn value_at<'a>(text: &'a str, terminators: &[char]) -> &'a str {
    let text = text.trim_start();
    if let Some(inner) = text.strip_prefix('"') {
        return inner.split('"').next().unwrap_or(inner);
    }
    text.split(terminators).next().unwrap_or(text).trim()
}

/// The refusal for an unenforceable distinguished-name pin.
///
/// Built rather than declared because it names the extension key, and a message
/// that quotes a key by hand is a message that survives the key being renamed.
fn refusal() -> String {
    format!(
        "this connection's endpoint sets SSL_SERVER_CERT_DN, which asks for the server \
         certificate's whole distinguished name to be pinned — and this driver version \
         cannot enforce it. The Oracle crate it wraps parses the parameter and sends it to \
         the server, but its TLS layer never reads it (upstream gap U-14), so the session \
         would be opened with a weaker guarantee than the profile asked for and nothing \
         would say so. What is verified on every TCPS session, and cannot be turned off: \
         the certificate chain, its validity dates, the serverAuth extended key usage, and \
         the host name from the descriptor's HOST against the certificate's subjectAltName \
         — never the DN. Either remove SSL_SERVER_CERT_DN from the descriptor, or set the \
         connection extension \"{EXT_ALLOW_UNENFORCED_SERVER_CERT_DN}\" to open the session \
         knowing the pin is not applied"
    )
}

/// The warning that replaces the refusal once the caller has opted in.
fn pin_not_applied() -> String {
    format!(
        "this connection's endpoint sets SSL_SERVER_CERT_DN and the extension \
         \"{EXT_ALLOW_UNENFORCED_SERVER_CERT_DN}\" is set, so the session was opened \
         without the distinguished-name pin the descriptor asked for: the Oracle crate this \
         driver wraps never applies it (upstream gap U-14). The server certificate's chain, \
         validity, serverAuth extended key usage and host name against subjectAltName were \
         verified as they always are"
    )
}

/// The warning for a descriptor that asks for matching to be switched off.
const DN_MATCH_CANNOT_BE_DISABLED: &str = "this connection's endpoint asks for SSL_SERVER_DN_MATCH to be off, and this driver \
     cannot switch server-identity verification off. The Oracle crate it wraps never reads \
     the parameter (upstream gap U-14), and the TLS verifier underneath has no way to \
     disable the check: the host name from the descriptor's HOST is matched against the \
     certificate's subjectAltName on every TCPS session. A connection that relied on the \
     check being disabled will fail rather than fall back";

/// The warning for a descriptor that asks for matching to be on.
const DN_MATCH_ALREADY_STRICTER: &str = "this connection's endpoint sets SSL_SERVER_DN_MATCH, which this driver does not use: \
     the Oracle crate it wraps sends the parameter to the server and its TLS layer never \
     reads it (upstream gap U-14). The server's identity is verified regardless — the host \
     name from the descriptor's HOST is matched against the certificate's subjectAltName on \
     every TCPS session, which is stricter than a distinguished-name match rather than \
     weaker — so the parameter asks for nothing that is not already happening";

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> ServerCertificateRequest {
        ServerCertificateRequest::scan_connect_string(text)
    }

    /// The refusal a scan produces, or a panic naming what it should have been.
    fn refusal(found: ServerCertificateRequest) -> DbError {
        match found.guard(false) {
            Ok(warnings) => panic!("this descriptor should have been refused, got {warnings:?}"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_descriptor_without_either_parameter_is_left_alone() {
        for text in [
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.example.internal)(PORT=2484))\
             (CONNECT_DATA=(SERVICE_NAME=RELDEX)))",
            "tcps://localhost:2484/RELDEX",
            "127.0.0.1:1521/RELDEX",
            "",
        ] {
            let found = scan(text);
            assert!(found.is_empty(), "{text}: {found:?}");
            assert!(
                found.guard(false).expect("nothing to refuse").is_empty(),
                "{text}"
            );
        }
    }

    #[test]
    fn a_certificate_distinguished_name_is_refused_by_default() {
        let found = scan(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.example.internal)(PORT=2484))\
             (CONNECT_DATA=(SERVICE_NAME=RELDEX))\
             (SECURITY=(SSL_SERVER_CERT_DN=CN=db.example.internal,O=Example)))",
        );
        assert!(found.pins_certificate_dn, "{found:?}");
        let error = refusal(found);
        assert_eq!(error.kind(), ErrorKind::Configuration);
        // The message has to carry all three things a caller needs: what was
        // found, what is verified instead, and both ways forward.
        assert!(error.message().contains("SSL_SERVER_CERT_DN"), "{error}");
        assert!(error.message().contains("subjectAltName"), "{error}");
        assert!(
            error
                .message()
                .contains(EXT_ALLOW_UNENFORCED_SERVER_CERT_DN),
            "{error}"
        );
    }

    #[test]
    fn the_opt_in_extension_turns_the_refusal_into_a_warning() {
        let found = scan("(SECURITY=(SSL_SERVER_CERT_DN=\"CN=db,O=x\"))");
        assert!(found.pins_certificate_dn);
        let warnings = found.guard(true).expect("the opt-in allows the session");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind(), WarningKind::Informational);
        assert!(
            warnings[0].message().contains("without"),
            "the warning must say the pin was not applied: {:?}",
            warnings[0]
        );
    }

    #[test]
    fn a_distinguished_name_is_found_however_the_descriptor_spells_it() {
        // Case, whitespace, newlines, single and double quotes, and the nesting
        // a real descriptor uses. A miss here is a session opened with a
        // guarantee the profile asked for and did not get.
        for text in [
            "(SECURITY=(SSL_SERVER_CERT_DN=CN=db))",
            "(security=(ssl_server_cert_dn=cn=db))",
            "(Security=(Ssl_Server_Cert_Dn=CN=db))",
            "(SECURITY = ( SSL_SERVER_CERT_DN = CN=db ))",
            "(SECURITY=\n  (SSL_SERVER_CERT_DN\n   =\n   CN=db,O=Example))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"CN=db,O=Example, C=TH\"))",
            "(SECURITY=(SSL_SERVER_CERT_DN='CN=db,O=Example'))",
            "(SECURITY=(SSL_SERVER_DN_MATCH=ON)(SSL_SERVER_CERT_DN=CN=db))",
            "(DESCRIPTION_LIST=\
               (DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=a)(PORT=2484))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX)))\
               (DESCRIPTION=(ADDRESS_LIST=(ADDRESS=(PROTOCOL=TCPS)(HOST=b)(PORT=2484))\
                 (ADDRESS=(PROTOCOL=TCPS)(HOST=c)(PORT=2484)))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX))\
                 (SECURITY=(SSL_SERVER_CERT_DN=CN=c))))",
            // Easy Connect. Upstream's Easy Connect parser has no query-string
            // arm at all — it reads protocol, hosts, service name, server type
            // and instance name and silently discards the rest — so this
            // parameter would be dropped on the floor. Detecting it anyway is
            // the conservative direction: the caller is told the pin is not
            // applied instead of believing it is.
            "tcps://db:2484/RELDEX?ssl_server_cert_dn=CN%3Ddb",
        ] {
            assert!(scan(text).pins_certificate_dn, "missed in {text}");
        }
    }

    #[test]
    fn an_empty_distinguished_name_pins_nothing_and_is_not_refused() {
        for text in [
            "(SECURITY=(SSL_SERVER_CERT_DN=))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"\"))",
            "(SECURITY=(SSL_SERVER_CERT_DN =   ))",
            // No `=` at all is not a descriptor node in any grammar.
            "(SERVICE_NAME=SSL_SERVER_CERT_DN)",
        ] {
            assert!(!scan(text).pins_certificate_dn, "{text}");
        }
    }

    #[test]
    fn matching_switched_on_is_reported_and_never_refused() {
        for value in ["ON", "on", "Yes", "yes", "TRUE", "true", " on "] {
            let found = scan(&format!("(SECURITY=(SSL_SERVER_DN_MATCH={value}))"));
            assert!(found.asks_dn_match_on, "{value}: {found:?}");
            assert!(!found.asks_dn_match_off, "{value}: {found:?}");
            let warnings = found.guard(false).expect("never a refusal");
            assert_eq!(warnings.len(), 1, "{value}");
            assert!(
                warnings[0].message().contains("subjectAltName"),
                "{value}: {:?}",
                warnings[0]
            );
        }
    }

    #[test]
    fn matching_switched_off_is_reported_as_something_that_cannot_be_honoured() {
        // Upstream reads anything that is not `on`/`yes`/`true` as false, typos
        // included, so this driver has to read it the same way or it would warn
        // about a setting upstream never saw.
        for value in ["OFF", "off", "No", "no", "FALSE", "false", "0", "nope"] {
            let found = scan(&format!("(SECURITY=(SSL_SERVER_DN_MATCH={value}))"));
            assert!(found.asks_dn_match_off, "{value}: {found:?}");
            assert!(!found.asks_dn_match_on, "{value}: {found:?}");
            let warnings = found.guard(false).expect("never a refusal");
            assert_eq!(warnings.len(), 1, "{value}");
            assert!(
                warnings[0].message().contains("cannot switch"),
                "{value}: {:?}",
                warnings[0]
            );
        }
    }

    #[test]
    fn a_description_list_that_disagrees_with_itself_reports_both_halves() {
        // One `DESCRIPTION` asking for matching and another switching it off is
        // not a contradiction this driver has to resolve: neither is applied, so
        // both are reported.
        let found = scan(
            "(DESCRIPTION_LIST=\
               (DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=a)(PORT=2484))\
                 (SECURITY=(SSL_SERVER_DN_MATCH=ON)))\
               (DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=b)(PORT=2484))\
                 (SECURITY=(SSL_SERVER_DN_MATCH=OFF))))",
        );
        assert!(
            found.asks_dn_match_on && found.asks_dn_match_off,
            "{found:?}"
        );
        assert_eq!(found.guard(false).expect("never a refusal").len(), 2);
    }

    #[test]
    fn both_parameters_together_refuse_before_they_warn() {
        let found = scan("(SECURITY=(SSL_SERVER_DN_MATCH=OFF)(SSL_SERVER_CERT_DN=CN=db))");
        assert_eq!(refusal(found).kind(), ErrorKind::Configuration);
        // And with the opt-in, both are reported.
        assert_eq!(found.guard(true).expect("opted in").len(), 2);
    }

    #[test]
    fn the_key_inside_another_parameters_value_is_refused_rather_than_missed() {
        // A quote-aware scanner would skip this — and the same cleverness is
        // what loses a real occurrence to an unbalanced quote. Refusing a
        // descriptor that spells the key somewhere odd costs an edit; missing
        // one costs the guarantee.
        let found = scan("(CONNECT_DATA=(SERVICE_NAME=\"R(SSL_SERVER_CERT_DN=CN=x)\"))");
        assert!(found.pins_certificate_dn);
        assert_eq!(refusal(found).kind(), ErrorKind::Configuration);
    }

    #[test]
    fn upstreams_own_rendering_is_read_by_its_own_rules() {
        // `get_connect_descriptor` writes `(SSL_SERVER_DN_MATCH=ON)` into every
        // TCPS descriptor because the flag defaults to true, so reading its
        // presence as a request would warn on every ordinary TLS connection.
        let ordinary = "(description=(address=(protocol=tcps)(host=db)(port=2484))\
                        (connect_data=(service_name=reldex))\
                        (security=(ssl_server_dn_match=on)))";
        assert!(
            ServerCertificateRequest::scan_upstream_descriptor(ordinary).is_empty(),
            "the default flag must not be mistaken for a request"
        );

        // Its **absence** from an emitted SECURITY segment does mean the caller
        // set it to something upstream read as false — the case a `tnsnames.ora`
        // alias would otherwise hide, because the alias text never reaches this
        // driver.
        let switched_off = "(description=(address=(protocol=tcps)(host=db)(port=2484))\
                            (connect_data=(service_name=reldex))(security=))";
        let found = ServerCertificateRequest::scan_upstream_descriptor(switched_off);
        assert!(found.asks_dn_match_off, "{found:?}");
        assert!(!found.asks_dn_match_on, "{found:?}");

        // A plaintext descriptor has no SECURITY segment at all, so there is
        // nothing to infer.
        let plaintext = "(description=(address=(protocol=tcp)(host=db)(port=1521))\
                         (connect_data=(service_name=reldex)))";
        assert!(ServerCertificateRequest::scan_upstream_descriptor(plaintext).is_empty());

        // A pin upstream parsed is rendered only when there was one.
        let pinned = "(description=(address=(protocol=tcps)(host=db)(port=2484))\
                      (connect_data=(service_name=reldex))\
                      (security=(ssl_server_dn_match=on)(ssl_server_cert_dn=CN=db,O=x)))";
        assert!(ServerCertificateRequest::scan_upstream_descriptor(pinned).pins_certificate_dn);
    }

    /// A TCPS descriptor with the given `SECURITY` segment spliced in.
    fn descriptor_with(security: &str) -> String {
        format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db)(PORT=2484))\
             (CONNECT_DATA=(SERVICE_NAME=RELDEX)){security})"
        )
    }

    #[test]
    fn the_guard_never_sees_less_than_upstream_keeps() {
        // The test that should have existed from the start, and the one that
        // caught nothing because it did not: for every shape upstream
        // **accepts**, if upstream's own rebuilt descriptor still carries an
        // `SSL_SERVER_CERT_DN` then the guard must have found one too. Stricter
        // is fine; laxer is the silent downgrade the whole module exists to
        // stop. It is checked against `oracledb` itself rather than against a
        // second reading of its source, because the first version of this
        // scanner was written from that source and still disagreed with it
        // about four shapes — `'…'`, `''…''`, `""` and a quoted single space.
        // The four shapes the first version got wrong, asserted by name so the
        // regression cannot come back through a table that quietly skips them.
        for security in [
            "(SECURITY=(SSL_SERVER_CERT_DN=''CN=db,O=Acme''))",
            "(SECURITY=(SSL_SERVER_CERT_DN=''))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\" \"))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"\t\"))",
        ] {
            let text = descriptor_with(security);
            let rebuilt = oracledb::Config::default()
                .set_connect_string(&text)
                .expect("upstream accepts this shape")
                .get_connect_descriptor();
            assert!(
                rebuilt.to_ascii_lowercase().contains(CERT_DN_KEY),
                "this shape is only interesting while upstream keeps it: {rebuilt}"
            );
            assert!(scan(&text).pins_certificate_dn, "{text}");
        }

        let mut checked = 0_usize;
        for security in [
            "(SECURITY=(SSL_SERVER_CERT_DN=CN=db,O=Acme))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"CN=db,O=Acme\"))",
            // A single quote is not a delimiter to upstream, so these three are
            // ordinary non-empty bare values.
            "(SECURITY=(SSL_SERVER_CERT_DN='CN=db,O=Acme'))",
            "(SECURITY=(SSL_SERVER_CERT_DN=''CN=db,O=Acme''))",
            "(SECURITY=(SSL_SERVER_CERT_DN=''))",
            // A quoted value is stored untrimmed, so whitespace is a value.
            "(SECURITY=(SSL_SERVER_CERT_DN=\" \"))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"\t\"))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"\r\n\"))",
            // Bare values are trimmed, so these two really are empty — upstream
            // drops the node and the guard is allowed to see nothing.
            "(SECURITY=(SSL_SERVER_CERT_DN=))",
            "(SECURITY=(SSL_SERVER_CERT_DN=   ))",
            // Case, spacing and line breaks.
            "(security=(ssl_server_cert_dn=cn=db))",
            "(Security=(Ssl_Server_Cert_Dn = CN=db ))",
            "(SECURITY=\n  (SSL_SERVER_CERT_DN\n   =\n   CN=db))",
            // Alongside the other parameter, in both orders.
            "(SECURITY=(SSL_SERVER_DN_MATCH=ON)(SSL_SERVER_CERT_DN=CN=db))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\"CN=db\")(SSL_SERVER_DN_MATCH=OFF))",
        ] {
            let text = descriptor_with(security);
            let Ok(config) = oracledb::Config::default().set_connect_string(&text) else {
                // Upstream refuses the shape outright, so no session can open
                // from it and there is nothing for the guard to be laxer than.
                continue;
            };
            let rebuilt = config.get_connect_descriptor();
            if !rebuilt.to_ascii_lowercase().contains(CERT_DN_KEY) {
                continue;
            }
            checked += 1;
            assert!(
                scan(&text).pins_certificate_dn,
                "upstream kept a pin out of {text} and the guard did not see it, so the \
                 session would have opened without the pin and without a word. Rebuilt \
                 descriptor: {rebuilt}"
            );
        }
        // Without this the table could stop proving anything — every row
        // skipped by one of the two `continue`s above still leaves a green
        // test.
        assert!(
            checked >= 12,
            "only {checked} of the shapes reached the assertion; the table has gone vacuous"
        );
    }

    #[test]
    fn an_easy_connect_plus_query_string_is_read_the_way_it_was_written() {
        // `&` and `;` separate Easy Connect Plus parameters. Without them in the
        // terminator set the value of a `ssl_server_dn_match=on` followed by a
        // second parameter read as `on&retry_count=3`, which is not `on`, and a
        // user who had switched matching **on** was told it could not be
        // switched off.
        for text in [
            "tcps://db:2484/RELDEX?ssl_server_dn_match=on&retry_count=3",
            "tcps://db:2484/RELDEX?ssl_server_dn_match=ON;retry_count=3",
            "tcps://db:2484/RELDEX?retry_count=3&ssl_server_dn_match=yes",
        ] {
            let found = scan(text);
            assert!(found.asks_dn_match_on, "{text}: {found:?}");
            assert!(!found.asks_dn_match_off, "{text}: {found:?}");
        }
        for text in [
            "tcps://db:2484/RELDEX?ssl_server_dn_match=off&retry_count=3",
            "tcps://db:2484/RELDEX?ssl_server_dn_match=no;retry_count=3",
        ] {
            let found = scan(text);
            assert!(found.asks_dn_match_off, "{text}: {found:?}");
            assert!(!found.asks_dn_match_on, "{text}: {found:?}");
        }
        // A pin in the same position is still a pin. The terminator set for it
        // is narrower on purpose, so a DN that contains `&` keeps it.
        let found = scan("tcps://db:2484/RELDEX?ssl_server_cert_dn=CN=a&b&retry_count=3");
        assert!(found.pins_certificate_dn, "{found:?}");
    }

    #[test]
    fn a_value_that_upstream_reads_as_empty_is_read_as_empty_here_too() {
        // The other direction of `the_guard_never_sees_less_than_upstream_keeps`:
        // the guard may be stricter, but gratuitous strictness is a refusal
        // nobody can act on, so the shapes upstream really does drop are
        // asserted as dropped.
        for security in [
            "(SECURITY=(SSL_SERVER_CERT_DN=))",
            "(SECURITY=(SSL_SERVER_CERT_DN=   ))",
            "(SECURITY=(SSL_SERVER_CERT_DN=\n))",
        ] {
            let text = descriptor_with(security);
            assert!(!scan(&text).pins_certificate_dn, "{text}");
            if let Ok(config) = oracledb::Config::default().set_connect_string(&text) {
                assert!(
                    !config
                        .get_connect_descriptor()
                        .to_ascii_lowercase()
                        .contains(CERT_DN_KEY),
                    "upstream kept {text} after all, so the guard must stop dropping it"
                );
            }
        }
        // `SSL_SERVER_DN_MATCH` with no value is skipped by upstream's
        // `process_child_nodes`, so warning about it would be warning about
        // something nobody set.
        assert!(
            scan(&descriptor_with("(SECURITY=(SSL_SERVER_DN_MATCH=))")).is_empty(),
            "an empty flag is not a request"
        );
    }

    #[test]
    fn a_tcps_alias_is_seen_through_upstream_and_a_plaintext_alias_is_not() {
        // Pins the alias path **and** the limitation the module documents under
        // "The one shape this cannot see". The second assertion is deliberately
        // an assertion about a weakness: if a future `oracledb` renders the
        // SECURITY segment for plaintext descriptors too, this test fails and
        // the limitation can be deleted rather than quietly outliving its
        // cause.
        let entry = |protocol: &str, port: u16| {
            format!(
                "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST=db)(PORT={port}))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX))\
                 (SECURITY=(SSL_SERVER_CERT_DN=CN=db,O=Acme)))"
            )
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let directory =
            std::env::temp_dir().join(format!("reldex-tnsnames-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a temporary directory");
        std::fs::write(
            directory.join("tnsnames.ora"),
            format!(
                "RELDEX_TCPS = {}\nRELDEX_TCP = {}\n",
                entry("TCPS", 2484),
                entry("TCP", 1521)
            ),
        )
        .expect("the temporary tnsnames.ora");

        let describe = |alias: &str| {
            oracledb::Config::default()
                .set_config_dir(directory.to_str().expect("a UTF-8 temporary path"))
                .set_connect_string(alias)
                .map(|config| config.get_connect_descriptor())
        };
        let tcps = describe("RELDEX_TCPS");
        let tcp = describe("RELDEX_TCP");
        let _ = std::fs::remove_dir_all(&directory);

        let tcps = tcps.expect("the TCPS alias should resolve");
        let tcp = tcp.expect("the TCP alias should resolve");

        // The alias name itself carries nothing, which is exactly why the
        // second source exists.
        assert!(scan("RELDEX_TCPS").is_empty());
        assert!(
            ServerCertificateRequest::scan_upstream_descriptor(&tcps).pins_certificate_dn,
            "a pin reached through a TCPS alias must still be found: {tcps}"
        );

        assert!(
            !tcp.to_ascii_lowercase().contains(CERT_DN_KEY),
            "upstream now renders SECURITY for a plaintext descriptor; the documented \
             limitation is gone and the module doc should be updated: {tcp}"
        );
        assert!(
            ServerCertificateRequest::scan_upstream_descriptor(&tcp).is_empty(),
            "known limitation: a plaintext alias hides its SECURITY segment"
        );
    }

    #[test]
    fn findings_from_the_two_sources_are_unioned() {
        let from_text = scan("(SECURITY=(SSL_SERVER_DN_MATCH=ON))");
        let from_upstream = ServerCertificateRequest::scan_upstream_descriptor(
            "(description=(address=(protocol=tcps)(host=db)(port=2484))\
             (security=(ssl_server_dn_match=on)(ssl_server_cert_dn=CN=db)))",
        );
        let merged = from_text.merged_with(from_upstream);
        assert!(
            merged.asks_dn_match_on && merged.pins_certificate_dn,
            "{merged:?}"
        );
        assert!(ServerCertificateRequest::default().is_empty());
    }
}
