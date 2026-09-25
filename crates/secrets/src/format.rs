//! How a password is laid out inside a platform store entry (ADR-0007 S8).
//!
//! ```text
//! offset 0     4        5
//!        +-----+--------+---------------------------------------------+
//!        | RLDX| 0x01   | password, UTF-8, no control character       |
//!        +-----+--------+---------------------------------------------+
//!         magic version  payload
//! ```
//!
//! The prefix is how Reldex recognises an entry as its own. An entry created
//! under Reldex's name by anything else — `cmdkey /generic:… /pass:…` stores
//! the password as UTF-16, which is often valid UTF-8 with NULs or other
//! control bytes in it — must never be sent to a database as the password: a
//! wrong password counts towards the account's failed-login limit and can
//! lock it. Such an entry decodes as [`CredentialError::Malformed`], and
//! `resolve_password` asks the user instead.
//!
//! A payload with a control character (U+0000–U+001F) is refused both ways:
//! on write with [`CredentialError::InvalidSecret`], on read as
//! [`CredentialError::Malformed`]. No database password Reldex has to support
//! needs one, and refusing them is what lets the reader tell a UTF-16 entry
//! from a Reldex one even if its first five bytes were to match. A later
//! format is a new version byte; this build reads version 1 only.
//!
//! Platform-neutral so it is tested everywhere and a later backend (Apple
//! Keychain, Android Keystore) can share it; today only the Windows backend
//! (and the in-memory test store's `check_storable`) uses it.

#![cfg_attr(
    not(windows),
    allow(dead_code, reason = "only the Windows backend stores entries today")
)]

use reldex_db_driver_api::Secret;
use zeroize::{Zeroize, Zeroizing};

use crate::store::CredentialError;

/// The four magic bytes every Reldex entry starts with.
const MAGIC: [u8; 4] = *b"RLDX";

/// The format version this build writes and reads.
const VERSION: u8 = 1;

/// Bytes the prefix takes from a backend's blob limit.
pub(crate) const PREFIX_LEN: usize = MAGIC.len() + 1;

/// Whether any store could keep `secret` faithfully: refuses a control
/// character, which the reader would reject.
pub(crate) fn check_storable(secret: &Secret) -> Result<(), CredentialError> {
    if secret.expose().bytes().any(is_control) {
        return Err(CredentialError::InvalidSecret);
    }
    Ok(())
}

/// The entry's bytes for `secret`: prefix, then the password's UTF-8 bytes,
/// in a buffer of exactly the right size that is wiped when dropped.
pub(crate) fn encode(secret: &Secret) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
    check_storable(secret)?;
    let payload = secret.expose().as_bytes();
    let mut blob = Zeroizing::new(Vec::with_capacity(PREFIX_LEN + payload.len()));
    blob.extend_from_slice(&MAGIC);
    blob.push(VERSION);
    blob.extend_from_slice(payload);
    Ok(blob)
}

/// Reads an entry's bytes back into a [`Secret`], or
/// [`CredentialError::Malformed`] when they are not a version-1 Reldex entry
/// holding control-free UTF-8. The buffer becomes the secret's own storage
/// (no copy) or is wiped before it is dropped.
pub(crate) fn decode(mut blob: Vec<u8>) -> Result<Secret, CredentialError> {
    let recognised = blob.len() >= PREFIX_LEN
        && blob[..MAGIC.len()] == MAGIC
        && blob[MAGIC.len()] == VERSION
        && !blob[PREFIX_LEN..].iter().copied().any(is_control);
    if !recognised {
        blob.zeroize();
        return Err(CredentialError::Malformed);
    }
    // Shifts the payload down inside the same buffer; the stale tail stays
    // within the capacity, which `Secret` wipes in full when it is dropped.
    blob.drain(..PREFIX_LEN);
    match String::from_utf8(blob) {
        Ok(text) => Ok(Secret::new(text)),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            Err(CredentialError::Malformed)
        }
    }
}

/// C0 control characters, NUL included. In UTF-8 these bytes never occur
/// inside a multi-byte sequence, so a byte test is a character test.
const fn is_control(byte: u8) -> bool {
    byte < 0x20
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn a_v1_entry_round_trips_byte_exact() {
        for password in ["", "correct horse", "รหัสผ่าน-ทดสอบ-🔑", &"x".repeat(2555)]
        {
            let encoded = encode(&Secret::new(password)).expect("storable");
            assert_eq!(&encoded[..5], b"RLDX\x01");
            assert_eq!(&encoded[5..], password.as_bytes());
            assert_eq!(encoded.capacity(), encoded.len(), "no spare copy");
            let decoded = decode(encoded.to_vec()).expect("a v1 entry");
            assert_eq!(decoded.expose(), password);
        }
    }

    #[test]
    fn what_cmdkey_writes_is_not_a_reldex_entry() {
        // `cmdkey /generic:… /pass:Hunter2x` stores UTF-16LE: valid UTF-8
        // with NULs in it, and no prefix.
        let ascii: Vec<u8> = "Hunter2x"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(
            std::str::from_utf8(&ascii).is_ok(),
            "the old check passed it"
        );
        assert_eq!(decode(ascii).map(|_| ()), Err(CredentialError::Malformed));
        // Thai "กข" as UTF-16LE is 01 0E 02 0E: valid UTF-8, no NUL.
        let thai: Vec<u8> = "กข".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(thai, [0x01, 0x0E, 0x02, 0x0E]);
        assert_eq!(decode(thai).map(|_| ()), Err(CredentialError::Malformed));
    }

    #[test]
    fn anything_but_a_v1_prefix_with_clean_text_is_malformed() {
        for bad in [
            blob(&[]),
            blob(&[b"RLDX"]),
            blob(&[b"RLDY\x01", b"pw"]),
            blob(&[b"rldx\x01", b"pw"]),
            // A future version this build does not read.
            blob(&[b"RLDX\x02", b"pw"]),
            blob(&[b"RLDX\x00", b"pw"]),
            // A version-1 prefix around a payload a Reldex writer never makes.
            blob(&[b"RLDX\x01", b"pw\x00"]),
            blob(&[b"RLDX\x01", b"p\tw"]),
            blob(&[b"RLDX\x01", &[0xFF, 0xFE]]),
            blob(&[
                b"RLDX\x01",
                &"ab"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<u8>>(),
            ]),
        ] {
            assert_eq!(
                decode(bad.clone()).map(|_| ()),
                Err(CredentialError::Malformed),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_password_with_a_control_character_is_not_stored() {
        for bad in ["a\u{0}b", "tab\there", "line\nbreak", "\u{1f}"] {
            assert_eq!(
                check_storable(&Secret::new(bad)),
                Err(CredentialError::InvalidSecret)
            );
            assert_eq!(
                encode(&Secret::new(bad)).map(|_| ()),
                Err(CredentialError::InvalidSecret)
            );
        }
        // DEL and non-ASCII are ordinary characters.
        assert_eq!(check_storable(&Secret::new("del\u{7f}ก🔑")), Ok(()));
    }
}
