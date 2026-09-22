//! Contacts: addresses an account has corresponded with.

use ferroma_core::UserId;
use sqlx::PgPool;

use crate::models::Contact;
use crate::StorageError;

use super::{limit_of, normalise, not_found, offset_of};

/// Reads and writes [`Contact`] rows.
#[derive(Debug, Clone)]
pub struct ContactsRepository {
    pool: PgPool,
}

impl ContactsRepository {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Remember an address. An address already stored keeps its name, note and flags;
    /// only a missing display name is filled in.
    pub async fn remember(
        &self,
        owner: UserId,
        address: &str,
        display_name: Option<&str>,
    ) -> Result<Contact, StorageError> {
        let address = normalise(address);
        if !address.contains('@') {
            return Err(StorageError::Invalid(format!("{address} is not an address")));
        }
        let name = display_name.map(str::trim).filter(|text| !text.is_empty());
        let row = sqlx::query_as::<_, Contact>(
            "INSERT INTO contacts (user_id, address, display_name)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id, address) DO UPDATE
                SET display_name = COALESCE(contacts.display_name, EXCLUDED.display_name),
                    updated_at = NOW()
             RETURNING *",
        )
        .bind(owner.get())
        .bind(&address)
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Whether mail from this address is delivered to Junk.
    pub async fn is_blocked(&self, owner: UserId, address: &str) -> Result<bool, StorageError> {
        let address = normalise(address);
        let blocked: Option<(bool,)> = sqlx::query_as(
            "SELECT blocked FROM contacts WHERE user_id = $1 AND address = $2",
        )
        .bind(owner.get())
        .bind(&address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(blocked.is_some_and(|(value,)| value))
    }

    /// The owner's contacts, favorites first. `query` matches the address, the name or the note.
    pub async fn list(&self, owner: UserId, query: &str, limit: i64) -> Result<Vec<Contact>, StorageError> {
        let pattern = super::like_pattern(&normalise(query));
        let rows = sqlx::query_as::<_, Contact>(
            "SELECT * FROM contacts
              WHERE user_id = $1
                AND ($2 = '%%' OR address ILIKE $2 OR COALESCE(display_name, '') ILIKE $2
                     OR COALESCE(note, '') ILIKE $2)
              ORDER BY favorite DESC, lower(COALESCE(display_name, address)) ASC
              LIMIT $3",
        )
        .bind(owner.get())
        .bind(&pattern)
        .bind(limit_of(limit))
        .fetch_all(&self.pool)
        .await?;
        let _ = offset_of(0);
        Ok(rows)
    }

    /// Replace the editable fields. The address itself does not change.
    pub async fn update(
        &self,
        owner: UserId,
        id: i64,
        display_name: Option<&str>,
        note: Option<&str>,
        favorite: Option<bool>,
        blocked: Option<bool>,
    ) -> Result<Contact, StorageError> {
        let name = display_name.map(str::trim).filter(|text| !text.is_empty());
        let note_given = note.is_some();
        let note = note.map(str::trim).filter(|text| !text.is_empty());
        let row = sqlx::query_as::<_, Contact>(
            "UPDATE contacts
                SET display_name = CASE WHEN $3 THEN $4 ELSE display_name END,
                    note = CASE WHEN $5 THEN $6 ELSE note END,
                    favorite = COALESCE($7, favorite),
                    blocked = COALESCE($8, blocked),
                    updated_at = NOW()
              WHERE id = $1 AND user_id = $2
              RETURNING *",
        )
        .bind(id)
        .bind(owner.get())
        .bind(display_name.is_some())
        .bind(name)
        .bind(note_given)
        .bind(note)
        .bind(favorite)
        .bind(blocked)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| not_found(format!("contact {id}")))?;
        Ok(row)
    }

    /// Remove one contact. The address is remembered again the next time mail names it.
    pub async fn delete(&self, owner: UserId, id: i64) -> Result<(), StorageError> {
        let done = sqlx::query("DELETE FROM contacts WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(owner.get())
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(not_found(format!("contact {id}")));
        }
        Ok(())
    }
}
