//! Time-based one-time passwords (RFC 6238) and the tokens that stand beside them.
//!
//! Three secrets live here, and they differ in what protects them at rest:
//!
//! * the **TOTP shared secret** — stored as it is. It has to be, because verifying a
//!   code means recomputing it; the database is the boundary, which is the same
//!   boundary [`crate::password`] already trusts for the address book and the
//!   message store.
//! * a **recovery code** — high-entropy and single-use, so it is stored as a
//!   SHA-256 digest. There is nothing to guess: the code space is large enough that
//!   a fast hash is not a weakened one, and Argon2 on every attempt would only make
//!   a brute-force attack cheaper than the honest path.
//! * an **application password** — same reasoning, and it is the credential IMAP
//!   and SMTP clients use when they cannot be asked for a TOTP code.
//!
//! # Why base32 is implemented here
//!
//! `otpauth://` mandates base32 without padding, and the workspace has no base32
//! crate. Pulling one in for forty lines would add a dependency to `ferroma-auth`
//! for every consumer of the platform; the RFC 4648 alphabet is small enough to
//! test properly, which is what the tests below do.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use ferroma_core::{FerromaError, Result};

type HmacSha1 = Hmac<Sha1>;

/// Digits in a generated code. RFC 6238 §5.3 allows 6 or 8; authenticators assume 6.
pub const CODE_DIGITS: u32 = 6;

/// Seconds per time step (RFC 6238 §5.2 default).
pub const TIME_STEP_SECS: u64 = 30;

/// Bytes of entropy in a new shared secret. RFC 4226 §4 recommends 160 bits, which
/// is also the HMAC-SHA1 block-friendly size.
pub const SECRET_BYTES: usize = 20;

/// How many steps either side of the current one are accepted.
///
/// One step covers ordinary clock drift in both directions (a client a few seconds
/// ahead or behind). Widening it multiplies an attacker's window, so it stays at one.
pub const DEFAULT_SKEW_STEPS: i64 = 1;

/// The RFC 4648 base32 alphabet, upper-case and without padding.
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Encode bytes as upper-case base32 without padding.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    for chunk in data.chunks(5) {
        let mut buffer = [0u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from(buffer[0]) << 32
            | u64::from(buffer[1]) << 24
            | u64::from(buffer[2]) << 16
            | u64::from(buffer[3]) << 8
            | u64::from(buffer[4]);
        // Five input bytes become eight output characters; only the ones that carry
        // real bits are emitted, which is what "without padding" means.
        let characters = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for index in 0..characters {
            let shift = 35 - index * 5;
            out.push(BASE32_ALPHABET[((bits >> shift) & 0x1F) as usize] as char);
        }
    }
    out
}

/// Decode base32, tolerating lower case, padding and embedded spaces.
pub fn base32_decode(raw: &str) -> Result<Vec<u8>> {
    let cleaned: Vec<u8> = raw
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() * 5 / 8);
    let mut bits: u64 = 0;
    let mut bit_count: u32 = 0;
    for byte in cleaned {
        let value = match byte.to_ascii_uppercase() {
            b'A'..=b'Z' => byte.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => byte.to_ascii_uppercase() - b'2' + 26,
            other => {
                return Err(FerromaError::Invalid(format!(
                    "not a base32 character: {:?}",
                    other as char
                )))
            }
        };
        bits = bits << 5 | u64::from(value);
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Ok(out)
}

/// A fresh shared secret, base32-encoded for storage and for the `otpauth://` URI.
pub fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    base32_encode(&bytes)
}

/// The counter for an instant: how many time steps have elapsed since the epoch.
pub fn counter_at(now: std::time::SystemTime, step_secs: u64) -> u64 {
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    seconds / step_secs.max(1)
}

/// The RFC 4226 §5.3 truncation of one HMAC-SHA1 counter.
fn hotp(secret: &[u8], counter: u64) -> Result<u32> {
    let mut mac = HmacSha1::new_from_slice(secret)
        .map_err(|_| FerromaError::internal("HMAC rejected the TOTP secret length"))?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    // Dynamic truncation: the low nibble of the last byte picks the offset.
    let offset = usize::from(digest[19] & 0x0F);
    let slice = &digest[offset..offset + 4];
    let value = u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]) & 0x7FFF_FFFF;
    Ok(value)
}

/// The code for a counter, zero-padded to [`CODE_DIGITS`].
pub fn code_for_counter(secret_base32: &str, counter: u64) -> Result<String> {
    let secret = base32_decode(secret_base32)?;
    if secret.is_empty() {
        return Err(FerromaError::Invalid("the TOTP secret is empty".into()));
    }
    let value = hotp(&secret, counter)?;
    let modulus = 10u32.pow(CODE_DIGITS);
    Ok(format!("{:0width$}", value % modulus, width = CODE_DIGITS as usize))
}

/// The code a well-behaved authenticator shows right now.
pub fn current_code(secret_base32: &str, now: std::time::SystemTime) -> Result<String> {
    code_for_counter(secret_base32, counter_at(now, TIME_STEP_SECS))
}

/// Whether `code` is valid for `secret` around `now`.
///
/// The comparison is constant time: a code is a short secret, and an early-exit
/// comparison leaks how much of a guess was right.
pub fn verify_code(
    secret_base32: &str,
    code: &str,
    now: std::time::SystemTime,
    skew_steps: i64,
) -> Result<bool> {
    let candidate = code.trim().replace(' ', "");
    if candidate.len() != CODE_DIGITS as usize || !candidate.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(false);
    }
    let counter = counter_at(now, TIME_STEP_SECS);
    for offset in -skew_steps..=skew_steps {
        let Some(shifted) = counter.checked_add_signed(offset) else {
            continue;
        };
        let value = code_for_counter(secret_base32, shifted)?;
        // `subtle` makes the comparison constant time: a code is a short secret, and
        // an early exit would leak how much of a guess was right.
        if value.as_bytes().ct_eq(candidate.as_bytes()).into() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The `otpauth://totp/...` URI an authenticator app scans.
///
/// The label is `issuer:account` as Google Authenticator expects, and the query
/// carries the parameters RFC 6238 clients read. `digits` and `period` are stated
/// explicitly rather than left to a default.
pub fn otpauth_uri(issuer: &str, account: &str, secret_base32: &str) -> String {
    let label = format!(
        "{}:{}",
        percent_encode(issuer.trim()),
        percent_encode(account.trim())
    );
    format!(
        "otpauth://totp/{label}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits={CODE_DIGITS}&period={TIME_STEP_SECS}",
        secret = secret_base32,
        issuer = percent_encode(issuer.trim()),
    )
}

/// Percent-encode everything outside the unreserved set, which is what a URI label
/// needs and what `url`'s query encoder would not do for a path segment.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Bytes of entropy behind one recovery code.
const RECOVERY_BYTES: usize = 10;

/// How many recovery codes an enrollment issues.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// A freshly minted recovery code, in a form a person can type back.
///
/// Base32 upper-case groups of four, which is unambiguous when read aloud and
/// cannot be confused by letter case.
pub fn generate_recovery_code() -> String {
    let mut bytes = [0u8; RECOVERY_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let encoded = base32_encode(&bytes);
    encoded
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalise a recovery code before hashing, so spacing and case cannot make a
/// correct code fail.
pub fn normalise_recovery_code(raw: &str) -> String {
    raw.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(|character| character.to_uppercase())
        .collect()
}

/// The stored form of a recovery code.
pub fn hash_recovery_code(code: &str) -> String {
    let normalised = normalise_recovery_code(code);
    hex::encode(Sha256::digest(normalised.as_bytes()))
}

/// Bytes of entropy behind one application password.
const APP_PASSWORD_BYTES: usize = 24;

/// A new application password, returned to the user exactly once.
pub fn generate_app_password() -> String {
    let mut bytes = [0u8; APP_PASSWORD_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    format!(
        "ap_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// The stored form of an application password. Same reasoning as a recovery code:
/// the plaintext is high-entropy, so a digest is enough and a slow hash would only
/// slow the honest path down.
pub fn hash_app_password(token: &str) -> String {
    hex::encode(Sha256::digest(token.trim().as_bytes()))
}

/// Whether a string looks like an application password rather than an account
/// password, so a protocol can pick the cheaper lookup first.
pub fn is_app_password(raw: &str) -> bool {
    raw.trim().starts_with("ap_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// RFC 4648 §10 test vectors.
    #[test]
    fn base32_matches_the_rfc_vectors() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(raw.as_bytes()), encoded, "{raw}");
            assert_eq!(base32_decode(encoded).unwrap(), raw.as_bytes(), "{encoded}");
        }
    }

    #[test]
    fn base32_decoding_tolerates_case_padding_and_spaces() {
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(base32_decode("MZXW6YTBOI======").unwrap(), b"foobar");
        assert_eq!(base32_decode(" MZXW 6YTB OI ").unwrap(), b"foobar");
        assert!(base32_decode("MZXW6YTB1").is_err(), "1 is not in the alphabet");
        assert!(base32_decode("MZXW6YTB-").is_err());
    }

    /// RFC 6238 Appendix B: the SHA-1 rows, with the 8-digit values truncated to
    /// the 6 digits this implementation issues.
    #[test]
    fn totp_matches_the_rfc_vectors() {
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        for (seconds, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            let code = current_code(secret, UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
            assert_eq!(code, expected, "at {seconds}s");
        }
    }

    #[test]
    fn a_code_is_the_counter_truncated_to_six_digits() {
        let secret = generate_secret();
        let counter = counter_at(UNIX_EPOCH + Duration::from_secs(1_234_567_890), TIME_STEP_SECS);
        assert_eq!(counter, 41_152_263);
        let code = code_for_counter(&secret, counter).unwrap();
        assert_eq!(code.len(), CODE_DIGITS as usize);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn verification_accepts_one_step_of_clock_drift_in_both_directions() {
        let secret = generate_secret();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let current = current_code(&secret, now).unwrap();
        assert!(verify_code(&secret, &current, now, DEFAULT_SKEW_STEPS).unwrap());

        let ahead = current_code(&secret, now + Duration::from_secs(TIME_STEP_SECS)).unwrap();
        assert!(verify_code(&secret, &ahead, now, DEFAULT_SKEW_STEPS).unwrap());
        let behind = current_code(&secret, now - Duration::from_secs(TIME_STEP_SECS)).unwrap();
        assert!(verify_code(&secret, &behind, now, DEFAULT_SKEW_STEPS).unwrap());

        // Two steps away is outside the window even with the default skew.
        let too_far = current_code(&secret, now + Duration::from_secs(TIME_STEP_SECS * 2)).unwrap();
        assert!(!verify_code(&secret, &too_far, now, DEFAULT_SKEW_STEPS).unwrap());
    }

    #[test]
    fn verification_refuses_malformed_input_without_erroring() {
        let secret = generate_secret();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for candidate in ["", "12345", "1234567", "abcdef", "12 34 5x", "------"] {
            assert!(
                !verify_code(&secret, candidate, now, DEFAULT_SKEW_STEPS).unwrap(),
                "{candidate:?}"
            );
        }
        // A space inside is the one tolerance: a person reading a code off a phone
        // may type it in two groups.
        let current = current_code(&secret, now).unwrap();
        let spaced = format!("{} {}", &current[..3], &current[3..]);
        assert!(verify_code(&secret, &spaced, now, DEFAULT_SKEW_STEPS).unwrap());
    }

    #[test]
    fn a_code_from_another_secret_never_matches() {
        let mine = generate_secret();
        let theirs = generate_secret();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let their_code = current_code(&theirs, now).unwrap();
        assert!(!verify_code(&mine, &their_code, now, DEFAULT_SKEW_STEPS).unwrap());
    }

    #[test]
    fn a_fresh_secret_is_the_documented_size_and_reusable() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 32, "20 bytes is 32 base32 characters");
        assert_eq!(base32_decode(&secret).unwrap().len(), SECRET_BYTES);
        let again = generate_secret();
        assert_ne!(secret, again, "two enrollments must not share a secret");
    }

    #[test]
    fn the_uri_names_the_issuer_account_and_parameters() {
        let uri = otpauth_uri("Ferroma", "alice@example.com", "MZXW6YTBOI");
        assert!(uri.starts_with("otpauth://totp/Ferroma:alice%40example.com?"), "{uri}");
        assert!(uri.contains("secret=MZXW6YTBOI"), "{uri}");
        assert!(uri.contains("issuer=Ferroma"), "{uri}");
        assert!(uri.contains("algorithm=SHA1"), "{uri}");
        assert!(uri.contains("digits=6"), "{uri}");
        assert!(uri.contains("period=30"), "{uri}");
    }

    #[test]
    fn the_uri_encodes_an_issuer_that_needs_it() {
        let uri = otpauth_uri("My Server", "alice@example.com", "MZXW6YTBOI");
        assert!(uri.contains("My%20Server:alice%40example.com"), "{uri}");
        assert!(uri.contains("issuer=My%20Server"), "{uri}");
    }

    #[test]
    fn recovery_codes_are_grouped_distinguishable_and_stable_to_hash() {
        let code = generate_recovery_code();
        let groups: Vec<&str> = code.split('-').collect();
        assert!(groups.len() >= 4, "{code}");
        assert!(groups.iter().all(|group| group.len() <= 4), "{code}");
        assert_ne!(code, generate_recovery_code());

        // The hash is case- and separator-insensitive, so a person cannot fail by
        // typing the code back in lower case or without the dashes.
        assert_eq!(hash_recovery_code(&code), hash_recovery_code(&code.to_lowercase()));
        assert_eq!(
            hash_recovery_code(&code),
            hash_recovery_code(&code.replace('-', ""))
        );
        assert_ne!(hash_recovery_code(&code), hash_recovery_code(&generate_recovery_code()));
    }

    #[test]
    fn app_passwords_are_prefixed_high_entropy_and_hashed_bare() {
        let token = generate_app_password();
        assert!(token.starts_with("ap_"), "{token}");
        assert!(token.len() > 30, "{token}");
        assert_ne!(token, generate_app_password());
        assert!(is_app_password(&token));
        assert!(!is_app_password("correct horse battery staple"));
        assert_eq!(hash_app_password(&token), hash_app_password(&format!(" {token} ")));
        assert_ne!(hash_app_password(&token), hash_app_password(&generate_app_password()));
    }
}
