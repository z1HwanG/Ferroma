//! Mailbox, folder, message, draft and attachment routes.
//!
//! This is the bulk of the management surface (`docs/api.md` §5) and it is also where
//! the platform's most important security property lives: **every route that takes an
//! id verifies ownership**, and a message belonging to somebody else is reported as
//! `404 not_found` — never `403 forbidden`, which would confirm that the id exists.
//!
//! The module is organised as:
//!
//! * [`shapes`] — the JSON responses, one type per documented object.
//! * [`ownership`] — the `404`-not-`403` checks, in one place.
//! * [`store`] — turning a request into RFC 5322 bytes, a Maildir file, a `messages`
//!   row, its recipients and its attachment rows.
//! * [`mailboxes`], [`messages`], [`attachments`], [`drafts`] — the handlers.

pub mod attachments;
pub mod contacts;
pub mod drafts;
pub mod mailboxes;
pub mod messages;
pub mod ownership;
pub mod shapes;
pub mod store;
