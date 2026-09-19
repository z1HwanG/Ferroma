//! `mailboxes` and `folders`.
//!
//! A *mailbox* is an address (`alice@example.com`) owned by a user; a *folder* is an
//! IMAP mailbox inside it. The schema keeps them apart, and so does this module —
//! [`MailboxesRepository`] answers "does this address exist?", [`FoldersRepository`]
//! answers "what is in it?".

use sqlx::{PgConnection, PgPool};

use ferroma_core::{DomainId, MailboxId, UserId};

use crate::error::{Result, StorageError};
use crate::models::{Folder, Mailbox};
use crate::repository::{normalise, not_found, unique_conflict};

/// Everything needed to create an address.
#[derive(Debug, Clone)]
pub struct NewMailbox {
    /// The owning account.
    pub user_id: UserId,
    /// The domain the address lives in.
    pub domain_id: DomainId,
    /// Address local part. Stored lower-cased.
    pub local_part: String,
    /// Optional human name shown next to the address.
    pub display_name: Option<String>,
    /// Whether this becomes the account's primary address. An existing primary
    /// address of the same user is demoted, because the schema allows only one.
    pub is_primary: bool,
    /// Per-address quota. `None` inherits the owner's quota.
    pub quota_bytes: Option<i64>,
}

/// A mailbox joined with its domain name — what most callers actually want.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxWithDomain {
    /// The address row.
    pub mailbox: Mailbox,
    /// The domain name the address belongs to, e.g. `example.com`.
    pub domain: String,
}

impl MailboxWithDomain {
    /// The full address, e.g. `alice@example.com`.
    pub fn address(&self) -> String {
        self.mailbox.address(&self.domain)
    }
}

/// Row shape for the mailbox/domain join.
#[derive(sqlx::FromRow)]
struct MailboxWithDomainRow {
    #[sqlx(flatten)]
    mailbox: Mailbox,
    domain: String,
}

impl From<MailboxWithDomainRow> for MailboxWithDomain {
    fn from(row: MailboxWithDomainRow) -> Self {
        MailboxWithDomain {
            mailbox: row.mailbox,
            domain: row.domain,
        }
    }
}

/// Addresses — the SMTP delivery targets.
#[derive(Debug, Clone)]
pub struct MailboxesRepository {
    pool: PgPool,
}

impl MailboxesRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create an address.
    ///
    /// The local part is lower-cased. Raising `is_primary` demotes the user's
    /// previous primary address in the same transaction, so the partial unique index
    /// (`mailboxes_primary_key`) is never the thing that fails first. A duplicate
    /// address is a [`StorageError::Conflict`].
    ///
    /// The six standard folders (`INBOX`, `Sent`, `Drafts`, `Trash`, `Junk`,
    /// `Archive`) are provisioned in the same transaction: an address that can
    /// receive mail needs an INBOX before SMTP delivery or IMAP can touch it, and
    /// making that the caller's job is how "no such folder" bugs reach production.
    /// The Maildir itself is the mail store's business, not this repository's.
    pub async fn create(&self, new: NewMailbox) -> Result<Mailbox> {
        let local_part = normalise(&new.local_part);
        if local_part.is_empty() {
            return Err(StorageError::Invalid(
                "mailbox local_part must not be blank".into(),
            ));
        }
        if new.quota_bytes.is_some_and(|q| q < 0) {
            return Err(StorageError::Invalid(
                "mailbox quota_bytes must be >= 0".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;

        if new.is_primary {
            // The schema allows exactly one primary address per user (partial unique
            // index `mailboxes_primary_key`), so demote the incumbent instead of
            // letting the index fail the insert.
            sqlx::query(
                "UPDATE mailboxes SET is_primary = FALSE, updated_at = NOW()
                  WHERE user_id = $1 AND is_primary",
            )
            .bind(new.user_id.get())
            .execute(&mut *tx)
            .await?;
        }

        let mailbox = sqlx::query_as::<_, Mailbox>(
            "INSERT INTO mailboxes (user_id, domain_id, local_part, display_name, is_primary, quota_bytes)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING *",
        )
        .bind(new.user_id.get())
        .bind(new.domain_id.get())
        .bind(&local_part)
        .bind(new.display_name.as_deref())
        .bind(new.is_primary)
        .bind(new.quota_bytes)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("mailbox {local_part}")))?;

        // An address that can receive mail must have an INBOX before SMTP delivery or
        // IMAP can touch it, so the folders are provisioned here, in the same
        // transaction: no caller can ever observe a half-built address, and no caller
        // has to remember to ask for them.
        provision_standard_folders(&mut tx, mailbox.mailbox_id()).await?;

        tx.commit().await?;
        Ok(mailbox)
    }

    /// Look an address up by primary key.
    pub async fn find_by_id(&self, id: MailboxId) -> Result<Option<Mailbox>> {
        Ok(sqlx::query_as::<_, Mailbox>("SELECT * FROM mailboxes WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look an address up by domain name and local part. Case-insensitive.
    ///
    /// Disabled mailboxes are returned; callers decide what a disabled address means
    /// for their protocol (SMTP refuses it, the Admin panel shows it).
    pub async fn find_by_address(&self, domain: &str, local_part: &str) -> Result<Option<Mailbox>> {
        Ok(sqlx::query_as::<_, Mailbox>(
            "SELECT m.*
               FROM mailboxes m
               JOIN domains d ON d.id = m.domain_id
              WHERE d.name = $1 AND m.local_part = $2",
        )
        .bind(normalise(domain))
        .bind(normalise(local_part))
        .fetch_optional(&self.pool)
        .await?)
    }

    /// [`MailboxesRepository::find_by_address`] plus the domain name.
    pub async fn find_by_address_with_domain(
        &self,
        domain: &str,
        local_part: &str,
    ) -> Result<Option<MailboxWithDomain>> {
        let row = sqlx::query_as::<_, MailboxWithDomainRow>(
            "SELECT m.*, d.name AS domain
               FROM mailboxes m
               JOIN domains d ON d.id = m.domain_id
              WHERE d.name = $1 AND m.local_part = $2",
        )
        .bind(normalise(domain))
        .bind(normalise(local_part))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// The user's primary address, if they have one.
    pub async fn find_primary(&self, user_id: UserId) -> Result<Option<Mailbox>> {
        Ok(sqlx::query_as::<_, Mailbox>(
            "SELECT * FROM mailboxes WHERE user_id = $1 AND is_primary ORDER BY id LIMIT 1",
        )
        .bind(user_id.get())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every address of one account, primary first, then by address.
    pub async fn list_by_user(&self, user_id: UserId) -> Result<Vec<Mailbox>> {
        Ok(sqlx::query_as::<_, Mailbox>(
            "SELECT * FROM mailboxes WHERE user_id = $1
              ORDER BY is_primary DESC, local_part ASC, id ASC",
        )
        .bind(user_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// [`MailboxesRepository::list_by_user`] plus each address's domain name.
    pub async fn list_by_user_with_domain(&self, user_id: UserId) -> Result<Vec<MailboxWithDomain>> {
        let rows = sqlx::query_as::<_, MailboxWithDomainRow>(
            "SELECT m.*, d.name AS domain
               FROM mailboxes m
               JOIN domains d ON d.id = m.domain_id
              WHERE m.user_id = $1
              ORDER BY m.is_primary DESC, d.name ASC, m.local_part ASC, m.id ASC",
        )
        .bind(user_id.get())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// [`MailboxesRepository::list_by_user_with_domain`] for several accounts at once.
    ///
    /// `GET /users` prints an address count on every row, so the alternative was one
    /// query per account on the page.
    pub async fn list_by_users_with_domain(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<MailboxWithDomain>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<i64> = user_ids.iter().map(|id| id.get()).collect();
        let rows = sqlx::query_as::<_, MailboxWithDomainRow>(
            "SELECT m.*, d.name AS domain
               FROM mailboxes m
               JOIN domains d ON d.id = m.domain_id
              WHERE m.user_id = ANY($1)
              ORDER BY m.user_id ASC, m.is_primary DESC, d.name ASC, m.local_part ASC, m.id ASC",
        )
        .bind(&raw)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Every address in one domain, alphabetical.
    pub async fn list_by_domain(&self, domain_id: DomainId) -> Result<Vec<Mailbox>> {
        Ok(sqlx::query_as::<_, Mailbox>(
            "SELECT * FROM mailboxes WHERE domain_id = $1 ORDER BY local_part ASC, id ASC",
        )
        .bind(domain_id.get())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Whether an address exists at all (enabled or not).
    pub async fn address_exists(&self, domain: &str, local_part: &str) -> Result<bool> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (
                 SELECT 1 FROM mailboxes m
                   JOIN domains d ON d.id = m.domain_id
                  WHERE d.name = $1 AND m.local_part = $2
             )",
        )
        .bind(normalise(domain))
        .bind(normalise(local_part))
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// Enable or disable one address.
    pub async fn set_enabled(&self, id: MailboxId, enabled: bool) -> Result<()> {
        let done = sqlx::query("UPDATE mailboxes SET enabled = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(enabled)
            .execute(&self.pool)
            .await?;
        touched(done.rows_affected(), id)
    }

    /// Replace the display name (`None` clears it).
    pub async fn set_display_name(&self, id: MailboxId, name: Option<&str>) -> Result<()> {
        let done =
            sqlx::query("UPDATE mailboxes SET display_name = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(name)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Set a per-address quota, or `None` to inherit the owner's again.
    pub async fn set_quota(&self, id: MailboxId, quota_bytes: Option<i64>) -> Result<()> {
        if quota_bytes.is_some_and(|q| q < 0) {
            return Err(StorageError::Invalid(
                "mailbox quota_bytes must be >= 0".into(),
            ));
        }
        let done = sqlx::query("UPDATE mailboxes SET quota_bytes = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(quota_bytes)
            .execute(&self.pool)
            .await?;
        touched(done.rows_affected(), id)
    }

    /// Make this the user's primary address (`true`) or drop the flag (`false`).
    ///
    /// Setting it demotes the previous primary address in the same transaction.
    pub async fn set_primary(&self, id: MailboxId, is_primary: bool) -> Result<()> {
        if !is_primary {
            let done =
                sqlx::query("UPDATE mailboxes SET is_primary = FALSE, updated_at = NOW() WHERE id = $1")
                    .bind(id.get())
                    .execute(&self.pool)
                    .await?;
            return touched(done.rows_affected(), id);
        }

        let mut tx = self.pool.begin().await?;
        let (user_id,): (i64,) = sqlx::query_as("SELECT user_id FROM mailboxes WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| not_found(format!("mailbox {id}")))?;

        sqlx::query(
            "UPDATE mailboxes SET is_primary = FALSE, updated_at = NOW()
              WHERE user_id = $1 AND is_primary AND id <> $2",
        )
        .bind(user_id)
        .bind(id.get())
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE mailboxes SET is_primary = TRUE, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Delete an address. Its folders, messages and aliases cascade away.
    ///
    /// Returns `false` when the address did not exist.
    pub async fn delete(&self, id: MailboxId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM mailboxes WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Effective quota: the mailbox's own, else the owner's.
    pub async fn quota(&self, id: MailboxId) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COALESCE(m.quota_bytes, u.quota_bytes)
               FROM mailboxes m
               JOIN users u ON u.id = m.user_id
              WHERE m.id = $1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(q,)| q)
            .ok_or_else(|| not_found(format!("mailbox {id}")))
    }

    /// Bytes currently stored, summed from the live (not expunged) messages.
    ///
    /// Quota accounting in Ferroma is per *account*: the figure this returns is the
    /// same one [`MailboxesRepository::add_usage`] maintains in `users.used_bytes`,
    /// i.e. the total over every address the mailbox's owner holds.
    pub async fn used_bytes(&self, id: MailboxId) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COALESCE(SUM(ms.size_bytes), 0)::BIGINT
               FROM messages ms
              WHERE ms.expunged_at IS NULL
                AND ms.mailbox_id IN (SELECT id FROM mailboxes WHERE user_id = $1)",
        )
        .bind(self.owner_of(id).await?.get())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(b,)| b).unwrap_or(0))
    }

    /// Add `delta` to the owner's `users.used_bytes` and return the new total
    /// (clamped at `>= 0`).
    pub async fn add_usage(&self, id: MailboxId, delta_bytes: i64) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE users
                SET used_bytes = GREATEST(used_bytes + $2, 0), updated_at = NOW()
              WHERE id = (SELECT user_id FROM mailboxes WHERE id = $1)
              RETURNING used_bytes",
        )
        .bind(id.get())
        .bind(delta_bytes)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(used,)| used)
            .ok_or_else(|| not_found(format!("mailbox {id}")))
    }

    /// Recompute `users.used_bytes` for the mailbox's owner from scratch and store it.
    pub async fn recompute_usage(&self, id: MailboxId) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE users
                SET used_bytes = COALESCE((
                        SELECT SUM(ms.size_bytes)
                          FROM messages ms
                         WHERE ms.expunged_at IS NULL
                           AND ms.mailbox_id IN (SELECT id FROM mailboxes WHERE user_id = users.id)
                    ), 0),
                    updated_at = NOW()
              WHERE id = (SELECT user_id FROM mailboxes WHERE id = $1)
              RETURNING used_bytes",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(used,)| used)
            .ok_or_else(|| not_found(format!("mailbox {id}")))
    }

    /// Refuse the write when storing `needed` more bytes would exceed the quota.
    ///
    /// The check is advisory: between this call and the write another session could
    /// fill the account. Callers that must not overshoot take the same check inside a
    /// transaction that also inserts the message.
    pub async fn check_quota(&self, id: MailboxId, needed: i64) -> Result<()> {
        let limit = self.quota(id).await?;
        let used = self.used_bytes(id).await?;
        if needed > 0 && used + needed > limit {
            return Err(StorageError::QuotaExceeded {
                mailbox_id: id.get(),
                used,
                needed,
                limit,
            });
        }
        Ok(())
    }

    /// The owner of an address, or [`StorageError::NotFound`].
    async fn owner_of(&self, id: MailboxId) -> Result<UserId> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT user_id FROM mailboxes WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(user_id,)| UserId::new(user_id))
            .ok_or_else(|| not_found(format!("mailbox {id}")))
    }
}

/// The six folders every IMAP account starts with, in canonical order.
///
/// `INBOX` carries no `special_use`: RFC 6154 reserves `\Inbox` for a different
/// purpose and clients must treat `INBOX` by name.
const STANDARD_FOLDERS: [(&str, Option<&str>); 6] = [
    ("INBOX", None),
    ("Sent", Some("\\Sent")),
    ("Drafts", Some("\\Drafts")),
    ("Trash", Some("\\Trash")),
    ("Junk", Some("\\Junk")),
    ("Archive", Some("\\Archive")),
];

/// The `special_use` values the schema accepts.
const SPECIAL_USES: [&str; 7] = [
    "\\Sent", "\\Drafts", "\\Trash", "\\Junk", "\\Archive", "\\All", "\\Flagged",
];

/// Order folders the way IMAP clients expect: `INBOX` first, then alphabetical
/// ignoring case, with the id as a stable tie-breaker.
const FOLDER_ORDER: &str = "ORDER BY (CASE WHEN lower(name) = 'inbox' THEN 0 ELSE 1 END), \
                            lower(name) ASC, id ASC";

/// IMAP folders.
///
/// Folder ids and mailbox ids are both `BIGSERIAL`s from different tables; this crate
/// deliberately reuses [`MailboxId`] for a folder primary key, because a folder
/// *is* the IMAP notion of a mailbox and introducing a third id type would only add
/// conversions at every call site.
#[derive(Debug, Clone)]
pub struct FoldersRepository {
    pool: PgPool,
}

impl FoldersRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create a folder inside an address.
    ///
    /// `INBOX` is spelled canonically whatever case it arrives in, because IMAP
    /// treats it case-insensitively while the unique index does not. A duplicate name
    /// is a [`StorageError::Conflict`].
    pub async fn create(
        &self,
        mailbox_id: MailboxId,
        name: &str,
        special_use: Option<&str>,
    ) -> Result<Folder> {
        let name = canonical_folder_name(name);
        if name.is_empty() {
            return Err(StorageError::Invalid(
                "folder name must not be blank".into(),
            ));
        }
        validate_special_use(special_use)?;

        sqlx::query_as::<_, Folder>(
            "INSERT INTO folders (mailbox_id, name, special_use) VALUES ($1, $2, $3) RETURNING *",
        )
        .bind(mailbox_id.get())
        .bind(&name)
        .bind(special_use)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("folder {name}")))
    }

    /// Look a folder up by id.
    pub async fn find_by_id(&self, id: MailboxId) -> Result<Option<Folder>> {
        Ok(sqlx::query_as::<_, Folder>("SELECT * FROM folders WHERE id = $1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look a folder up by name. `INBOX` is matched case-insensitively; every other
    /// name is matched exactly, as IMAP requires.
    pub async fn find_by_name(&self, mailbox_id: MailboxId, name: &str) -> Result<Option<Folder>> {
        if name.trim().eq_ignore_ascii_case("INBOX") {
            return Ok(sqlx::query_as::<_, Folder>(
                "SELECT * FROM folders WHERE mailbox_id = $1 AND lower(name) = 'inbox' LIMIT 1",
            )
            .bind(mailbox_id.get())
            .fetch_optional(&self.pool)
            .await?);
        }

        Ok(
            sqlx::query_as::<_, Folder>(
                "SELECT * FROM folders WHERE mailbox_id = $1 AND name = $2 LIMIT 1",
            )
            .bind(mailbox_id.get())
            .bind(name.trim())
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    /// [`FoldersRepository::find_by_name`], but a missing folder is an error.
    pub async fn require_by_name(&self, mailbox_id: MailboxId, name: &str) -> Result<Folder> {
        self.find_by_name(mailbox_id, name)
            .await?
            .ok_or_else(|| not_found(format!("folder {name}")))
    }

    /// Every folder of an address, `INBOX` first, then case-insensitively alphabetical.
    pub async fn list(&self, mailbox_id: MailboxId) -> Result<Vec<Folder>> {
        let sql = format!("SELECT * FROM folders WHERE mailbox_id = $1 {FOLDER_ORDER}");
        Ok(sqlx::query_as::<_, Folder>(&sql)
            .bind(mailbox_id.get())
            .fetch_all(&self.pool)
            .await?)
    }

    /// The subscribed subset, same order as [`FoldersRepository::list`].
    pub async fn list_subscribed(&self, mailbox_id: MailboxId) -> Result<Vec<Folder>> {
        let sql =
            format!("SELECT * FROM folders WHERE mailbox_id = $1 AND subscribed {FOLDER_ORDER}");
        Ok(sqlx::query_as::<_, Folder>(&sql)
            .bind(mailbox_id.get())
            .fetch_all(&self.pool)
            .await?)
    }

    /// Create `INBOX`/`Sent`/`Drafts`/`Trash`/`Junk`/`Archive` if they are missing,
    /// with their `special_use` markers. Idempotent and safe to race with itself.
    ///
    /// [`MailboxesRepository::create`] already calls this, so an ordinary address has
    /// its folders before any caller sees it; this is the repair path for mailboxes
    /// that predate that guarantee (and the reason the fixture's call is a no-op).
    ///
    /// Returns the six standard folders in canonical order.
    pub async fn ensure_standard(&self, mailbox_id: MailboxId) -> Result<Vec<Folder>> {
        let mut tx = self.pool.begin().await?;
        let folders = provision_standard_folders(&mut tx, mailbox_id).await?;
        tx.commit().await?;
        Ok(folders)
    }

    /// Rename a folder. A name already used in the same address is a
    /// [`StorageError::Conflict`].
    pub async fn rename(&self, id: MailboxId, new_name: &str) -> Result<Folder> {
        let new_name = canonical_folder_name(new_name);
        if new_name.is_empty() {
            return Err(StorageError::Invalid(
                "folder name must not be blank".into(),
            ));
        }

        sqlx::query_as::<_, Folder>(
            "UPDATE folders SET name = $2, updated_at = NOW() WHERE id = $1 RETURNING *",
        )
        .bind(id.get())
        .bind(&new_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| unique_conflict(e.into(), format!("folder {new_name}")))?
        .ok_or_else(|| not_found(format!("folder {id}")))
    }

    /// Subscribe or unsubscribe a folder (IMAP `SUBSCRIBE`/`UNSUBSCRIBE`).
    pub async fn set_subscribed(&self, id: MailboxId, subscribed: bool) -> Result<()> {
        let done = sqlx::query("UPDATE folders SET subscribed = $2, updated_at = NOW() WHERE id = $1")
            .bind(id.get())
            .bind(subscribed)
            .execute(&self.pool)
            .await?;
        touched(done.rows_affected(), id)
    }

    /// Set or clear the `special_use` marker.
    pub async fn set_special_use(&self, id: MailboxId, special_use: Option<&str>) -> Result<()> {
        validate_special_use(special_use)?;
        let done =
            sqlx::query("UPDATE folders SET special_use = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(special_use)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Delete a folder and everything in it. Returns `false` when it did not exist.
    pub async fn delete(&self, id: MailboxId) -> Result<bool> {
        let done = sqlx::query("DELETE FROM folders WHERE id = $1")
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Atomically hand out the next IMAP UID.
    ///
    /// The `UPDATE ... RETURNING uid_next - 1` takes a row lock, so concurrent
    /// callers are serialised by PostgreSQL and every UID is handed out exactly once.
    /// UIDs are never reused, which is what `UIDVALIDITY` promises a client.
    pub async fn allocate_uid(&self, id: MailboxId) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE folders SET uid_next = uid_next + 1, updated_at = NOW()
              WHERE id = $1
              RETURNING uid_next - 1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(uid,)| uid)
            .ok_or_else(|| not_found(format!("folder {id}")))
    }

    /// Atomically bump and return the folder's `highest_modseq` (CONDSTORE).
    pub async fn bump_modseq(&self, id: MailboxId) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE folders SET highest_modseq = highest_modseq + 1, updated_at = NOW()
              WHERE id = $1
              RETURNING highest_modseq",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(modseq,)| modseq)
            .ok_or_else(|| not_found(format!("folder {id}")))
    }

    /// Set `UIDVALIDITY` — done when UIDs are renumbered, never otherwise.
    pub async fn set_uid_validity(&self, id: MailboxId, uid_validity: i64) -> Result<()> {
        let done =
            sqlx::query("UPDATE folders SET uid_validity = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.get())
                .bind(uid_validity)
                .execute(&self.pool)
                .await?;
        touched(done.rows_affected(), id)
    }

    /// Recompute `message_count` / `unseen_count` / `total_bytes` from `messages`.
    ///
    /// A message counts as unseen unless its flag string contains `seen`. Expunged
    /// rows are excluded: they are tombstones, not mail.
    pub async fn recount(&self, id: MailboxId) -> Result<Folder> {
        sqlx::query_as::<_, Folder>(
            "UPDATE folders f
                SET message_count = s.msg_count,
                    unseen_count  = s.unseen,
                    total_bytes   = s.bytes,
                    updated_at    = NOW()
               FROM (
                    SELECT COUNT(*)::INT AS msg_count,
                           COUNT(*) FILTER (
                               WHERE NOT ('seen' = ANY(string_to_array(lower(flags), ' ')))
                           )::INT AS unseen,
                           COALESCE(SUM(size_bytes), 0)::BIGINT AS bytes
                      FROM messages
                     WHERE folder_id = $1 AND expunged_at IS NULL
               ) s
              WHERE f.id = $1
              RETURNING f.*",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| not_found(format!("folder {id}")))
    }
}

/// Spell `INBOX` canonically, and trim everything else.
fn canonical_folder_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Create any of the six standard folders that `mailbox_id` is missing, and return
/// all six in canonical order.
///
/// Runs on the caller's connection, so it can be part of a larger transaction (the
/// one that creates the address, for instance) — either the whole address exists
/// with its folders, or nothing does.
///
/// Two details make it safe to call at any time, from anywhere:
///
/// * each insert is `ON CONFLICT DO NOTHING`, which covers both the
///   `(mailbox_id, name)` index and the partial `(mailbox_id, special_use)` index,
///   so two concurrent provisioners cannot fail each other;
/// * a folder that already carries the name but not the marker (created by hand, or
///   by a version that predates `special_use`) is *adopted* — the existing row keeps
///   its id, because clients may hold that id.
///
/// `INBOX` is looked up case-insensitively: the unique index is case-sensitive, so a
/// legacy row spelled `inbox` must be adopted rather than shadowed by a second
/// `INBOX`. It is adopted under its existing spelling, since IMAP treats the name
/// case-insensitively and renaming it would surprise an open session.
async fn provision_standard_folders(
    conn: &mut PgConnection,
    mailbox_id: MailboxId,
) -> Result<Vec<Folder>> {
    let mut folders = Vec::with_capacity(STANDARD_FOLDERS.len());

    for (name, special_use) in STANDARD_FOLDERS {
        let existing = find_standard_folder(conn, mailbox_id, name).await?;
        let mut folder = match existing {
            Some(folder) => folder,
            None => {
                sqlx::query(
                    "INSERT INTO folders (mailbox_id, name, special_use) VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                )
                .bind(mailbox_id.get())
                .bind(name)
                .bind(special_use)
                .execute(&mut *conn)
                .await?;

                // We either inserted it or lost the race to another provisioner;
                // re-reading covers both.
                find_standard_folder(conn, mailbox_id, name)
                    .await?
                    .ok_or_else(|| not_found(format!("folder {name}")))?
            }
        };

        if folder.special_use.is_none() {
            if let Some(marker) = special_use {
                let adopted: Option<Folder> = sqlx::query_as::<_, Folder>(
                    "UPDATE folders SET special_use = $3, updated_at = NOW()
                      WHERE id = $1
                        AND NOT EXISTS (
                            SELECT 1 FROM folders f2
                             WHERE f2.mailbox_id = $2 AND f2.special_use = $3
                        )
                      RETURNING *",
                )
                .bind(folder.id)
                .bind(mailbox_id.get())
                .bind(marker)
                .fetch_optional(&mut *conn)
                .await?;
                if let Some(adopted) = adopted {
                    folder = adopted;
                }
            }
        }

        folders.push(folder);
    }

    Ok(folders)
}

/// Find one of the standard folders by name, matching `INBOX` case-insensitively.
async fn find_standard_folder(
    conn: &mut PgConnection,
    mailbox_id: MailboxId,
    name: &str,
) -> Result<Option<Folder>> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Ok(sqlx::query_as::<_, Folder>(
            "SELECT * FROM folders
              WHERE mailbox_id = $1 AND lower(name) = 'inbox'
              ORDER BY id ASC LIMIT 1",
        )
        .bind(mailbox_id.get())
        .fetch_optional(&mut *conn)
        .await?);
    }

    Ok(
        sqlx::query_as::<_, Folder>(
            "SELECT * FROM folders WHERE mailbox_id = $1 AND name = $2 ORDER BY id ASC LIMIT 1",
        )
        .bind(mailbox_id.get())
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?,
    )
}

/// Reject a `special_use` the schema's `CHECK` would reject, with a better message.
fn validate_special_use(special_use: Option<&str>) -> Result<()> {
    if let Some(value) = special_use {
        if !SPECIAL_USES.contains(&value) {
            return Err(StorageError::Invalid(format!(
                "unknown special_use {value:?}; expected one of {SPECIAL_USES:?}"
            )));
        }
    }
    Ok(())
}

/// Turn "the `UPDATE` matched no row" into [`StorageError::NotFound`].
fn touched(rows: u64, id: MailboxId) -> Result<()> {
    if rows == 0 {
        return Err(not_found(format!("mailbox {id}")));
    }
    Ok(())
}
