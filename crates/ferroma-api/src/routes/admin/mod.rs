//! `docs/api.md` §4 — administration.
//!
//! Every route in this module is behind [`crate::extract::AdminUser`], so an
//! authenticated non-admin gets `403 forbidden` and an anonymous caller gets
//! `401 unauthorized` — the two are never confused, and no route has to remember to
//! check the bit itself.
//!
//! The pieces:
//!
//! * [`users`] — accounts and the addresses they own, including the Maildir and the
//!   six standard folders a new address needs.
//! * [`domains`] — managed domains, forwarding aliases and the two DNS screens
//!   (`/dns` diagnostics and `/dkim` key material).
//! * [`queue`] — the outbound queue: listing, one entry with its attempt history,
//!   retry, cancel and statistics.
//! * [`system`] — storage accounting, garbage collection, the audit trail, DB-backed
//!   settings and the first-run wizard.
//! * [`logs`] — the in-process log ring buffer and the server-wide device list,
//!   including the two revoke verbs.
//! * [`tls`] — the TLS posture of this deployment: the configured PEM files as the
//!   host sees them, and every port that can speak TLS.
//!
//! # DNS diagnostics
//!
//! `GET /domains/:id/dns` runs *live* checks. `ferroma-api` has no DNS dependency of
//! its own (`hickory-resolver` belongs to `ferroma-smtp`, which this crate may not
//! import), so the resolver used here is the platform one through
//! `std::net::ToSocketAddrs` for `A`/`AAAA`, and the record checks that need real DNS
//! — `MX`, `TXT`, `PTR` — are composed from what the *configuration* promises plus the
//! address resolution that can be done locally. Each record therefore reports
//! `status: "skip"` with a `hint` when it cannot be checked in this build, rather than
//! claiming a false `ok`. See the crate report for the exact split.

pub mod domains;
pub mod logs;
pub mod queue;
pub mod system;
pub mod tls;
pub mod users;
