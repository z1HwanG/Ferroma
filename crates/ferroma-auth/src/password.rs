//! Password hashing and policy.
//!
//! Ferroma stores Argon2id PHC strings — the same format `argon2` and every other
//! modern implementation produces, so a database can be moved between Ferroma
//! versions (and read by `ferroma` CLI tooling) without a rehash.
//!
//! ```text
//! $argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>
//! ```
//!
//! The parameters follow OWASP's 2024 recommendation for Argon2id (19 MiB, 2
//! iterations, 1 lane) rather than the crate's defaults, and they are recorded in
//! the hash itself — so raising them later does not invalidate existing passwords.
//! [`PasswordHasher::needs_rehash`] tells the login path when to transparently
//! upgrade a stored hash.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use ferroma_core::{FerromaError, Result};

/// Minimum accepted password length.
pub const MIN_PASSWORD_LENGTH: usize = 8;
/// Maximum accepted length. Argon2 itself has no limit; this bounds request size.
pub const MAX_PASSWORD_LENGTH: usize = 1024;

/// Argon2id parameters, in the shape the PHC string records them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory cost in kibibytes.
    pub memory_kib: u32,
    /// Time cost (iterations).
    pub iterations: u32,
    /// Parallelism (lanes).
    pub parallelism: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        // OWASP 2024: m=19456 KiB (19 MiB), t=2, p=1.
        Argon2Params {
            memory_kib: 19_456,
            iterations: 2,
            parallelism: 1,
        }
    }
}

impl Argon2Params {
    /// Cheap parameters for tests. Never use these to protect a real account.
    pub fn fast_for_tests() -> Self {
        Argon2Params {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        }
    }

    fn to_argon2(self) -> Result<Argon2<'static>> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, None)
            .map_err(|e| FerromaError::Config(format!("invalid Argon2 parameters: {e}")))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

/// Hashes and verifies passwords.
#[derive(Debug, Clone, Copy)]
pub struct PasswordHasher {
    params: Argon2Params,
}

impl Default for PasswordHasher {
    fn default() -> Self {
        PasswordHasher::new(Argon2Params::default())
    }
}

impl PasswordHasher {
    /// Build a hasher with explicit parameters.
    pub fn new(params: Argon2Params) -> Self {
        PasswordHasher { params }
    }

    /// A hasher with deliberately weak parameters, for tests only.
    pub fn fast_for_tests() -> Self {
        PasswordHasher::new(Argon2Params::fast_for_tests())
    }

    /// Hash a password into a PHC string.
    pub fn hash(&self, password: &str) -> Result<String> {
        validate_password(password)?;
        let salt = SaltString::generate(&mut OsRng);
        let hash = self
            .params
            .to_argon2()?
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| FerromaError::Internal(format!("password hashing failed: {e}")))?;
        Ok(hash.to_string())
    }

    /// Verify a password against a stored PHC string.
    ///
    /// Returns `Ok(false)` for a wrong password and for a hash that cannot be
    /// parsed — the two are deliberately indistinguishable to the caller, so a
    /// corrupted row cannot be probed for information. Verification is
    /// constant-time with respect to the hash.
    pub fn verify(&self, password: &str, stored: &str) -> bool {
        match PasswordHash::new(stored) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Whether `stored` was produced with weaker parameters than we now use.
    ///
    /// The login path calls this after a successful verification and silently
    /// re-hashes, which upgrades the whole database over time without a migration.
    pub fn needs_rehash(&self, stored: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(stored) else {
            return true;
        };
        let params = parsed.params;
        let current = self.params;

        let read = |name: &str, fallback: u32| -> u32 {
            params
                .get(name)
                .and_then(|v| v.decimal().ok())
                .unwrap_or(fallback)
        };

        // Argon2id only; anything else (argon2i, argon2d) is upgraded.
        let algorithm_ok = parsed.algorithm.as_str() == "argon2id";
        let m = read("m", 0);
        let t = read("t", 0);
        let p = read("p", 0);

        !algorithm_ok
            || m < current.memory_kib
            || t < current.iterations
            || p != current.parallelism
    }

    /// The parameters recorded inside a stored hash, when it parses.
    pub fn stored_params(stored: &str) -> Option<Argon2Params> {
        let parsed = PasswordHash::new(stored).ok()?;
        let read = |name: &str| parsed.params.get(name).and_then(|v| v.decimal().ok());
        Some(Argon2Params {
            memory_kib: read("m")?,
            iterations: read("t")?,
            parallelism: read("p")?,
        })
    }
}

/// Reject passwords that would be a liability, with a message worth showing a user.
pub fn validate_password(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_LENGTH {
        return Err(FerromaError::Invalid(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_LENGTH {
        return Err(FerromaError::Invalid(format!(
            "password must be at most {MAX_PASSWORD_LENGTH} characters"
        )));
    }
    if password.chars().all(|c| c.is_whitespace()) {
        return Err(FerromaError::Invalid("password must not be blank".into()));
    }
    // A password made entirely of one character survives length checks but falls
    // instantly to a dictionary attack.
    let first = password.chars().next().unwrap_or(' ');
    if password.chars().all(|c| c == first) {
        return Err(FerromaError::Invalid(
            "password must not repeat a single character".into(),
        ));
    }
    Ok(())
}

/// A rough strength estimate, used by the Admin UI to warn on weak passwords.
///
/// Deliberately simple and dependency-free: length dominates, character-class
/// variety adds a little. Returns 0–4 (very weak … strong).
pub fn strength(password: &str) -> u8 {
    let mut classes = 0;
    if password.chars().any(|c| c.is_ascii_lowercase()) {
        classes += 1;
    }
    if password.chars().any(|c| c.is_ascii_uppercase()) {
        classes += 1;
    }
    if password.chars().any(|c| c.is_ascii_digit()) {
        classes += 1;
    }
    if password.chars().any(|c| !c.is_alphanumeric()) {
        classes += 1;
    }
    let len = password.chars().count();
    let mut score = 0;
    if len >= MIN_PASSWORD_LENGTH {
        score += 1;
    }
    if len >= 12 {
        score += 1;
    }
    if len >= 16 {
        score += 1;
    }
    if classes >= 3 {
        score += 1;
    }
    score.min(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hasher() -> PasswordHasher {
        PasswordHasher::fast_for_tests()
    }

    #[test]
    fn hashes_and_verifies() {
        let h = hasher();
        let phc = h.hash("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(h.verify("correct horse battery", &phc));
        assert!(!h.verify("wrong horse battery", &phc));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let h = hasher();
        let a = h.hash("correct horse battery").unwrap();
        let b = h.hash("correct horse battery").unwrap();
        assert_ne!(a, b, "a per-hash salt is what makes rainbow tables useless");
        assert!(h.verify("correct horse battery", &a));
        assert!(h.verify("correct horse battery", &b));
    }

    #[test]
    fn a_corrupt_hash_verifies_as_false_rather_than_erroring() {
        let h = hasher();
        assert!(!h.verify("anything", "not-a-phc-string"));
        assert!(!h.verify("anything", ""));
        assert!(!h.verify("anything", "$argon2id$broken"));
    }

    #[test]
    fn stored_parameters_round_trip() {
        let params = Argon2Params {
            memory_kib: 4096,
            iterations: 3,
            parallelism: 2,
        };
        let phc = PasswordHasher::new(params).hash("a long enough password").unwrap();
        let read = PasswordHasher::stored_params(&phc).unwrap();
        assert_eq!(read, params);
    }

    #[test]
    fn needs_rehash_detects_weaker_stored_parameters() {
        let weak = PasswordHasher::new(Argon2Params {
            memory_kib: 512,
            iterations: 1,
            parallelism: 1,
        });
        let phc = weak.hash("a long enough password").unwrap();

        let strong = PasswordHasher::default();
        assert!(strong.needs_rehash(&phc), "512 KiB is weaker than 19 MiB");

        let current = hasher();
        let phc2 = current.hash("a long enough password").unwrap();
        assert!(!current.needs_rehash(&phc2));
    }

    #[test]
    fn needs_rehash_treats_garbage_as_stale() {
        assert!(hasher().needs_rehash("nonsense"));
    }

    #[test]
    fn validate_password_enforces_the_policy() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("        ").is_err());
        assert!(validate_password("aaaaaaaaaaaa").is_err(), "single repeated character");
        assert!(validate_password(&"a".repeat(MAX_PASSWORD_LENGTH + 1)).is_err());
        assert!(validate_password("correct horse battery").is_ok());
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password("密码密码密码密码").is_ok(), "unicode counts by bytes/chars, not ASCII");
    }

    #[test]
    fn strength_grows_with_length_and_variety() {
        assert_eq!(strength("short"), 0);
        assert!(strength("abcdefghijkl") < strength("Abcdefgh1jkl!"));
        assert!(strength("Abcdefgh1jkl!") <= 4);
        assert_eq!(strength("Abcdefgh1jkl!mnop"), 4);
    }

    #[test]
    fn default_parameters_match_the_owasp_recommendation() {
        let p = Argon2Params::default();
        assert_eq!(p.memory_kib, 19_456);
        assert_eq!(p.iterations, 2);
        assert_eq!(p.parallelism, 1);

        // ...and they actually work.
        let phc = PasswordHasher::default().hash("correct horse battery").unwrap();
        assert!(PasswordHasher::default().verify("correct horse battery", &phc));
        let stored = PasswordHasher::stored_params(&phc).unwrap();
        assert_eq!(stored, p);
    }
}
