//! Every route handler, grouped the way [`docs/api.md`](../../../docs/api.md) groups
//! them.
//!
//! * [`health`] — §2: health, version, autodiscovery and the MTA-STS policy.
//! * [`auth`] — §3: login, refresh, logout, `/auth/me` and password changes.
//! * [`admin`] — §4: users, domains, aliases, DNS, queue, storage, audit, settings,
//!   the first-run wizard, the log ring and the server-wide device list.
//! * [`mail`] — §5: mailboxes, folders, messages, drafts and attachments.
//! * [`client`] — §6: the Ferroma Client Protocol.
//!
//! Every handler returns `Result<_, `[`crate::error::ApiError`]`>`, so a failure leaves
//! through one envelope and one set of status codes.
//!
//! The submodules are **not** glob re-exported at this level: `admin` and `mail` both
//! define a `MailboxListResponse`, and a glob would make the name ambiguous. Call sites
//! name the module (`routes::admin::list_users`), which is clearer anyway.

pub mod admin;
pub mod auth;
pub mod client;
pub mod health;
pub mod mail;
