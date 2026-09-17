//! `docs/api.md` §4.5 — the mail queue and its delivery log.
//!
//! Everything here reads and writes through `ferroma-storage`'s queue repositories, so
//! this module owns no queue semantics of its own: `retry` moves a failed row back to
//! `pending` with `next_attempt_at = NOW()`, which is exactly what the dispatcher
//! claims, and `cancel` refuses a row that has already been delivered or bounced.
//!
//! The `?status=` filter accepts a comma-separated list, because the Admin panel's most
//! useful view is `?status=retry,failed` — "what needs attention".

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferroma_core::{FerromaError, QueueId};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{AdminUser, Pagination};
use crate::routes::admin::domains::audit;
use crate::routes::mail::shapes::{DeliveryAttemptResponse, QueueResponse};
use crate::state::AppState;

/// The statuses a queue row may hold.
pub const QUEUE_STATUSES: [&str; 6] = [
    "pending",
    "delivering",
    "delivered",
    "retry",
    "failed",
    "cancelled",
];

/// The `GET /api/v1/queue` filters.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueueListQuery {
    /// One status, or several separated by commas.
    pub status: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Rows to skip.
    pub offset: Option<i64>,
}

/// A page of queue rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueListResponse {
    /// The rows.
    pub items: Vec<QueueResponse>,
    /// How many matched across every requested status.
    pub total: i64,
    /// The page size used.
    pub limit: i64,
    /// The offset used.
    pub offset: i64,
}

/// One queue row with its attempt history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntryResponse {
    /// The row.
    pub entry: QueueResponse,
    /// Every attempt, oldest first.
    pub attempts: Vec<DeliveryAttemptResponse>,
    /// The stored message's subject, when it still exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The envelope recipient count of the whole message.
    pub recipient_count: usize,
}

/// `GET /api/v1/queue/stats`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStatsResponse {
    /// Waiting for a first attempt.
    pub pending: i64,
    /// Claimed by a worker right now.
    pub delivering: i64,
    /// Waiting for a later attempt.
    pub retry: i64,
    /// Accepted by the remote server.
    pub delivered: i64,
    /// Given up on (bounced).
    pub failed: i64,
    /// Withdrawn before delivery.
    pub cancelled: i64,
    /// Rows that are neither finished nor in flight.
    pub outstanding: i64,
    /// When the next attempt is due, if anything is waiting.
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Parse a `?status=` value into the list of statuses it names.
///
/// Unknown names are refused rather than ignored: a typo that silently returns every
/// row is worse than a `400`.
pub fn parse_statuses(raw: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for piece in raw.split(',') {
        let piece = piece.trim().to_ascii_lowercase();
        if piece.is_empty() {
            continue;
        }
        if !QUEUE_STATUSES.contains(&piece.as_str()) {
            return Err(ApiError::new(FerromaError::Invalid(format!(
                "unknown queue status {piece:?}; expected one of {QUEUE_STATUSES:?}"
            ))));
        }
        if !out.contains(&piece) {
            out.push(piece);
        }
    }
    Ok(out)
}

/// `GET /api/v1/queue`
pub async fn list_queue(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<QueueListQuery>,
) -> Result<Json<QueueListResponse>, ApiError> {
    let pagination = Pagination::clamped(query.limit, query.offset);
    let statuses = parse_statuses(query.status.as_deref())?;

    let mut rows = Vec::new();
    let mut total = 0i64;

    if statuses.is_empty() {
        // No filter: every status, newest first. The repository pages one status at a
        // time, so the merged view is assembled here and paged in memory.
        for status in QUEUE_STATUSES {
            let count = state.repos.queue.count_by_status(status).await?;
            total += count;
            rows.extend(
                state
                    .repos
                    .queue
                    .list_by_status(status, pagination.limit + pagination.offset, 0)
                    .await?,
            );
        }
        rows.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let start = pagination.offset.max(0) as usize;
        rows = rows
            .into_iter()
            .skip(start)
            .take(pagination.limit.max(0) as usize)
            .collect();
    } else {
        for status in &statuses {
            total += state.repos.queue.count_by_status(status).await?;
            rows.extend(
                state
                    .repos
                    .queue
                    .list_by_status(status, pagination.limit + pagination.offset, 0)
                    .await?,
            );
        }
        rows.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let start = pagination.offset.max(0) as usize;
        rows = rows
            .into_iter()
            .skip(start)
            .take(pagination.limit.max(0) as usize)
            .collect();
    }

    Ok(Json(QueueListResponse {
        items: rows.iter().map(QueueResponse::from_row).collect(),
        total,
        limit: pagination.limit,
        offset: pagination.offset,
    }))
}

/// `GET /api/v1/queue/:id` — one entry plus its attempt history.
pub async fn get_queue_entry(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<QueueEntryResponse>, ApiError> {
    let queue_id = QueueId::new(id);
    let entry = state
        .repos
        .queue
        .find_by_id(queue_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("queue entry {id}"))))?;

    let attempts = state.repos.delivery_attempts.list_by_queue(queue_id).await?;
    let subject = state
        .repos
        .messages
        .find_by_id(ferroma_core::MessageId::new(entry.message_id))
        .await?
        .and_then(|message| message.subject);
    let recipient_count = state
        .repos
        .queue
        .list_by_message(ferroma_core::MessageId::new(entry.message_id))
        .await
        .map(|rows| rows.len())
        .unwrap_or(0);

    Ok(Json(QueueEntryResponse {
        entry: QueueResponse::from_row(&entry),
        attempts: attempts.iter().map(DeliveryAttemptResponse::from_row).collect(),
        subject,
        recipient_count,
    }))
}

/// `POST /api/v1/queue/:id/retry` — requeue a failed entry now.
pub async fn retry_queue_entry(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<QueueResponse>, ApiError> {
    let queue_id = QueueId::new(id);
    let entry = state
        .repos
        .queue
        .find_by_id(queue_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("queue entry {id}"))))?;

    if matches!(entry.status.as_str(), "delivered") {
        return Err(ApiError::new(FerromaError::Conflict(
            "this entry has already been delivered".to_string(),
        )));
    }
    if entry.status == "cancelled" {
        return Err(ApiError::new(FerromaError::Conflict(
            "this entry was cancelled; it cannot be retried".to_string(),
        )));
    }

    // `mark_retry` with `now` is exactly what the dispatcher claims. The attempt
    // counter is *not* reset here: `attempts` is the queue's record of what has already
    // happened — wiping it would hide the history an operator is looking at when they
    // press retry. The queue's own attempt budget still applies from where it stands.
    state
        .repos
        .queue
        .mark_retry(
            queue_id,
            chrono::Utc::now(),
            "requeued by an administrator",
            None,
            None,
            None,
        )
        .await?;

    audit(
        &state,
        &admin,
        "queue.retried",
        Some("queue"),
        Some(&id.to_string()),
        serde_json::json!({ "recipient": entry.recipient }),
    )
    .await;

    let updated = state
        .repos
        .queue
        .find_by_id(queue_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("queue entry {id}"))))?;
    Ok(Json(QueueResponse::from_row(&updated)))
}

/// `DELETE /api/v1/queue/:id` — cancel an entry that has not been delivered.
pub async fn cancel_queue_entry(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let queue_id = QueueId::new(id);
    let entry = state
        .repos
        .queue
        .find_by_id(queue_id)
        .await?
        .ok_or_else(|| ApiError::new(FerromaError::NotFound(format!("queue entry {id}"))))?;

    let cancelled = state.repos.queue.cancel(queue_id).await?;
    if !cancelled {
        return Err(ApiError::new(FerromaError::Conflict(format!(
            "queue entry {id} is {}, so it cannot be cancelled",
            entry.status
        ))));
    }

    audit(
        &state,
        &admin,
        "queue.cancelled",
        Some("queue"),
        Some(&id.to_string()),
        serde_json::json!({ "recipient": entry.recipient }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/queue/stats`
pub async fn queue_stats(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<QueueStatsResponse>, ApiError> {
    let stats = state.repos.queue.stats().await?;
    let next_due_at = state.repos.queue.next_due_at().await?;
    Ok(Json(QueueStatsResponse {
        pending: stats.pending,
        delivering: stats.delivering,
        retry: stats.retry,
        delivered: stats.delivered,
        failed: stats.failed,
        cancelled: stats.cancelled,
        outstanding: stats.outstanding(),
        next_due_at,
    }))
}

/// Every queue row for one stored message, oldest first.
pub async fn rows_for_message(
    state: &AppState,
    message_id: ferroma_core::MessageId,
) -> Result<Vec<QueueResponse>, ApiError> {
    Ok(state
        .repos
        .queue
        .list_by_message(message_id)
        .await?
        .iter()
        .map(QueueResponse::from_row)
        .collect())
}

/// Whether a status name is one the schema accepts.
pub fn is_known_status(status: &str) -> bool {
    QUEUE_STATUSES.contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_status_parses() {
        assert_eq!(parse_statuses(Some("retry")).expect("valid"), vec!["retry"]);
        assert_eq!(parse_statuses(Some(" FAILED ")).expect("valid"), vec!["failed"]);
    }

    #[test]
    fn the_admin_panel_filter_parses() {
        assert_eq!(
            parse_statuses(Some("retry,failed")).expect("valid"),
            vec!["retry", "failed"]
        );
        assert_eq!(
            parse_statuses(Some(" pending , delivering ")).expect("valid"),
            vec!["pending", "delivering"]
        );
    }

    #[test]
    fn a_repeated_status_is_deduplicated() {
        assert_eq!(
            parse_statuses(Some("retry,retry,retry")).expect("valid"),
            vec!["retry"]
        );
    }

    #[test]
    fn an_empty_filter_means_every_status() {
        assert!(parse_statuses(None).expect("valid").is_empty());
        assert!(parse_statuses(Some("")).expect("valid").is_empty());
        assert!(parse_statuses(Some("   ")).expect("valid").is_empty());
        assert!(parse_statuses(Some(",,")).expect("valid").is_empty());
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        let err = parse_statuses(Some("retrys")).expect_err("must be refused");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.source.to_string().contains("retrys"));
    }

    #[test]
    fn every_documented_status_is_known() {
        for status in QUEUE_STATUSES {
            assert!(is_known_status(status), "{status}");
            assert_eq!(
                parse_statuses(Some(status)).expect("valid"),
                vec![status.to_string()]
            );
        }
        assert!(!is_known_status("teleported"));
        assert_eq!(QUEUE_STATUSES.len(), 6);
    }

    #[test]
    fn the_stats_shape_carries_the_next_due_instant() {
        let response = QueueStatsResponse {
            pending: 1,
            delivering: 2,
            retry: 3,
            delivered: 4,
            failed: 5,
            cancelled: 6,
            outstanding: 6,
            next_due_at: None,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["pending"], 1);
        assert_eq!(json["outstanding"], 6);
        assert!(json["next_due_at"].is_null());
    }

    #[test]
    fn the_list_shape_is_a_normal_page() {
        let response = QueueListResponse {
            items: Vec::new(),
            total: 3,
            limit: 50,
            offset: 0,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["total"], 3);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        assert_eq!(json["items"], serde_json::json!([]));
    }

    #[test]
    fn the_entry_shape_carries_the_attempt_history() {
        let response = QueueEntryResponse {
            entry: QueueResponse {
                id: 91,
                message_id: 4821,
                user_id: Some(7),
                sender: "alice@example.com".into(),
                recipient: "bob@example.net".into(),
                status: "retry".into(),
                attempts: 2,
                max_attempts: 12,
                next_attempt_at: None,
                last_attempt_at: None,
                delivered_at: None,
                last_error: Some("421 too many connections".into()),
                last_status_code: Some(421),
                last_status_text: None,
                remote_mx: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
            attempts: Vec::new(),
            subject: Some("Invoice".into()),
            recipient_count: 2,
        };
        let json = serde_json::to_value(&response).expect("must serialise");
        assert_eq!(json["entry"]["id"], 91);
        assert_eq!(json["attempts"], serde_json::json!([]));
        assert_eq!(json["subject"], "Invoice");
        assert_eq!(json["recipient_count"], 2);
    }

    #[test]
    fn the_filter_query_is_stable() {
        let query: QueueListQuery = serde_json::from_value(serde_json::json!({
            "status": "retry,failed",
            "limit": 10
        }))
        .expect("must deserialise");
        assert_eq!(query.status.as_deref(), Some("retry,failed"));
        assert_eq!(query.limit, Some(10));
        assert!(query.offset.is_none());
    }
}
