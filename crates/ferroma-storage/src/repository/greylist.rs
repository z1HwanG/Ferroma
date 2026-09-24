//! Greylisting: the triplets a peer has been seen sending from, and when.
//!
//! The decision is deliberately made here rather than in the protocol crate: the
//! same row answers "have I seen this sender talk to this recipient before" and
//! "how long ago", so a second caller cannot answer it differently.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::error::Result;

/// What the peer should be told about one triplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreylistDecision {
    /// The triplet has waited out its delay, or this is a later sighting of one
    /// that already has. The message may proceed.
    Accept,
    /// First sighting: defer once and let the peer queue the message.
    Defer,
}

/// The greylist table.
#[derive(Debug, Clone)]
pub struct GreylistRepository {
    pool: PgPool,
}

impl GreylistRepository {
    /// Build the repository over `pool`.
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Record this sighting and decide whether the triplet may proceed.
    ///
    /// The first sighting is the one that starts the clock and it is never moved:
    /// a peer that retries early must not push its own deadline forward, which is
    /// what would let a persistent client wait forever.
    pub async fn check_and_record(
        &self,
        peer_ip: &str,
        sender: &str,
        recipient: &str,
        delay: Duration,
    ) -> Result<GreylistDecision> {
        let row: (DateTime<Utc>,) = sqlx::query_as(
            "INSERT INTO greylist (peer_ip, sender, recipient)
             VALUES ($1, $2, $3)
             ON CONFLICT (peer_ip, sender, recipient)
             DO UPDATE SET last_seen_at = NOW()
             RETURNING first_seen_at",
        )
        .bind(peer_ip.trim())
        .bind(sender.trim())
        .bind(recipient.trim())
        .fetch_one(&self.pool)
        .await?;

        if row.0 + delay <= Utc::now() {
            Ok(GreylistDecision::Accept)
        } else {
            Ok(GreylistDecision::Defer)
        }
    }

    /// How long ago this triplet was first seen, if it is known.
    pub async fn first_seen(
        &self,
        peer_ip: &str,
        sender: &str,
        recipient: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "SELECT first_seen_at FROM greylist
              WHERE peer_ip = $1 AND sender = $2 AND recipient = $3",
        )
        .bind(peer_ip.trim())
        .bind(sender.trim())
        .bind(recipient.trim())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(seen,)| seen))
    }

    /// Forget triplets last seen before `cutoff`, returning how many went.
    ///
    /// Pruning is the operator's job, like expired tombstones: the row only avoids
    /// a deferral, so removing one costs a single extra `451` rather than any mail.
    pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM greylist WHERE last_seen_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }

    /// How many triplets are remembered.
    pub async fn count(&self) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM greylist")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }
}
