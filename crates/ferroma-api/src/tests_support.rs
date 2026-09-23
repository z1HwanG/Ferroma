//! Test support shared by this crate's unit tests.
//!
//! This crate has no direct `sqlx` dependency, so it cannot build a *lazy* pool for a
//! pure-logic test; the tests that need repositories **and** a database live in
//! `tests/` instead, where `ferroma-storage` can be reached through the harness.
//! The send path commits through `ferroma_storage::store_submission` now, so there
//! is no in-crate queue seam left to fake.

/// Repositories over a pool that is never connected.
///
/// The pool is created with `sqlx`'s lazy constructor, which opens no socket, so a
/// pure-logic test needs no database; a query made through it fails at runtime, which
/// is exactly right — an accidental query becomes a visible failure rather than a hang.
/// `sqlx` is a **dev-dependency**, so this helper exists only in test builds; the
/// published library never touches a database driver of its own.
#[cfg(test)]
pub fn lazy_repos() -> ferroma_storage::Repositories {
    let pool = sqlx::PgPool::connect_lazy("postgres://ferroma@127.0.0.1:5433/ferroma_api_unit")
        .expect("a lazy pool is always constructible");
    ferroma_storage::Repositories::new(pool)
}
