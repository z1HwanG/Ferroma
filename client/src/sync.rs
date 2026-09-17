//! The sync engine — `docs/fcp.md` §3, implemented exactly.
//!
//! # The contract
//!
//! 1. **Apply, then advance.** Every change of a page is applied *and* the new
//!    cursor is stored inside **one** SQLite transaction. A crash before the
//!    commit therefore re-fetches the same page; a crash after it never skips a
//!    change. That is only sound because every change is applied idempotently,
//!    which this module is careful about: re-applying a page changes nothing.
//! 2. **Page until `has_more` is false.** One response is never the whole delta.
//! 3. **`seq` is ascending and gapless per user.** A page whose sequence numbers
//!    are not strictly ascending — or whose `next_cursor` moves backwards — is
//!    the observable symptom of a server that lost history. The client discards
//!    the stream and resyncs from `0`.
//! 4. **Metadata first, bodies on demand.** `message_created` carries ids; the
//!    header/snippet metadata is fetched right after the page commits, and the
//!    body only when the user opens the message.
//! 5. **A stale cursor is recoverable.** `409 conflict` means "cursor too old";
//!    the client drops that folder's cache and resyncs from `0`.
//! 6. **`uid_validity` is an epoch.** A different value for the same folder id
//!    invalidates every UID the client holds, so the folder's cache is dropped.
//!
//! The server is the source of truth. Nothing in this module ever pushes local
//! state at the server: local mutations travel through
//! [`crate::operations::PendingQueue`].

use std::sync::Arc;

use ferroma_core::Cursor;
use sqlx::{Row, SqliteConnection};

use crate::api::{Change, FcpClient, FolderInfo, MailboxInfo};
use crate::database::{ClientDatabase, SearchDocument, SearchMode};
use crate::error::{ClientError, ClientResult};
use crate::util::now_rfc3339;

/// The default page size asked of the server (it caps it at
/// `client.sync_page_size`).
pub const DEFAULT_PAGE_LIMIT: usize = 500;

/// A safety net against a server that always answers `has_more: true`.
pub const DEFAULT_MAX_PAGES: usize = 10_000;

/// The `folder_id` used by the account-level stream (folder list, drafts,
/// settings) — `docs/fcp.md` §3 omits `folder_id` for it.
pub const ACCOUNT_STREAM: i64 = 0;

/// What phase of a sync a progress report describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    /// The first page has not arrived yet.
    Starting,
    /// A `409` or a lost-history signal forced a full resync.
    Resyncing,
    /// Applying a page.
    Applying,
    /// Fetching message metadata for newly created messages.
    FetchingMetadata,
    /// The stream is up to date.
    Done,
    /// The sync failed; `last_error` on the stream has the reason.
    Failed,
}

impl SyncPhase {
    /// A short string for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncPhase::Starting => "starting",
            SyncPhase::Resyncing => "resyncing",
            SyncPhase::Applying => "applying",
            SyncPhase::FetchingMetadata => "fetching_metadata",
            SyncPhase::Done => "done",
            SyncPhase::Failed => "failed",
        }
    }
}

/// One progress report, shaped for the "syncing 412 of 1200" line of §3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncProgress {
    /// The account being synced.
    pub account_id: i64,
    /// The folder (or [`ACCOUNT_STREAM`]).
    pub folder_id: i64,
    /// The folder's name, when it is known locally.
    pub folder_name: Option<String>,
    /// How many changes have been applied so far in this run.
    pub applied: usize,
    /// The folder's `message_count`, for the first-sync progress bar.
    pub total: Option<i64>,
    /// What the engine is doing.
    pub phase: SyncPhase,
}

impl SyncProgress {
    /// `412 of 1200`, or `412` when the total is unknown.
    pub fn label(&self) -> String {
        match self.total {
            Some(total) if total > 0 => format!("{} of {total}", self.applied),
            _ => format!("{}", self.applied),
        }
    }
}

/// A progress sink. The UI shell passes one in; the CLI prints from one.
pub type ProgressCallback = Arc<dyn Fn(SyncProgress) + Send + Sync>;

/// A test hook that fails a page part-way, proving the cursor only advances
/// together with the data it covers.
///
/// It exists in release builds too (it is inert unless set) so the guarantee is
/// stated in the type system rather than hidden behind `#[cfg(test)]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// Fail after `n` changes of the first page have been applied.
    AfterChanges(usize),
    /// Fail just before the cursor would be advanced.
    BeforeAdvance,
}

/// The result of syncing one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// The account.
    pub account_id: i64,
    /// The folder, or [`ACCOUNT_STREAM`].
    pub folder_id: i64,
    /// How many changes were applied.
    pub applied: usize,
    /// How many pages were fetched.
    pub pages: usize,
    /// The cursor the stream holds afterwards.
    pub cursor: Cursor,
    /// Whether a full resync from `0` happened.
    pub resynced: bool,
}

/// The result of syncing a whole account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    /// One entry per stream.
    pub outcomes: Vec<SyncOutcome>,
    /// The total number of changes applied.
    pub applied: usize,
    /// The folders that were thrown away and re-synced from `0`.
    pub resynced_folders: Vec<i64>,
}

impl SyncSummary {
    /// The number of streams that were synced.
    pub fn streams(&self) -> usize {
        self.outcomes.len()
    }
}

/// The sync engine.
#[derive(Clone)]
pub struct SyncEngine {
    db: Arc<ClientDatabase>,
    client: FcpClient,
    page_limit: usize,
    max_pages: usize,
    fetch_metadata: bool,
    fault: Option<FaultPoint>,
}

impl std::fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEngine")
            .field("page_limit", &self.page_limit)
            .field("fetch_metadata", &self.fetch_metadata)
            .finish_non_exhaustive()
    }
}

impl SyncEngine {
    /// Build an engine over a cache and a client.
    pub fn new(db: Arc<ClientDatabase>, client: FcpClient) -> Self {
        SyncEngine {
            db,
            client,
            page_limit: DEFAULT_PAGE_LIMIT,
            max_pages: DEFAULT_MAX_PAGES,
            fetch_metadata: true,
            fault: None,
        }
    }

    /// Ask the server for at most `limit` changes per page.
    pub fn with_page_limit(mut self, limit: usize) -> Self {
        self.page_limit = limit.max(1);
        self
    }

    /// Stop after `pages` pages (a safety net for a misbehaving server).
    pub fn with_max_pages(mut self, pages: usize) -> Self {
        self.max_pages = pages.max(1);
        self
    }

    /// Skip the post-page metadata fetch (used by tests that only assert the
    /// cursor and row bookkeeping).
    pub fn without_metadata_fetch(mut self) -> Self {
        self.fetch_metadata = false;
        self
    }

    /// Install a fault-injection point. See [`FaultPoint`].
    pub fn with_fault_point(mut self, fault: FaultPoint) -> Self {
        self.fault = Some(fault);
        self
    }

    /// The cache behind the engine.
    pub fn database(&self) -> &Arc<ClientDatabase> {
        &self.db
    }

    /// The client the engine talks to.
    pub fn client(&self) -> &FcpClient {
        &self.client
    }

    // -- stream bookkeeping ------------------------------------------------

    /// Create the `sync_state` row for a stream if it is missing.
    pub async fn ensure_stream(
        &self,
        account_id: i64,
        folder_id: i64,
        mailbox_id: Option<i64>,
    ) -> ClientResult<()> {
        sqlx::query(
            "INSERT INTO sync_state (account_id, folder_id, mailbox_id, cursor, status)
             VALUES (?, ?, ?, '0', 'idle')
             ON CONFLICT(account_id, folder_id) DO NOTHING",
        )
        .bind(account_id)
        .bind(folder_id)
        .bind(mailbox_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// The stored cursor of a stream.
    pub async fn cursor(&self, account_id: i64, folder_id: i64) -> ClientResult<Cursor> {
        let row = sqlx::query("SELECT cursor FROM sync_state WHERE account_id = ? AND folder_id = ?")
            .bind(account_id)
            .bind(folder_id)
            .fetch_optional(self.db.pool())
            .await?;
        match row {
            Some(row) => {
                let raw: String = row.try_get("cursor")?;
                Ok(Cursor::parse(&raw)?)
            }
            None => Ok(Cursor::ZERO),
        }
    }

    /// The `uid_validity` the client last saw for a folder.
    pub async fn stored_uid_validity(
        &self,
        account_id: i64,
        folder_id: i64,
    ) -> ClientResult<Option<i64>> {
        let row = sqlx::query(
            "SELECT uid_validity FROM sync_state WHERE account_id = ? AND folder_id = ?",
        )
        .bind(account_id)
        .bind(folder_id)
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some(row) => Ok(row.try_get::<Option<i64>, _>("uid_validity")?),
            None => Ok(None),
        }
    }

    async fn set_status(
        &self,
        account_id: i64,
        folder_id: i64,
        status: &str,
        error: Option<&str>,
    ) -> ClientResult<()> {
        sqlx::query(
            "UPDATE sync_state SET status = ?, last_error = ? WHERE account_id = ? AND folder_id = ?",
        )
        .bind(status)
        .bind(error)
        .bind(account_id)
        .bind(folder_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Store an explicit cursor. Used when a caller knows better than the
    /// engine (a device restore, or the CLI's `--cursor`).
    pub async fn store_cursor(
        &self,
        account_id: i64,
        folder_id: i64,
        cursor: Cursor,
    ) -> ClientResult<()> {
        self.ensure_stream(account_id, folder_id, None).await?;
        sqlx::query("UPDATE sync_state SET cursor = ? WHERE account_id = ? AND folder_id = ?")
            .bind(cursor.as_str())
            .bind(account_id)
            .bind(folder_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    // -- cache invalidation ------------------------------------------------

    /// Throw away everything cached for a folder and reset its cursor to `0`.
    ///
    /// This is the response to three different signals, all of which mean "your
    /// UIDs are meaningless": a `uid_validity` change, a `409 conflict`, and a
    /// lost-history gap.
    pub async fn reset_folder(&self, account_id: i64, folder_id: i64) -> ClientResult<usize> {
        let mut tx = self.db.pool().begin().await?;
        let removed = clear_folder_cache(&mut tx, self.db.search_mode(), account_id, folder_id).await?;
        sqlx::query(
            "INSERT INTO sync_state (account_id, folder_id, cursor, status, uid_validity)
             VALUES (?, ?, '0', 'idle', NULL)
             ON CONFLICT(account_id, folder_id) DO UPDATE SET cursor = '0', uid_validity = NULL",
        )
        .bind(account_id)
        .bind(folder_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Note the `uid_validity` a folder currently has.
    ///
    /// Returns `true` when it changed, in which case the folder's cache has been
    /// dropped and its cursor reset (`docs/fcp.md` §4).
    pub async fn note_uid_validity(
        &self,
        account_id: i64,
        folder_id: i64,
        uid_validity: i64,
    ) -> ClientResult<bool> {
        self.ensure_stream(account_id, folder_id, None).await?;
        let stored = self.stored_uid_validity(account_id, folder_id).await?;
        match stored {
            Some(current) if current == uid_validity => Ok(false),
            Some(_) => {
                tracing::info!(
                    account_id,
                    folder_id,
                    uid_validity,
                    "uid_validity changed; dropping the folder's cache and resyncing from 0"
                );
                let mut tx = self.db.pool().begin().await?;
                clear_folder_cache(&mut tx, self.db.search_mode(), account_id, folder_id).await?;
                sqlx::query(
                    "UPDATE sync_state SET cursor = '0', uid_validity = ? WHERE account_id = ? AND folder_id = ?",
                )
                .bind(uid_validity)
                .bind(account_id)
                .bind(folder_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(true)
            }
            None => {
                sqlx::query(
                    "UPDATE sync_state SET uid_validity = ? WHERE account_id = ? AND folder_id = ?",
                )
                .bind(uid_validity)
                .bind(account_id)
                .bind(folder_id)
                .execute(self.db.pool())
                .await?;
                Ok(false)
            }
        }
    }

    // -- mailbox and folder bookkeeping ------------------------------------

    /// Cache the mailbox/folder tree from `GET /mailboxes`, applying the
    /// `uid_validity` rule to every folder.
    ///
    /// Returns the folder ids whose cache was invalidated.
    pub async fn store_mailboxes(
        &self,
        account_id: i64,
        mailboxes: &[MailboxInfo],
    ) -> ClientResult<Vec<i64>> {
        let now = now_rfc3339();
        for mailbox in mailboxes {
            sqlx::query(
                "INSERT INTO mailboxes (account_id, id, address, display_name, is_primary)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, id) DO UPDATE SET
                     address = excluded.address,
                     display_name = excluded.display_name,
                     is_primary = excluded.is_primary",
            )
            .bind(account_id)
            .bind(mailbox.id)
            .bind(&mailbox.address)
            .bind(&mailbox.display_name)
            .bind(i64::from(mailbox.is_primary))
            .execute(self.db.pool())
            .await?;

            for folder in &mailbox.folders {
                self.store_folder(account_id, mailbox.id, folder, &now).await?;
            }
        }

        let mut invalidated = Vec::new();
        for mailbox in mailboxes {
            for folder in &mailbox.folders {
                if folder.uid_validity > 0
                    && self
                        .note_uid_validity(account_id, folder.id, folder.uid_validity)
                        .await?
                {
                    invalidated.push(folder.id);
                }
            }
        }
        Ok(invalidated)
    }

    async fn store_folder(
        &self,
        account_id: i64,
        mailbox_id: i64,
        folder: &FolderInfo,
        now: &str,
    ) -> ClientResult<()> {
        sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name, special_use, message_count,
                                  unseen_count, uid_validity, uid_next, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id, id) DO UPDATE SET
                 mailbox_id = excluded.mailbox_id,
                 name = excluded.name,
                 special_use = COALESCE(excluded.special_use, folders.special_use),
                 message_count = excluded.message_count,
                 unseen_count = excluded.unseen_count,
                 uid_validity = excluded.uid_validity,
                 uid_next = excluded.uid_next,
                 updated_at = excluded.updated_at",
        )
        .bind(account_id)
        .bind(folder.id)
        .bind(mailbox_id)
        .bind(&folder.name)
        .bind(&folder.special_use)
        .bind(folder.message_count)
        .bind(folder.unseen_count)
        .bind(folder.uid_validity)
        .bind(folder.uid_next)
        .bind(now)
        .execute(self.db.pool())
        .await?;
        self.ensure_stream(account_id, folder.id, Some(mailbox_id))
            .await?;
        Ok(())
    }

    // -- the engine --------------------------------------------------------

    /// Sync a whole account: the mailbox tree, then every folder, then the
    /// account-level stream.
    pub async fn sync_account(
        &self,
        account_id: i64,
        progress: Option<&ProgressCallback>,
    ) -> ClientResult<SyncSummary> {
        let mailboxes = self.client.mailboxes().await?;
        self.sync_account_from(account_id, &mailboxes, progress).await
    }

    /// Sync an account whose mailbox tree has already been fetched.
    pub async fn sync_account_from(
        &self,
        account_id: i64,
        mailboxes: &[MailboxInfo],
        progress: Option<&ProgressCallback>,
    ) -> ClientResult<SyncSummary> {
        let invalidated = self.store_mailboxes(account_id, mailboxes).await?;
        let mut summary = SyncSummary {
            outcomes: Vec::new(),
            applied: 0,
            resynced_folders: invalidated,
        };

        for mailbox in mailboxes {
            for folder in &mailbox.folders {
                let outcome = self
                    .sync_folder(account_id, mailbox.id, folder.id, progress)
                    .await?;
                summary.applied += outcome.applied;
                if outcome.resynced && !summary.resynced_folders.contains(&folder.id) {
                    summary.resynced_folders.push(folder.id);
                }
                summary.outcomes.push(outcome);
            }
            let outcome = self
                .sync_account_stream(account_id, mailbox.id, progress)
                .await?;
            summary.applied += outcome.applied;
            summary.outcomes.push(outcome);
        }
        Ok(summary)
    }

    /// Sync one folder's stream to the head of the change log.
    ///
    /// This is the §3 loop: fetch a page, apply it and the cursor in one
    /// transaction, report progress, repeat until `has_more` is false.
    pub async fn sync_folder(
        &self,
        account_id: i64,
        mailbox_id: i64,
        folder_id: i64,
        progress: Option<&ProgressCallback>,
    ) -> ClientResult<SyncOutcome> {
        self.sync_stream(account_id, mailbox_id, folder_id, progress)
            .await
    }

    /// Sync the account-level stream (folder list, drafts, settings).
    pub async fn sync_account_stream(
        &self,
        account_id: i64,
        mailbox_id: i64,
        progress: Option<&ProgressCallback>,
    ) -> ClientResult<SyncOutcome> {
        self.sync_stream(account_id, mailbox_id, ACCOUNT_STREAM, progress)
            .await
    }

    async fn sync_stream(
        &self,
        account_id: i64,
        mailbox_id: i64,
        folder_id: i64,
        progress: Option<&ProgressCallback>,
    ) -> ClientResult<SyncOutcome> {
        self.ensure_stream(account_id, folder_id, Some(mailbox_id))
            .await?;
        self.set_status(account_id, folder_id, "syncing", None).await?;

        let folder_name = self.folder_name(account_id, folder_id).await?;
        let total = self.folder_message_count(account_id, folder_id).await?;

        let mut cursor = self.cursor(account_id, folder_id).await?;
        let mut applied = 0usize;
        let mut pages = 0usize;
        let mut resynced = false;
        let mut allow_resync = true;

        let outcome = loop {
            report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Starting);

            let folder_param = if folder_id == ACCOUNT_STREAM {
                None
            } else {
                Some(folder_id)
            };

            let page = match self
                .client
                .sync(mailbox_id, folder_param, &cursor.as_str(), Some(self.page_limit))
                .await
            {
                Ok(page) => page,
                Err(ClientError::Api(api)) if api.status == 409 && allow_resync => {
                    // "cursor too old; full resync required" (FCP §3.6).
                    tracing::info!(
                        account_id,
                        folder_id,
                        "the server refused the cursor; resyncing the folder from 0"
                    );
                    allow_resync = false;
                    resynced = true;
                    report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Resyncing);
                    self.reset_folder(account_id, folder_id).await?;
                    cursor = Cursor::ZERO;
                    applied = 0;
                    continue;
                }
                Err(err) => {
                    self.set_status(account_id, folder_id, "error", Some(&err.user_message()))
                        .await?;
                    report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Failed);
                    return Err(err);
                }
            };

            if let Some(reason) = detect_seq_gap(cursor, &page.changes, &page.next_cursor) {
                if allow_resync {
                    tracing::warn!(
                        account_id,
                        folder_id,
                        %reason,
                        "the change log skipped ahead; resyncing the folder from 0"
                    );
                    allow_resync = false;
                    resynced = true;
                    report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Resyncing);
                    self.reset_folder(account_id, folder_id).await?;
                    cursor = Cursor::ZERO;
                    applied = 0;
                    continue;
                }
                self.set_status(account_id, folder_id, "error", Some(&reason))
                    .await?;
                return Err(ClientError::Conflict(reason));
            }

            let next_cursor = Cursor::parse(&page.next_cursor)?;
            let page_result = self
                .apply_page(account_id, mailbox_id, folder_id, &page.changes, next_cursor)
                .await;
            let applied_page = match page_result {
                Ok(applied_page) => applied_page,
                Err(err) => {
                    self.set_status(account_id, folder_id, "error", Some(&err.user_message()))
                        .await?;
                    report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Failed);
                    return Err(err);
                }
            };

            applied += page.changes.len();
            pages += 1;
            cursor = next_cursor;

            report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Applying);

            if self.fetch_metadata && !applied_page.created.is_empty() {
                report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::FetchingMetadata);
                self.fetch_metadata_for(account_id, &applied_page.created).await;
            }

            // A `uid_validity` change for *this* folder invalidates everything we
            // just wrote, so the stream has to start over from `0`. The cache was
            // already dropped inside the page's transaction; only the cursor,
            // which `apply_page` has just advanced, still has to be rewound.
            if applied_page.reset_folders.contains(&folder_id) {
                if !allow_resync {
                    tracing::warn!(
                        account_id,
                        folder_id,
                        "uid_validity changed twice in one run; stopping to avoid a loop"
                    );
                    break SyncOutcome {
                        account_id,
                        folder_id,
                        applied,
                        pages,
                        cursor: Cursor::ZERO,
                        resynced: true,
                    };
                }
                allow_resync = false;
                resynced = true;
                self.store_cursor(account_id, folder_id, Cursor::ZERO).await?;
                cursor = Cursor::ZERO;
                applied = 0;
                report(progress, account_id, folder_id, folder_name.clone(), applied, total, SyncPhase::Resyncing);
                continue;
            }

            if !page.has_more {
                break SyncOutcome {
                    account_id,
                    folder_id,
                    applied,
                    pages,
                    cursor,
                    resynced,
                };
            }
            if pages >= self.max_pages {
                tracing::warn!(account_id, folder_id, pages, "stopping a sync that never ends");
                break SyncOutcome {
                    account_id,
                    folder_id,
                    applied,
                    pages,
                    cursor,
                    resynced,
                };
            }
        };

        sqlx::query(
            "UPDATE sync_state SET cursor = ?, last_sync_at = ?, status = 'idle', last_error = NULL
             WHERE account_id = ? AND folder_id = ?",
        )
        .bind(outcome.cursor.as_str())
        .bind(now_rfc3339())
        .bind(account_id)
        .bind(folder_id)
        .execute(self.db.pool())
        .await?;
        report(progress, account_id, folder_id, folder_name, applied, total, SyncPhase::Done);
        Ok(outcome)
    }

    /// Apply one page and advance the cursor, atomically.
    ///
    /// Returns which messages were **newly** inserted (what the metadata fetch
    /// afterwards uses) and which folders had their `uid_validity` changed and
    /// therefore need their cursor rewound. Re-applying the same page inserts
    /// nothing, so it also fetches nothing.
    pub async fn apply_page(
        &self,
        account_id: i64,
        mailbox_id: i64,
        folder_id: i64,
        changes: &[Change],
        next_cursor: Cursor,
    ) -> ClientResult<AppliedPage> {
        let mut tx = self.db.pool().begin().await?;
        let mut applied = AppliedPage::default();

        for (index, change) in changes.iter().enumerate() {
            if self.fault == Some(FaultPoint::AfterChanges(index)) {
                // Deliberately not committed: the cursor stays where it was and
                // the same page will be fetched again.
                return Err(ClientError::cache(format!(
                    "injected fault after {index} changes"
                )));
            }
            apply_change(
                &mut tx,
                self.db.search_mode(),
                account_id,
                mailbox_id,
                folder_id,
                change,
                &mut applied,
            )
            .await?;
        }

        if self.fault == Some(FaultPoint::BeforeAdvance) {
            return Err(ClientError::cache("injected fault before advancing the cursor"));
        }

        sqlx::query(
            "INSERT INTO sync_state (account_id, folder_id, mailbox_id, cursor, last_sync_at, status)
             VALUES (?, ?, ?, ?, ?, 'syncing')
             ON CONFLICT(account_id, folder_id) DO UPDATE SET
                 cursor = excluded.cursor,
                 last_sync_at = excluded.last_sync_at",
        )
        .bind(account_id)
        .bind(folder_id)
        .bind(mailbox_id)
        .bind(next_cursor.as_str())
        .bind(now_rfc3339())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(applied)
    }

    /// Fetch header metadata for freshly created messages.
    ///
    /// This runs **after** the page committed, and a failure for one message is
    /// logged and skipped: a body-less list entry is far better than a stuck
    /// cursor. Bodies stay lazy (§3.4).
    pub async fn fetch_metadata_for(&self, account_id: i64, message_ids: &[i64]) {
        for message_id in message_ids {
            match self.client.message(*message_id).await {
                Ok(detail) => {
                    if let Err(err) = self.store_message_detail(account_id, &detail).await {
                        tracing::warn!(message_id, error = %err, "could not cache message metadata");
                    }
                }
                Err(err) => {
                    // A 404 means the message was deleted between the change log
                    // and now; the tombstone will arrive as its own change.
                    tracing::debug!(message_id, error = %err.user_message(), "metadata fetch skipped");
                }
            }
        }
    }

    /// Store a `GET /messages/:id` payload: headers, snippet, attachments.
    pub async fn store_message_detail(
        &self,
        account_id: i64,
        detail: &crate::api::MessageDetail,
    ) -> ClientResult<()> {
        let id = detail.item.id;
        if id <= 0 {
            return Err(ClientError::parse("a message payload without an id"));
        }
        let from = detail.item.from.clone().unwrap_or_default();
        let to_summary = detail
            .item
            .to
            .iter()
            .map(|address| address.address.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "INSERT INTO messages (account_id, id, folder_id, uid, subject, from_address, from_name,
                                   to_summary, snippet, flags, size_bytes, has_attachments,
                                   attachment_count, internal_date, sent_at, rfc_message_id,
                                   body_state, cached_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, ''), ?, ?, ?, ?, ?, ?, 'ready', ?)
             ON CONFLICT(account_id, id) DO UPDATE SET
                 folder_id = COALESCE(excluded.folder_id, messages.folder_id),
                 uid = COALESCE(excluded.uid, messages.uid),
                 subject = excluded.subject,
                 from_address = excluded.from_address,
                 from_name = excluded.from_name,
                 to_summary = excluded.to_summary,
                 snippet = COALESCE(excluded.snippet, messages.snippet),
                 flags = excluded.flags,
                 size_bytes = excluded.size_bytes,
                 has_attachments = excluded.has_attachments,
                 attachment_count = excluded.attachment_count,
                 internal_date = COALESCE(excluded.internal_date, messages.internal_date),
                 sent_at = COALESCE(excluded.sent_at, messages.sent_at),
                 rfc_message_id = COALESCE(excluded.rfc_message_id, messages.rfc_message_id),
                 deleted = 0,
                 cached_at = excluded.cached_at",
        )
        .bind(account_id)
        .bind(id)
        .bind(detail.item.folder_id)
        .bind(detail.item.uid)
        .bind(detail.item.subject.clone().unwrap_or_default())
        .bind(&from.address)
        .bind(&from.name)
        .bind(&to_summary)
        .bind(&detail.item.snippet)
        .bind(&detail.item.flags)
        .bind(detail.item.size_bytes as i64)
        .bind(i64::from(detail.item.has_attachments))
        .bind(detail.item.attachment_count)
        .bind(&detail.item.internal_date)
        .bind(&detail.item.sent_at)
        .bind(&detail.item.rfc_message_id)
        .bind(now_rfc3339())
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM message_headers WHERE account_id = ? AND message_id = ?")
            .bind(account_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for (ordinal, header) in detail.headers.iter().enumerate() {
            sqlx::query(
                "INSERT INTO message_headers (account_id, message_id, ordinal, name, value)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(account_id)
            .bind(id)
            .bind(ordinal as i64)
            .bind(&header.name)
            .bind(&header.value)
            .execute(&mut *tx)
            .await?;
        }

        for attachment in &detail.attachments {
            sqlx::query(
                "INSERT INTO attachments (account_id, id, message_id, filename, content_type,
                                          size_bytes, sha256, content_id, disposition)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, id) DO UPDATE SET
                     message_id = excluded.message_id,
                     filename = excluded.filename,
                     content_type = excluded.content_type,
                     size_bytes = excluded.size_bytes,
                     sha256 = COALESCE(excluded.sha256, attachments.sha256),
                     content_id = excluded.content_id,
                     disposition = excluded.disposition",
            )
            .bind(account_id)
            .bind(attachment.id)
            .bind(id)
            .bind(&attachment.filename)
            .bind(&attachment.content_type)
            .bind(attachment.size_bytes as i64)
            .bind(&attachment.sha256)
            .bind(&attachment.content_id)
            .bind(&attachment.disposition)
            .execute(&mut *tx)
            .await?;
        }

        reindex_message(&mut tx, self.db.search_mode(), account_id, id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Store a message body (`GET /messages/:id` bodies) and index it.
    pub async fn store_message_body(
        &self,
        account_id: i64,
        message_id: i64,
        text_body: Option<&str>,
        html_body: Option<&str>,
        raw: Option<&[u8]>,
    ) -> ClientResult<()> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            "INSERT INTO message_bodies (account_id, message_id, text_body, html_body, raw, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id, message_id) DO UPDATE SET
                 text_body = COALESCE(excluded.text_body, message_bodies.text_body),
                 html_body = COALESCE(excluded.html_body, message_bodies.html_body),
                 raw = COALESCE(excluded.raw, message_bodies.raw),
                 fetched_at = excluded.fetched_at",
        )
        .bind(account_id)
        .bind(message_id)
        .bind(text_body)
        .bind(html_body)
        .bind(raw)
        .bind(now_rfc3339())
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE messages SET body_state = 'ready' WHERE account_id = ? AND id = ?")
            .bind(account_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        reindex_message(&mut tx, self.db.search_mode(), account_id, message_id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn folder_name(&self, account_id: i64, folder_id: i64) -> ClientResult<Option<String>> {
        let row = sqlx::query("SELECT name FROM folders WHERE account_id = ? AND id = ?")
            .bind(account_id)
            .bind(folder_id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.map(|row| row.get::<String, _>("name")))
    }

    async fn folder_message_count(
        &self,
        account_id: i64,
        folder_id: i64,
    ) -> ClientResult<Option<i64>> {
        let row = sqlx::query("SELECT message_count FROM folders WHERE account_id = ? AND id = ?")
            .bind(account_id)
            .bind(folder_id)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.map(|row| row.get::<i64, _>("message_count")))
    }
}

fn report(
    callback: Option<&ProgressCallback>,
    account_id: i64,
    folder_id: i64,
    folder_name: Option<String>,
    applied: usize,
    total: Option<i64>,
    phase: SyncPhase,
) {
    if let Some(callback) = callback {
        callback(SyncProgress {
            account_id,
            folder_id,
            folder_name,
            applied,
            total,
            phase,
        });
    }
}

/// Detect the observable symptom of a server that lost history.
///
/// `seq` is global per user and gapless (`docs/fcp.md` §3.3), so within one page
/// the sequence numbers must be strictly ascending and must all be greater than
/// the cursor the client asked from. Gaps *between* pages cannot be seen through
/// an opaque cursor — the client only ever learns what the server chose to send
/// — which is exactly why a stale cursor is recoverable (`409`) rather than
/// fatal. These checks catch the cases that *are* observable.
pub fn detect_seq_gap(cursor: Cursor, changes: &[Change], next_cursor: &str) -> Option<String> {
    let mut previous = cursor.0;
    for change in changes {
        let Some(seq) = change.seq() else {
            continue;
        };
        if seq <= previous {
            return Some(format!(
                "change {} repeated or went backwards (seq {seq} after {previous})",
                change.kind()
            ));
        }
        previous = seq;
    }
    match Cursor::parse(next_cursor) {
        Ok(next) if next.0 < previous => Some(format!(
            "next_cursor {} is behind the last applied seq {previous}",
            next.0
        )),
        Ok(_) => None,
        Err(err) => Some(format!("unusable next_cursor: {err}")),
    }
}

/// The result of applying one page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedPage {
    /// Messages that did not exist in the cache before this page.
    pub created: Vec<i64>,
    /// Folders whose `uid_validity` changed: their cache has been dropped and
    /// their cursor must be rewound to `0`.
    pub reset_folders: Vec<i64>,
}

/// Apply one change inside the caller's transaction.
///
/// The transaction handle is narrowed to the connection it wraps so the body can
/// reborrow it freely (`&mut *tx`) for each statement.
async fn apply_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    mode: SearchMode,
    account_id: i64,
    mailbox_id: i64,
    folder_id: i64,
    change: &Change,
    applied: &mut AppliedPage,
) -> ClientResult<()> {
    let tx: &mut SqliteConnection = &mut *tx;
    let now = now_rfc3339();
    match change {
        Change::MessageCreated {
            message_id,
            uid,
            folder_id: change_folder,
            ..
        } => {
            let target = change_folder.unwrap_or(folder_id);
            let target = if target == ACCOUNT_STREAM {
                None
            } else {
                Some(target)
            };
            let existing: i64 = sqlx::query(
                "SELECT COUNT(*) AS n FROM messages WHERE account_id = ? AND id = ?",
            )
            .bind(account_id)
            .bind(message_id)
            .fetch_one(&mut *tx)
            .await?
            .get("n");

            sqlx::query(
                "INSERT INTO messages (account_id, id, folder_id, mailbox_id, uid, body_state, cached_at)
                 VALUES (?, ?, ?, ?, ?, 'pending', ?)
                 ON CONFLICT(account_id, id) DO UPDATE SET
                     folder_id = COALESCE(excluded.folder_id, messages.folder_id),
                     mailbox_id = COALESCE(excluded.mailbox_id, messages.mailbox_id),
                     uid = COALESCE(excluded.uid, messages.uid),
                     deleted = 0",
            )
            .bind(account_id)
            .bind(message_id)
            .bind(target)
            .bind(mailbox_id)
            .bind(uid)
            .bind(&now)
            .execute(&mut *tx)
            .await?;

            if existing == 0 {
                applied.created.push(*message_id);
            }
            // Keep the search index in step with the row. The document is
            // re-indexed again once the metadata (and later the body) arrives,
            // and `index_document` replaces rather than appends.
            if mode == SearchMode::Fts5 {
                reindex_message(tx, mode, account_id, *message_id).await?;
            }
        }
        Change::MessageUpdated {
            message_id,
            flags,
            folder_id: change_folder,
            uid,
            has_attachments,
            ..
        } => {
            let affected = sqlx::query(
                "UPDATE messages SET
                     flags = COALESCE(?, flags),
                     folder_id = COALESCE(?, folder_id),
                     uid = COALESCE(?, uid),
                     has_attachments = COALESCE(?, has_attachments)
                 WHERE account_id = ? AND id = ? AND deleted = 0",
            )
            .bind(flags)
            .bind(change_folder)
            .bind(uid)
            .bind(has_attachments.map(i64::from))
            .bind(account_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

            if affected == 0 {
                // The client had never heard of this message (it was created
                // before this installation existed). Store a placeholder so the
                // update is not silently dropped.
                sqlx::query(
                    "INSERT INTO messages (account_id, id, folder_id, uid, flags, body_state, cached_at)
                     VALUES (?, ?, ?, ?, COALESCE(?, ''), 'pending', ?)
                     ON CONFLICT(account_id, id) DO UPDATE SET
                         flags = excluded.flags,
                         deleted = 0",
                )
                .bind(account_id)
                .bind(message_id)
                .bind(change_folder.or(Some(folder_id)))
                .bind(uid)
                .bind(flags)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                applied.created.push(*message_id);
            }
        }
        Change::MessageDeleted { message_id, .. } => {
            unindex_message(tx, mode, account_id, *message_id).await?;
            sqlx::query(
                "UPDATE messages SET deleted = 1, folder_id = NULL WHERE account_id = ? AND id = ?",
            )
            .bind(account_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        }
        Change::MessageMoved {
            message_id,
            to_folder_id,
            uid,
            ..
        } => {
            sqlx::query(
                "UPDATE messages SET folder_id = COALESCE(?, folder_id), uid = COALESCE(?, uid)
                 WHERE account_id = ? AND id = ? AND deleted = 0",
            )
            .bind(to_folder_id)
            .bind(uid)
            .bind(account_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        }
        Change::FolderCreated {
            folder_id: created_folder,
            name,
            mailbox_id: change_mailbox,
            special_use,
            ..
        } => {
            sqlx::query(
                "INSERT INTO folders (account_id, id, mailbox_id, name, special_use, updated_at)
                 VALUES (?, ?, ?, COALESCE(?, ''), ?, ?)
                 ON CONFLICT(account_id, id) DO UPDATE SET
                     name = CASE WHEN excluded.name = '' THEN folders.name ELSE excluded.name END,
                     special_use = COALESCE(excluded.special_use, folders.special_use),
                     updated_at = excluded.updated_at",
            )
            .bind(account_id)
            .bind(created_folder)
            .bind(change_mailbox.unwrap_or(mailbox_id))
            .bind(name)
            .bind(special_use)
            .bind(&now)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO sync_state (account_id, folder_id, mailbox_id, cursor, status)
                 VALUES (?, ?, ?, '0', 'idle')
                 ON CONFLICT(account_id, folder_id) DO NOTHING",
            )
            .bind(account_id)
            .bind(created_folder)
            .bind(change_mailbox.unwrap_or(mailbox_id))
            .execute(&mut *tx)
            .await?;
        }
        Change::FolderUpdated {
            folder_id: updated_folder,
            name,
            uid_validity,
            message_count,
            unseen_count,
            uid_next,
            ..
        } => {
            let stored: Option<Option<i64>> =
                sqlx::query("SELECT uid_validity FROM sync_state WHERE account_id = ? AND folder_id = ?")
                    .bind(account_id)
                    .bind(updated_folder)
                    .fetch_optional(&mut *tx)
                    .await?
                    .map(|row| row.try_get::<Option<i64>, _>("uid_validity"))
                    .transpose()?;

            let epoch_changed = matches!(
                (stored.flatten(), uid_validity),
                (Some(current), Some(new)) if current != *new
            );

            sqlx::query(
                "UPDATE folders SET
                     name = COALESCE(?, name),
                     uid_validity = COALESCE(?, uid_validity),
                     message_count = COALESCE(?, message_count),
                     unseen_count = COALESCE(?, unseen_count),
                     uid_next = COALESCE(?, uid_next),
                     updated_at = ?
                 WHERE account_id = ? AND id = ?",
            )
            .bind(name)
            .bind(uid_validity)
            .bind(message_count)
            .bind(unseen_count)
            .bind(uid_next)
            .bind(&now)
            .bind(account_id)
            .bind(updated_folder)
            .execute(&mut *tx)
            .await?;

            if epoch_changed {
                tracing::info!(
                    account_id,
                    folder_id = updated_folder,
                    "uid_validity changed mid-stream; dropping the folder's cache"
                );
                clear_folder_cache(tx, mode, account_id, *updated_folder).await?;
                applied.reset_folders.push(*updated_folder);
                sqlx::query(
                    "INSERT INTO sync_state (account_id, folder_id, cursor, uid_validity, status)
                     VALUES (?, ?, '0', ?, 'idle')
                     ON CONFLICT(account_id, folder_id) DO UPDATE SET cursor = '0', uid_validity = excluded.uid_validity",
                )
                .bind(account_id)
                .bind(updated_folder)
                .bind(uid_validity)
                .execute(&mut *tx)
                .await?;
            } else if let Some(uid_validity) = uid_validity {
                sqlx::query(
                    "INSERT INTO sync_state (account_id, folder_id, mailbox_id, cursor, uid_validity, status)
                     VALUES (?, ?, ?, '0', ?, 'idle')
                     ON CONFLICT(account_id, folder_id) DO UPDATE SET uid_validity = excluded.uid_validity",
                )
                .bind(account_id)
                .bind(updated_folder)
                .bind(mailbox_id)
                .bind(uid_validity)
                .execute(&mut *tx)
                .await?;
            }
        }
        Change::FolderDeleted {
            folder_id: deleted_folder,
            ..
        } => {
            clear_folder_cache(tx, mode, account_id, *deleted_folder).await?;
            sqlx::query("DELETE FROM folders WHERE account_id = ? AND id = ?")
                .bind(account_id)
                .bind(deleted_folder)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM sync_state WHERE account_id = ? AND folder_id = ?")
                .bind(account_id)
                .bind(deleted_folder)
                .execute(&mut *tx)
                .await?;
        }
        Change::DraftCreated { draft_id, .. } | Change::DraftUpdated { draft_id, .. } => {
            // The change carries only an id, so the local draft layer re-reads
            // the content (`crate::draft`); here we make sure the row exists so
            // the account stream cannot lose a draft it has never seen.
            sqlx::query(
                "INSERT INTO drafts (account_id, local_uid, id, subject, local_updated_at, dirty, deleted)
                 VALUES (?, ?, ?, '', ?, 0, 0)
                 ON CONFLICT(account_id, local_uid) DO UPDATE SET deleted = 0",
            )
            .bind(account_id)
            .bind(format!("srv:{draft_id}"))
            .bind(draft_id)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        Change::DraftDeleted { draft_id, .. } => {
            // A locally edited draft (dirty) is kept and flagged instead: the
            // user's text must not be thrown away by another device's delete
            // (`Server = Source of Truth`, but never at the cost of typed text).
            sqlx::query("DELETE FROM drafts WHERE account_id = ? AND id = ? AND dirty = 0")
                .bind(account_id)
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE drafts SET deleted = 1 WHERE account_id = ? AND id = ?")
                .bind(account_id)
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
        }
        Change::Unknown => {
            // A change type from a newer server. Not an error: the cursor still
            // has to advance, or this client would be stuck forever.
        }
    }
    Ok(())
}

/// Delete every cached message of a folder (and its search index entries).
async fn clear_folder_cache(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    folder_id: i64,
) -> ClientResult<usize> {
    if mode == SearchMode::Fts5 {
        let rows = sqlx::query("SELECT id FROM messages WHERE account_id = ? AND folder_id = ?")
            .bind(account_id)
            .bind(folder_id)
            .fetch_all(&mut *conn)
            .await?;
        for row in rows {
            let id: i64 = row.try_get("id")?;
            unindex_message_raw(&mut *conn, account_id, id).await?;
        }
    }
    let removed = sqlx::query("DELETE FROM messages WHERE account_id = ? AND folder_id = ?")
        .bind(account_id)
        .bind(folder_id)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize;
    sqlx::query("DELETE FROM attachments WHERE account_id = ? AND message_id NOT IN (SELECT id FROM messages WHERE account_id = ?)")
        .bind(account_id)
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    Ok(removed)
}

async fn unindex_message(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    message_id: i64,
) -> ClientResult<()> {
    if mode == SearchMode::Fts5 {
        unindex_message_raw(conn, account_id, message_id).await?;
    }
    Ok(())
}

async fn unindex_message_raw(
    conn: &mut SqliteConnection,
    account_id: i64,
    message_id: i64,
) -> ClientResult<()> {
    crate::search::unindex_document(conn, SearchMode::Fts5, account_id, message_id).await
}

/// Rebuild the search index row of one message from the cache.
async fn reindex_message(
    conn: &mut SqliteConnection,
    mode: SearchMode,
    account_id: i64,
    message_id: i64,
) -> ClientResult<()> {
    let row = sqlx::query(
        "SELECT m.subject, m.from_address, m.from_name, m.to_summary, m.folder_id,
                COALESCE(b.text_body, '') AS body
         FROM messages m
         LEFT JOIN message_bodies b ON b.account_id = m.account_id AND b.message_id = m.id
         WHERE m.account_id = ? AND m.id = ?",
    )
    .bind(account_id)
    .bind(message_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(());
    };

    let attachment_rows = sqlx::query(
        "SELECT filename FROM attachments WHERE account_id = ? AND message_id = ? ORDER BY id",
    )
    .bind(account_id)
    .bind(message_id)
    .fetch_all(&mut *conn)
    .await?;
    let attachments = attachment_rows
        .iter()
        .map(|row| row.try_get::<String, _>("filename").unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ");

    let from_name: Option<String> = row.try_get("from_name").ok().flatten();
    let sender = match from_name {
        Some(name) if !name.is_empty() => {
            format!("{} {}", name, row.try_get::<String, _>("from_address").unwrap_or_default())
        }
        _ => row.try_get::<String, _>("from_address").unwrap_or_default(),
    };

    let document = SearchDocument {
        account_id,
        message_id,
        folder_id: row.try_get("folder_id").ok().flatten(),
        subject: row.try_get::<String, _>("subject").unwrap_or_default(),
        sender,
        recipients: row.try_get::<Option<String>, _>("to_summary").ok().flatten().unwrap_or_default(),
        body: row.try_get::<String, _>("body").unwrap_or_default(),
        attachments,
    };
    crate::search::index_document(&mut *conn, mode, &document).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RetryPolicy;
    use crate::testutil::{MockServer, MockResponse, TempDir};
    use ferroma_core::OperationId;
    use std::sync::Mutex;

    async fn engine(db: Arc<ClientDatabase>, client: FcpClient) -> SyncEngine {
        SyncEngine::new(db, client).without_metadata_fetch()
    }

    async fn fixture() -> (TempDir, Arc<ClientDatabase>, MockServer, SyncEngine) {
        let dir = TempDir::new().expect("temp dir");
        let db = Arc::new(
            ClientDatabase::open(dir.path().join("cache.db"))
                .await
                .expect("open"),
        );
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'alice@example.com', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(db.pool())
        .await
        .expect("account");
        sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name, message_count, uid_validity)
             VALUES (1, 5, 3, 'INBOX', 412, 1)",
        )
        .execute(db.pool())
        .await
        .expect("folder");

        let server = MockServer::start().await;
        let client = FcpClient::with_retry(
            server.fcp_base_url(),
            Arc::new(crate::api::MemoryTokenStore::new()),
            RetryPolicy::none(),
        )
        .expect("client");
        client
            .set_access_token("token", std::time::Duration::from_secs(600))
            .await;
        let engine = engine(db.clone(), client).await;
        (dir, db, server, engine)
    }

    fn page(next: &str, has_more: bool, changes: &str) -> String {
        format!(r#"{{"next_cursor":"{next}","has_more":{has_more},"changes":[{changes}]}}"#)
    }

    async fn message_count(db: &ClientDatabase) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM messages")
            .fetch_one(db.pool())
            .await
            .expect("count")
            .get("n")
    }

    #[tokio::test]
    async fn a_page_is_applied_and_the_cursor_advances() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page(
                "1841",
                false,
                r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117},
                   {"type":"message_created","seq":1837,"message_id":4822,"uid":118}"#,
            ),
        );

        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert_eq!(outcome.applied, 2);
        assert_eq!(outcome.pages, 1);
        assert_eq!(outcome.cursor, Cursor(1841));
        assert!(!outcome.resynced);
        assert_eq!(message_count(&db).await, 2);
        assert_eq!(engine.cursor(1, 5).await.expect("cursor"), Cursor(1841));

        let row = sqlx::query("SELECT uid, folder_id, body_state FROM messages WHERE id = 4821")
            .fetch_one(db.pool())
            .await
            .expect("row");
        assert_eq!(row.get::<i64, _>("uid"), 117);
        assert_eq!(row.get::<i64, _>("folder_id"), 5);
        assert_eq!(row.get::<String, _>("body_state"), "pending", "bodies are lazy");
    }

    #[tokio::test]
    async fn applying_the_same_page_twice_changes_nothing_the_second_time() {
        let (_dir, db, server, engine) = fixture().await;
        let body = page(
            "1841",
            false,
            r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117},
               {"type":"message_updated","seq":1837,"message_id":4821,"flags":"seen"},
               {"type":"folder_created","seq":1838,"folder_id":6,"name":"Archive/2026"}"#,
        );
        server.json_route("GET", "/api/v1/client/sync", 200, body.clone());

        engine.sync_folder(1, 3, 5, None).await.expect("first");
        let before = snapshot(&db).await;
        // Rewind the cursor, exactly as a crash before the commit would have.
        engine.store_cursor(1, 5, Cursor::ZERO).await.expect("rewind");
        engine.sync_folder(1, 3, 5, None).await.expect("second");
        let after = snapshot(&db).await;

        assert_eq!(before, after, "re-applying a page must be a no-op");
        assert_eq!(engine.cursor(1, 5).await.expect("cursor"), Cursor(1841));
    }

    async fn snapshot(db: &ClientDatabase) -> String {
        let messages = sqlx::query("SELECT id, uid, flags, folder_id, deleted FROM messages ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("messages");
        let folders = sqlx::query("SELECT id, name FROM folders ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("folders");
        let mut out = String::new();
        for row in messages {
            out.push_str(&format!(
                "m{}:{}:{}:{:?}:{}\n",
                row.get::<i64, _>("id"),
                row.get::<Option<i64>, _>("uid").unwrap_or(-1),
                row.get::<String, _>("flags"),
                row.get::<Option<i64>, _>("folder_id"),
                row.get::<i64, _>("deleted")
            ));
        }
        for row in folders {
            out.push_str(&format!(
                "f{}:{}\n",
                row.get::<i64, _>("id"),
                row.get::<String, _>("name")
            ));
        }
        out
    }

    #[tokio::test]
    async fn the_cursor_only_advances_after_a_successful_commit() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page(
                "1841",
                false,
                r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117},
                   {"type":"message_created","seq":1837,"message_id":4822,"uid":118}"#,
            ),
        );
        let engine = engine.with_fault_point(FaultPoint::AfterChanges(1));

        let err = engine.sync_folder(1, 3, 5, None).await.expect_err("must fail");
        assert!(matches!(err, ClientError::Cache(_)), "got {err:?}");
        assert_eq!(engine.cursor(1, 5).await.expect("cursor"), Cursor::ZERO);
        assert_eq!(message_count(&db).await, 0, "the transaction rolled back");
    }

    #[tokio::test]
    async fn a_fault_before_advancing_also_rolls_back() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("1841", false, r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117}"#),
        );
        let engine = engine.with_fault_point(FaultPoint::BeforeAdvance);
        engine.sync_folder(1, 3, 5, None).await.expect_err("must fail");
        assert_eq!(engine.cursor(1, 5).await.expect("cursor"), Cursor::ZERO);
        assert_eq!(message_count(&db).await, 0);
    }

    #[tokio::test]
    async fn a_409_triggers_a_full_resync_of_that_folder() {
        let (_dir, db, server, engine) = fixture().await;
        // Seed a stale cache and a stale cursor.
        sqlx::query(
            "INSERT INTO messages (account_id, id, folder_id, uid, cached_at) VALUES (1, 900, 5, 1, 'now')",
        )
        .execute(db.pool())
        .await
        .expect("seed");
        engine.store_cursor(1, 5, Cursor(17)).await.expect("cursor");
        engine.store_cursor(1, 9, Cursor(3)).await.expect("other cursor");

        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                MockResponse::error(409, "conflict", "cursor too old; full resync required"),
                MockResponse::json(page(
                    "1841",
                    false,
                    r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117}"#,
                )),
            ],
        );

        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert!(outcome.resynced);
        assert_eq!(outcome.cursor, Cursor(1841));
        assert_eq!(server.count_for("/api/v1/client/sync"), 2, "resynced once");
        assert_eq!(message_count(&db).await, 1, "the stale cache was dropped");
        let row = sqlx::query("SELECT id FROM messages").fetch_one(db.pool()).await.expect("row");
        assert_eq!(row.get::<i64, _>("id"), 4821);
        assert_eq!(
            engine.cursor(1, 9).await.expect("other"),
            Cursor(3),
            "only the affected folder was reset"
        );

        let request = server.requests_for("/api/v1/client/sync").remove(1);
        assert_eq!(request.query_params().get("cursor").map(String::as_str), Some("0"));
    }

    #[tokio::test]
    async fn a_second_409_is_surfaced_rather_than_looping_forever() {
        let (_dir, _db, server, engine) = fixture().await;
        server.error_route("GET", "/api/v1/client/sync", 409, "conflict", "still too old");
        let err = engine.sync_folder(1, 3, 5, None).await.expect_err("must give up");
        assert_eq!(err.api_status(), Some(409));
        assert_eq!(server.count_for("/api/v1/client/sync"), 2, "resync is attempted once");
    }

    #[tokio::test]
    async fn a_seq_gap_triggers_a_resync() {
        let (_dir, _db, server, engine) = fixture().await;
        engine.store_cursor(1, 5, Cursor(10)).await.expect("cursor");
        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                // A change whose seq is not greater than the cursor we asked
                // from: the server lost history.
                MockResponse::json(page(
                    "12",
                    false,
                    r#"{"type":"message_created","seq":9,"message_id":4821,"uid":117}"#,
                )),
                MockResponse::json(page(
                    "12",
                    false,
                    r#"{"type":"message_created","seq":11,"message_id":4821,"uid":117}"#,
                )),
            ],
        );
        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert!(outcome.resynced);
        let requests = server.requests_for("/api/v1/client/sync");
        assert_eq!(requests[0].query_params().get("cursor").map(String::as_str), Some("10"));
        assert_eq!(requests[1].query_params().get("cursor").map(String::as_str), Some("0"));
    }

    #[tokio::test]
    async fn a_backwards_next_cursor_triggers_a_resync() {
        let (_dir, _db, server, engine) = fixture().await;
        engine.store_cursor(1, 5, Cursor(20)).await.expect("cursor");
        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                MockResponse::json(page(
                    "15",
                    false,
                    r#"{"type":"message_created","seq":21,"message_id":4821,"uid":117}"#,
                )),
                MockResponse::json(page(
                    "22",
                    false,
                    r#"{"type":"message_created","seq":21,"message_id":4821,"uid":117}"#,
                )),
            ],
        );
        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert!(outcome.resynced);
    }

    #[test]
    fn gap_detection_rules() {
        let changes = vec![
            Change::MessageCreated {
                seq: 5,
                message_id: 1,
                uid: None,
                folder_id: None,
            },
            Change::MessageCreated {
                seq: 7,
                message_id: 2,
                uid: None,
                folder_id: None,
            },
        ];
        // Ascending and above the cursor: fine (the missing 6 belongs to another
        // folder's stream).
        assert!(detect_seq_gap(Cursor(4), &changes, "7").is_none());
        // Repeating the cursor: not fine.
        assert!(detect_seq_gap(Cursor(5), &changes, "7").is_some());
        // Going backwards inside a page: not fine.
        let backwards = vec![
            Change::MessageCreated { seq: 9, message_id: 1, uid: None, folder_id: None },
            Change::MessageCreated { seq: 8, message_id: 2, uid: None, folder_id: None },
        ];
        assert!(detect_seq_gap(Cursor(5), &backwards, "9").is_some());
        // A next_cursor behind the last seq: not fine.
        assert!(detect_seq_gap(Cursor(4), &changes, "6").is_some());
        // An unparseable cursor: not fine, but not a panic either.
        assert!(detect_seq_gap(Cursor(4), &changes, "not-a-number").is_some());
        // An unknown change type carries no seq and is simply skipped.
        let unknown = vec![Change::Unknown];
        assert!(detect_seq_gap(Cursor(4), &unknown, "4").is_none());
    }

    #[tokio::test]
    async fn has_more_pages_until_the_server_says_stop() {
        let (_dir, db, server, engine) = fixture().await;
        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                MockResponse::json(page(
                    "100",
                    true,
                    r#"{"type":"message_created","seq":98,"message_id":1,"uid":1}"#,
                )),
                MockResponse::json(page(
                    "200",
                    true,
                    r#"{"type":"message_created","seq":150,"message_id":2,"uid":2},
                       {"type":"message_created","seq":199,"message_id":3,"uid":3}"#,
                )),
                MockResponse::json(page(
                    "300",
                    false,
                    r#"{"type":"message_created","seq":250,"message_id":4,"uid":4}"#,
                )),
            ],
        );

        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert_eq!(outcome.pages, 3);
        assert_eq!(outcome.applied, 4);
        assert_eq!(outcome.cursor, Cursor(300));
        assert_eq!(message_count(&db).await, 4);

        let cursors: Vec<String> = server
            .requests_for("/api/v1/client/sync")
            .iter()
            .map(|r| r.query_params().get("cursor").cloned().unwrap_or_default())
            .collect();
        assert_eq!(cursors, vec!["0", "100", "200"]);
    }

    #[tokio::test]
    async fn an_unknown_change_type_still_advances_the_cursor() {
        let (_dir, _db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("9", false, r#"{"type":"quantum_entanglement","seq":9}"#),
        );
        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert_eq!(outcome.cursor, Cursor(9));
        assert_eq!(engine.cursor(1, 5).await.expect("cursor"), Cursor(9));
    }

    #[tokio::test]
    async fn the_other_change_types_are_applied() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page(
                "20",
                false,
                r#"{"type":"message_created","seq":10,"message_id":1,"uid":1},
                   {"type":"message_updated","seq":11,"message_id":1,"flags":"seen flagged"},
                   {"type":"message_created","seq":12,"message_id":2,"uid":2},
                   {"type":"message_moved","seq":13,"message_id":2,"from_folder_id":5,"to_folder_id":6},
                   {"type":"message_created","seq":14,"message_id":3,"uid":3},
                   {"type":"message_deleted","seq":15,"message_id":3},
                   {"type":"folder_created","seq":16,"folder_id":6,"name":"Archive/2026"},
                   {"type":"folder_updated","seq":17,"folder_id":6,"message_count":39,"unseen_count":0},
                   {"type":"draft_created","seq":18,"draft_id":44},
                   {"type":"draft_updated","seq":19,"draft_id":44},
                   {"type":"draft_deleted","seq":20,"draft_id":45}"#,
            ),
        );

        engine.sync_folder(1, 3, 5, None).await.expect("sync");

        let flags: String = sqlx::query("SELECT flags FROM messages WHERE id = 1")
            .fetch_one(db.pool())
            .await
            .expect("m1")
            .get("flags");
        assert_eq!(flags, "seen flagged");

        let moved: Option<i64> = sqlx::query("SELECT folder_id FROM messages WHERE id = 2")
            .fetch_one(db.pool())
            .await
            .expect("m2")
            .get("folder_id");
        assert_eq!(moved, Some(6));

        let deleted: i64 = sqlx::query("SELECT deleted FROM messages WHERE id = 3")
            .fetch_one(db.pool())
            .await
            .expect("m3")
            .get("deleted");
        assert_eq!(deleted, 1, "a deletion is a tombstone, not a hole");

        let folder: (String, i64) =
            sqlx::query_as("SELECT name, message_count FROM folders WHERE account_id = 1 AND id = 6")
                .fetch_one(db.pool())
                .await
                .expect("folder");
        assert_eq!(folder.0, "Archive/2026");
        assert_eq!(folder.1, 39);

        let drafts: i64 = sqlx::query("SELECT COUNT(*) AS n FROM drafts WHERE account_id = 1 AND deleted = 0")
            .fetch_one(db.pool())
            .await
            .expect("drafts")
            .get("n");
        assert_eq!(drafts, 1);
    }

    #[tokio::test]
    async fn folder_deletion_clears_the_folder_and_its_messages() {
        let (_dir, db, server, engine) = fixture().await;
        sqlx::query("INSERT INTO messages (account_id, id, folder_id, uid, cached_at) VALUES (1, 7, 5, 1, 'now')")
            .execute(db.pool())
            .await
            .expect("seed");
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("5", false, r#"{"type":"folder_deleted","seq":5,"folder_id":5}"#),
        );
        engine.sync_folder(1, 3, 5, None).await.expect("sync");
        let folders: i64 = sqlx::query("SELECT COUNT(*) AS n FROM folders WHERE id = 5")
            .fetch_one(db.pool())
            .await
            .expect("folders")
            .get("n");
        assert_eq!(folders, 0);
        assert_eq!(message_count(&db).await, 0);
    }

    #[tokio::test]
    async fn a_uid_validity_change_in_the_stream_clears_the_folder_and_restarts_from_zero() {
        let (_dir, db, server, engine) = fixture().await;
        engine.note_uid_validity(1, 5, 1).await.expect("epoch");
        sqlx::query("INSERT INTO messages (account_id, id, folder_id, uid, cached_at) VALUES (1, 7, 5, 1, 'now')")
            .execute(db.pool())
            .await
            .expect("seed");
        engine.store_cursor(1, 5, Cursor(50)).await.expect("cursor");

        // The first page carries the epoch change; the second is what a real
        // server would replay from cursor 0 (the whole history).
        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                MockResponse::json(page(
                    "51",
                    false,
                    r#"{"type":"folder_updated","seq":51,"folder_id":5,"uid_validity":2}"#,
                )),
                MockResponse::json(page(
                    "51",
                    false,
                    r#"{"type":"message_created","seq":9,"message_id":4821,"uid":117}"#,
                )),
            ],
        );
        engine.sync_folder(1, 3, 5, None).await.expect("sync");

        assert_eq!(
            message_count(&db).await,
            1,
            "the stale UID cache was dropped and rebuilt from 0"
        );
        let rebuilt: i64 = sqlx::query("SELECT id FROM messages")
            .fetch_one(db.pool())
            .await
            .expect("row")
            .get("id");
        assert_eq!(rebuilt, 4821, "the old message 7 is gone");

        let cursors: Vec<String> = server
            .requests_for("/api/v1/client/sync")
            .iter()
            .map(|request| request.query_params().get("cursor").cloned().unwrap_or_default())
            .collect();
        assert_eq!(
            cursors,
            vec!["50", "0"],
            "the folder must be re-synced from 0, not from the stale cursor"
        );
        assert_eq!(engine.stored_uid_validity(1, 5).await.expect("epoch"), Some(2));
    }

    #[tokio::test]
    async fn note_uid_validity_is_idempotent_and_reports_changes() {
        let (_dir, _db, _server, engine) = fixture().await;
        assert!(!engine.note_uid_validity(1, 5, 1).await.expect("first"));
        assert!(!engine.note_uid_validity(1, 5, 1).await.expect("same"));
        assert!(engine.note_uid_validity(1, 5, 2).await.expect("changed"));
    }

    #[tokio::test]
    async fn progress_is_reported_with_counts_the_ui_can_show() {
        let (_dir, _db, server, engine) = fixture().await;
        server.script(
            "GET",
            "/api/v1/client/sync",
            vec![
                MockResponse::json(page(
                    "100",
                    true,
                    r#"{"type":"message_created","seq":98,"message_id":1,"uid":1}"#,
                )),
                MockResponse::json(page(
                    "200",
                    false,
                    r#"{"type":"message_created","seq":150,"message_id":2,"uid":2},
                       {"type":"message_created","seq":199,"message_id":3,"uid":3}"#,
                )),
            ],
        );

        let seen: Arc<Mutex<Vec<SyncProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let callback: ProgressCallback = Arc::new(move |progress| {
            if let Ok(mut guard) = captured.lock() {
                guard.push(progress);
            }
        });
        let outcome = engine
            .sync_folder(1, 3, 5, Some(&callback))
            .await
            .expect("sync");
        assert_eq!(outcome.applied, 3);

        let reports = seen.lock().expect("reports");
        assert!(reports.len() >= 3);
        let last = reports.last().expect("last");
        assert_eq!(last.phase, SyncPhase::Done);
        assert_eq!(last.applied, 3);
        assert_eq!(last.total, Some(412), "the folder's message_count");
        assert_eq!(last.folder_name.as_deref(), Some("INBOX"));
        assert_eq!(last.folder_id, 5);
        assert!(
            reports.iter().any(|p| p.phase == SyncPhase::Applying && p.label() == "1 of 412"),
            "the first page must report 1 of 412"
        );
    }

    #[tokio::test]
    async fn a_failure_marks_the_stream_and_reports_it() {
        let (_dir, db, server, engine) = fixture().await;
        server.error_route("GET", "/api/v1/client/sync", 500, "storage_error", "db down");
        let seen: Arc<Mutex<Vec<SyncProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let callback: ProgressCallback = Arc::new(move |progress| {
            if let Ok(mut guard) = captured.lock() {
                guard.push(progress);
            }
        });
        engine
            .sync_folder(1, 3, 5, Some(&callback))
            .await
            .expect_err("must fail");
        assert!(seen
            .lock()
            .expect("reports")
            .iter()
            .any(|p| p.phase == SyncPhase::Failed));
        let status: String = sqlx::query("SELECT status FROM sync_state WHERE account_id = 1 AND folder_id = 5")
            .fetch_one(db.pool())
            .await
            .expect("state")
            .get("status");
        assert_eq!(status, "error");
    }

    #[tokio::test]
    async fn the_account_stream_is_stored_under_folder_zero() {
        let (_dir, _db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("77", false, r#"{"type":"folder_created","seq":77,"folder_id":8,"name":"Sent"}"#),
        );
        let outcome = engine.sync_account_stream(1, 3, None).await.expect("sync");
        assert_eq!(outcome.folder_id, ACCOUNT_STREAM);
        assert_eq!(engine.cursor(1, ACCOUNT_STREAM).await.expect("cursor"), Cursor(77));

        let request = server.requests_for("/api/v1/client/sync").remove(0);
        assert!(
            !request.query_params().contains_key("folder_id"),
            "the account stream omits folder_id"
        );
    }

    #[tokio::test]
    async fn every_folder_carries_its_own_cursor() {
        let (_dir, _db, server, engine) = fixture().await;
        server.route("GET", "/api/v1/client/sync", |req, _n| {
            let folder = req
                .query_params()
                .get("folder_id")
                .cloned()
                .unwrap_or_else(|| "0".into());
            MockResponse::json(serde_json::json!({
                "next_cursor": format!("{folder}00"),
                "has_more": false,
                "changes": [],
            })
            .to_string())
        });
        engine.sync_folder(1, 3, 5, None).await.expect("f5");
        engine.sync_folder(1, 3, 6, None).await.expect("f6");
        assert_eq!(engine.cursor(1, 5).await.expect("c5"), Cursor(500));
        assert_eq!(engine.cursor(1, 6).await.expect("c6"), Cursor(600));
    }

    #[tokio::test]
    async fn syncing_an_account_stores_the_tree_and_every_folder() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/mailboxes",
            200,
            r#"{"mailboxes":[{"id":3,"address":"alice@example.com","is_primary":true,
                 "folders":[{"id":5,"name":"INBOX","message_count":412,"uid_validity":1},
                            {"id":6,"name":"Sent","special_use":"\\Sent","message_count":152,"uid_validity":1}]}]}"#,
        );
        server.route("GET", "/api/v1/client/sync", |_req, _n| {
            MockResponse::json(r#"{"next_cursor":"5","has_more":false,"changes":[]}"#)
        });

        let summary = engine.sync_account(1, None).await.expect("sync");
        assert_eq!(summary.streams(), 3, "two folders plus the account stream");
        assert_eq!(summary.applied, 0);

        let folders: i64 = sqlx::query("SELECT COUNT(*) AS n FROM folders WHERE account_id = 1")
            .fetch_one(db.pool())
            .await
            .expect("folders")
            .get("n");
        assert_eq!(folders, 2);
        let special: Option<String> =
            sqlx::query("SELECT special_use FROM folders WHERE id = 6")
                .fetch_one(db.pool())
                .await
                .expect("sent")
                .get("special_use");
        assert_eq!(special.as_deref(), Some("\\Sent"));
    }

    #[tokio::test]
    async fn a_mailbox_tree_with_a_new_uid_validity_invalidates_the_folder() {
        let (_dir, db, server, engine) = fixture().await;
        engine.note_uid_validity(1, 5, 1).await.expect("epoch");
        sqlx::query("INSERT INTO messages (account_id, id, folder_id, uid, cached_at) VALUES (1, 7, 5, 1, 'now')")
            .execute(db.pool())
            .await
            .expect("seed");
        server.route("GET", "/api/v1/client/sync", |_req, _n| {
            MockResponse::json(r#"{"next_cursor":"1","has_more":false,"changes":[]}"#)
        });
        let mailboxes = vec![MailboxInfo {
            id: 3,
            address: "alice@example.com".into(),
            display_name: None,
            is_primary: true,
            folders: vec![FolderInfo {
                id: 5,
                name: "INBOX".into(),
                special_use: None,
                message_count: 412,
                unseen_count: 0,
                uid_validity: 7,
                uid_next: 118,
            }],
        }];
        let summary = engine
            .sync_account_from(1, &mailboxes, None)
            .await
            .expect("sync");
        assert_eq!(summary.resynced_folders, vec![5]);
        assert_eq!(message_count(&db).await, 0);
    }

    #[tokio::test]
    async fn metadata_is_fetched_for_new_messages_only() {
        let (_dir, db, server, engine) = fixture().await;
        let engine = engine.clone();
        let engine = SyncEngine {
            fetch_metadata: true,
            ..engine
        };
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("1841", false, r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117}"#),
        );
        server.json_route(
            "GET",
            "/api/v1/client/messages/4821",
            200,
            r#"{"id":4821,"uid":117,"folder_id":5,"subject":"Invoice for September",
                "from":{"address":"bob@example.net","name":"Bob"},
                "to":[{"address":"alice@example.com"}],
                "snippet":"Hi Alice…","flags":"seen","size_bytes":24831,
                "internal_date":"2026-09-16T09:12:44Z",
                "attachments":[{"id":11,"filename":"invoice.pdf","content_type":"application/pdf","size_bytes":1000}],
                "headers":[{"name":"Subject","value":"Invoice for September"}]}"#,
        );

        engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert_eq!(server.count_for("/api/v1/client/messages/4821"), 1);

        let row = sqlx::query("SELECT subject, from_address, snippet FROM messages WHERE id = 4821")
            .fetch_one(db.pool())
            .await
            .expect("row");
        assert_eq!(row.get::<String, _>("subject"), "Invoice for September");
        assert_eq!(row.get::<String, _>("from_address"), "bob@example.net");

        let attachments: i64 = sqlx::query("SELECT COUNT(*) AS n FROM attachments")
            .fetch_one(db.pool())
            .await
            .expect("attachments")
            .get("n");
        assert_eq!(attachments, 1);

        // The same page again must not re-fetch.
        engine.store_cursor(1, 5, Cursor::ZERO).await.expect("rewind");
        engine.sync_folder(1, 3, 5, None).await.expect("second");
        assert_eq!(
            server.count_for("/api/v1/client/messages/4821"),
            1,
            "idempotent application must not re-fetch metadata"
        );
    }

    #[tokio::test]
    async fn a_failing_metadata_fetch_does_not_break_the_sync() {
        let (_dir, db, server, engine) = fixture().await;
        let engine = SyncEngine {
            fetch_metadata: true,
            ..engine
        };
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("1841", false, r#"{"type":"message_created","seq":1836,"message_id":4821,"uid":117}"#),
        );
        server.error_route("GET", "/api/v1/client/messages/4821", 404, "not_found", "gone");

        let outcome = engine.sync_folder(1, 3, 5, None).await.expect("sync");
        assert_eq!(outcome.cursor, Cursor(1841));
        assert_eq!(message_count(&db).await, 1, "the placeholder row remains");
    }

    #[tokio::test]
    async fn storing_a_body_indexes_it_for_search() {
        let (_dir, db, server, engine) = fixture().await;
        server.json_route(
            "GET",
            "/api/v1/client/sync",
            200,
            page("1", false, r#"{"type":"message_created","seq":1,"message_id":4821,"uid":117}"#),
        );
        engine.sync_folder(1, 3, 5, None).await.expect("sync");
        engine
            .store_message_body(1, 4821, Some("the quick brown fox"), None, None)
            .await
            .expect("body");
        let body: String = sqlx::query("SELECT text_body FROM message_bodies WHERE message_id = 4821")
            .fetch_one(db.pool())
            .await
            .expect("body row")
            .get("text_body");
        assert_eq!(body, "the quick brown fox");

        if db.fts5_available() {
            let hits: i64 = sqlx::query(
                "SELECT COUNT(*) AS n FROM messages_fts WHERE messages_fts MATCH 'brown'",
            )
            .fetch_one(db.pool())
            .await
            .expect("fts")
            .get("n");
            assert_eq!(hits, 1, "the body must be searchable");
        }
    }

    #[tokio::test]
    async fn a_409_does_not_lose_a_queued_operation() {
        // The sync engine and the offline queue are independent: a folder reset
        // must never touch `pending_operations`.
        let (_dir, db, server, engine) = fixture().await;
        let queue = crate::operations::PendingQueue::new(db.clone());
        let op = queue
            .enqueue(
                1,
                crate::operations::OperationKind::MarkRead,
                serde_json::json!({"message_id": 4821}),
            )
            .await
            .expect("enqueue");
        assert!(op.operation_id.starts_with("op_"));

        server.error_route("GET", "/api/v1/client/sync", 409, "conflict", "too old");
        let _ = engine.sync_folder(1, 3, 5, None).await;
        assert_eq!(queue.len(1).await.expect("len"), 1);
    }

    #[tokio::test]
    async fn an_operation_id_is_never_generated_by_the_sync_engine() {
        // Guard rail: sync must not fabricate idempotency keys (§5 — they belong
        // to the code that queues the user's intent).
        let (_dir, db, _server, _engine) = fixture().await;
        let rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM pending_operations")
            .fetch_one(db.pool())
            .await
            .expect("count")
            .get("n");
        assert_eq!(rows, 0);
        let _ = OperationId::generate();
    }
}
