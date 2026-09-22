//! Test support shared by this crate's unit tests.
//!
//! This crate has no direct `sqlx` dependency, so it cannot build a *lazy* pool for a
//! pure-logic test; the tests that need repositories **and** a database live in
//! `tests/` instead, where `ferroma-storage` can be reached through the harness. What
//! is left here is what a unit test genuinely needs without a database: a
//! [`RecordingMailSender`], which stands in for the delivery seam so a test can assert
//! on what *would* have been queued.

use ferroma_core::{FerromaError, MessageId, Result, UserId};
use std::sync::{Arc, Mutex};

use crate::state::{MailSender, SendOutcome};

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

/// One recorded [`MailSender::enqueue`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSend {
    /// Who sent it.
    pub user_id: Option<UserId>,
    /// The stored message.
    pub message_id: MessageId,
    /// The envelope sender.
    pub sender: String,
    /// The envelope recipients.
    pub recipients: Vec<String>,
}

/// A [`MailSender`] that records instead of queueing.
#[derive(Debug, Default)]
pub struct RecordingMailSender {
    sends: Mutex<Vec<RecordedSend>>,
    /// When set, every call fails with this message.
    pub fail_with: Option<String>,
}

impl RecordingMailSender {
    /// A sender that accepts everything.
    pub fn new() -> Self {
        RecordingMailSender::default()
    }

    /// A sender whose every call fails, for exercising the failure path.
    pub fn failing(message: &str) -> Self {
        RecordingMailSender {
            sends: Mutex::new(Vec::new()),
            fail_with: Some(message.to_string()),
        }
    }

    /// Everything recorded so far.
    pub fn recorded(&self) -> Vec<RecordedSend> {
        self.sends
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// How many calls were recorded.
    pub fn call_count(&self) -> usize {
        self.sends.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// Every recipient that was handed over, across all calls.
    pub fn all_recipients(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .flat_map(|send| send.recipients)
            .collect()
    }
}

impl MailSender for RecordingMailSender {
    fn enqueue(
        &self,
        user_id: Option<UserId>,
        message_id: MessageId,
        sender: String,
        recipients: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SendOutcome>> + Send + '_>> {
        Box::pin(async move {
            if let Some(message) = &self.fail_with {
                return Err(FerromaError::Network(message.clone()));
            }
            if let Ok(mut guard) = self.sends.lock() {
                guard.push(RecordedSend {
                    user_id,
                    message_id,
                    sender,
                    recipients: recipients.clone(),
                });
            }
            Ok(SendOutcome {
                queued: recipients.len(),
                recipients,
            })
        })
    }
}

/// A shared recording sender plus a handle to read it back.
pub fn shared_recorder() -> (Arc<RecordingMailSender>, Arc<dyn MailSender>) {
    let recorder = Arc::new(RecordingMailSender::new());
    let erased: Arc<dyn MailSender> = Arc::clone(&recorder) as Arc<dyn MailSender>;
    (recorder, erased)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_recorder_captures_every_call() {
        let (recorder, sender) = shared_recorder();
        let outcome = sender
            .enqueue(
                Some(UserId::new(7)),
                MessageId::new(4821),
                "alice@example.com".into(),
                vec!["bob@example.net".into(), "carol@example.org".into()],
            )
            .await
            .expect("the recorder accepts everything");
        assert_eq!(outcome.queued, 2);
        assert_eq!(recorder.call_count(), 1);
        assert_eq!(
            recorder.all_recipients(),
            vec!["bob@example.net", "carol@example.org"]
        );
        assert_eq!(recorder.recorded()[0].user_id, Some(UserId::new(7)));
        assert_eq!(recorder.recorded()[0].message_id, MessageId::new(4821));
        assert_eq!(recorder.recorded()[0].sender, "alice@example.com");
    }

    #[tokio::test]
    async fn the_failing_recorder_reports_a_temporary_failure() {
        let sender = RecordingMailSender::failing("queue is not running");
        let err = sender
            .enqueue(
                None,
                MessageId::new(1),
                "a@b.c".into(),
                vec!["d@e.f".into()],
            )
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "network_error");
        assert!(err.is_temporary());
        assert_eq!(sender.call_count(), 0);
    }
}
