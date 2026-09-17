//! The Ferroma Client Protocol (FCP) — `docs/api.md` §6 and `docs/fcp.md`.
//!
//! This surface exists because IMAP cannot express what an official client needs:
//! incremental sync with a resumable cursor, server-side drafts, device management,
//! chunked attachments and realtime push. Everything here is bearer-only (a browser
//! cookie must never drive the desktop protocol) and goes through
//! [`crate::extract::ClientAuth`], which also enforces the protocol floor of
//! `client.min_protocol_version` with a `426 Upgrade Required`.
//!
//! The pieces:
//!
//! * [`auth`] — login (registering the device), refresh, logout and the account blob.
//! * [`resources`] — mailboxes, sync, messages, drafts, attachments and devices.
//!
//! Every mutating route accepts an `operation_id` and runs through
//! [`ferroma_sync::SyncService::with_operation`], which is what makes a retry after a
//! timeout — or after the client crashed mid-request — safe: the server replays the
//! recorded response instead of acting twice.

pub mod auth;
pub mod resources;
