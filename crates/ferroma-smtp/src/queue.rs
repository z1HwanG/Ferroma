//! The outbound queue: a worker pool that turns `mail_queue` rows into attempts,
//! retries and bounces.
//!
//! ```text
//!   poll ─► claim_due(limit) ──► for each row ──► deliver (concurrently, ≤ workers)
//!            (FOR UPDATE                 │
//!             SKIP LOCKED)               ├─ Delivered  → mark_delivered
//!                                        ├─ Temporary  → mark_retry  (while attempts < max)
//!                                        │              mark_failed (when they run out)
//!                                        └─ Permanent  → mark_failed
//!                                                  │
//!                                                  ├─ delivery_attempts row
//!                                                  ├─ delivery.updated event
//!                                                  └─ bounce to the sender
//! ```
//!
//! # Claiming is the race
//!
//! `QueueRepository::claim_due` uses `SELECT … FOR UPDATE SKIP LOCKED`, so several
//! workers (and several processes) can poll at the same instant and every row still
//! goes to exactly one of them. The worker never re-implements that logic.
//!
//! # Retry, or bounce?
//!
//! [`crate::client::DeliveryOutcome`] decides. A `Temporary` outcome retries while
//! `attempts < max_attempts`; a `Permanent` one bounces immediately. The backoff
//! comes from [`ferroma_core::config::QueueConfig::backoff_for_attempt`], which is
//! the single source of truth for the schedule (1m, 5m, 15m, 1h, 6h, 24h).
//!
//! # Shutdown
//!
//! `tokio::sync::watch` rather than a `CancellationToken` (which this workspace does
//! not depend on): the loop stops claiming, finishes the attempts already in flight,
//! and returns. A test can therefore prove that an in-flight delivery completed
//! before `run` returned.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use ferroma_core::{config::Config, EmailAddress, FerromaError, MessageId, QueueId, UserId};
use ferroma_events::event::DeliveryUpdated;
use ferroma_events::{Event, EventBus, EventScope};
use ferroma_mail::MessageBuilder;
use ferroma_storage::models::{Message, QueueEntry};
use ferroma_storage::repository::{NewDeliveryAttempt, NewQueueEntry};
use ferroma_storage::{Maildir, Repositories};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::watch;

use crate::client::{DeliveryOutcome, SmtpClient, SmtpClientConfig};
use crate::delivery::{DeliveryService, INBOX};
use crate::dkim::{DkimKey, DkimSigner};
use crate::mx::{MxHost, MxResolver};

/// The queue settings the worker needs, as a plain view.
///
/// A snapshot rather than a `&Config` so the worker does not hold the whole tree
/// alive and so a test can build one with a two-line schedule.
#[derive(Debug, Clone)]
pub struct QueueConfigView {
    /// A worker's claim batch: how many rows to take per poll.
    pub workers: usize,
    /// Attempts before a message is bounced.
    pub max_attempts: i32,
    /// Backoff schedule in seconds; the last entry repeats.
    pub retry_schedule_secs: Vec<u64>,
    /// How often to look for due rows.
    pub poll_interval: std::time::Duration,
    /// Generate a bounce when delivery finally fails.
    pub bounce_on_failure: bool,
    /// The envelope sender bounces come from.
    pub mailer_daemon: String,
    /// This server's own hostname.
    ///
    /// Recorded as the "remote" host of a delivery that never left the box, so the
    /// Admin queue view shows *where* a message went rather than leaving the field
    /// blank.
    pub hostname: String,
    /// Where outbound mail goes instead of straight to the recipient's MX.
    pub relay: Option<RelayConfig>,
    /// The `[dkim]` block, which decides whether an outgoing message is signed.
    ///
    /// Carried here so the worker needs no extra constructor argument, and so the
    /// signing rules stay next to the delivery path that applies them.
    pub dkim: ferroma_core::config::DkimConfig,
}

/// A relay every (or some) outbound message is handed to.
///
/// The reason this exists: a host whose public IP has no PTR record cannot deliver
/// to Gmail or Microsoft at all. The relay — a hosting provider's submission
/// service, a transactional API, or another host with a proper reverse record —
/// carries the message instead, and this server never has to talk to the recipient.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// The relay's host name: what is resolved, put in `EHLO`, and used as the TLS
    /// server name.
    pub host: String,
    /// The client to talk to it with: port, TLS mode and credentials.
    pub client: SmtpClient,
    /// The `[queue]` block the decision comes from, so the rule for "does this
    /// message go through the relay" has exactly one implementation.
    policy: ferroma_core::config::QueueConfig,
}

impl RelayConfig {
    /// From the configuration, when one is set.
    pub fn from_config(config: &Config) -> Option<RelayConfig> {
        let (host, client) = SmtpClientConfig::for_relay(config)?;
        Some(RelayConfig {
            host,
            client: SmtpClient::new(client),
            policy: config.queue.clone(),
        })
    }

    /// Whether a message with this envelope sender belongs on the relay.
    pub fn applies_to(&self, sender: &str) -> bool {
        self.policy.relays_sender(sender)
    }
}

impl Default for QueueConfigView {
    fn default() -> Self {
        QueueConfigView::from_config(&Config::default())
    }
}

impl QueueConfigView {
    /// From the `[queue]` block.
    pub fn from_config(config: &Config) -> Self {
        QueueConfigView {
            workers: config.queue.workers.max(1),
            max_attempts: config.queue.max_attempts as i32,
            retry_schedule_secs: config.queue.retry_schedule_secs.clone(),
            poll_interval: std::time::Duration::from_secs(config.queue.poll_interval_secs.max(1)),
            bounce_on_failure: config.queue.bounce_on_failure,
            mailer_daemon: format!("MAILER-DAEMON@{}", config.server.hostname),
            hostname: config.server.hostname.clone(),
            relay: RelayConfig::from_config(config),
            dkim: config.dkim.clone(),
        }
    }

    /// Backoff for the attempt that has just failed.
    ///
    /// `attempts` is the value `claim_due` stored, i.e. the 1-based number of the
    /// attempt that just ran: attempt 1 is the first delivery, so its *retry* waits
    /// `retry_schedule_secs[0]` (one minute by default).
    pub fn backoff_for_attempt(&self, attempts: i32) -> ChronoDuration {
        let index = (attempts.max(1) - 1) as usize;
        let secs = match self.retry_schedule_secs.len() {
            0 => 60,
            len => self.retry_schedule_secs[index.min(len - 1)],
        };
        ChronoDuration::seconds(secs.min(i64::MAX as u64) as i64)
    }

    /// Whether another attempt is allowed after `attempts` attempts have run.
    pub fn may_retry(&self, attempts: i32) -> bool {
        attempts < self.max_attempts
    }
}

/// The dispatcher.
pub struct QueueWorker {
    client: SmtpClient,
    resolver: Arc<MxResolver>,
    repos: Repositories,
    maildir: Maildir,
    delivery: Arc<DeliveryService>,
    config: QueueConfigView,
    events: Option<EventBus>,
    /// Set by [`QueueWorkerHandle::shutdown`].
    stop: Arc<AtomicBool>,
    /// How many attempts this worker has made, for `/health` and for tests.
    delivered: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    /// Parsed outbound signer per sender domain, built on first use.
    ///
    /// `None` is a cached "this domain has no usable key", so a misconfiguration is
    /// reported once per domain instead of on every delivery attempt. Deriving an RSA
    /// key from PEM is not free and a queue worker sends many messages per domain.
    signers: Mutex<HashMap<String, Option<DkimSigner>>>,
}

impl std::fmt::Debug for QueueWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueWorker")
            .field("workers", &self.config.workers)
            .field("max_attempts", &self.config.max_attempts)
            .field("poll_interval", &self.config.poll_interval)
            .finish_non_exhaustive()
    }
}

impl QueueWorker {
    /// Build a worker.
    pub fn new(
        client: SmtpClient,
        resolver: Arc<MxResolver>,
        repos: Repositories,
        maildir: Maildir,
        delivery: Arc<DeliveryService>,
        config: QueueConfigView,
    ) -> Self {
        QueueWorker {
            client,
            resolver,
            repos,
            maildir,
            delivery,
            config,
            events: None,
            stop: Arc::new(AtomicBool::new(false)),
            delivered: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
            signers: Mutex::new(HashMap::new()),
        }
    }

    /// Publish `delivery.updated` on this bus.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }

    /// How many recipients this worker has delivered to.
    pub fn delivered_count(&self) -> u64 {
        self.delivered.load(Ordering::Acquire)
    }

    /// How many recipients this worker has given up on.
    pub fn failed_count(&self) -> u64 {
        self.failed.load(Ordering::Acquire)
    }

    /// Build a handle that can stop this worker.
    pub fn handle(&self) -> QueueWorkerHandle {
        QueueWorkerHandle {
            stop: Arc::clone(&self.stop),
        }
    }

    /// Run until the shutdown channel fires (or `stop` is set through a handle).
    ///
    /// Returns when the loop has stopped *and* every attempt already in flight has
    /// finished, so a caller that awaits `run` knows nothing is still writing.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!(
            workers = self.config.workers,
            poll_interval_secs = self.config.poll_interval.as_secs(),
            "queue worker started"
        );

        loop {
            if self.should_stop(&shutdown) {
                break;
            }

            match self.dispatch_batch().await {
                Ok(0) => {}
                Ok(count) => tracing::debug!(count, "queue batch dispatched"),
                Err(e) => tracing::warn!(error = %e, "queue batch failed"),
            }

            if self.should_stop(&shutdown) {
                break;
            }

            // Sleep, but wake immediately if shutdown is requested.
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }

        tracing::info!(
            delivered = self.delivered_count(),
            failed = self.failed_count(),
            "queue worker stopped"
        );
    }

    /// Whether the loop should end.
    fn should_stop(&self, shutdown: &watch::Receiver<bool>) -> bool {
        self.stop.load(Ordering::Acquire) || *shutdown.borrow()
    }

    /// Claim and process one batch. Returns how many rows were claimed.
    ///
    /// Public so a test can drive exactly one round without a timer.
    pub async fn dispatch_batch(&mut self) -> Result<usize, FerromaError> {
        let limit = self.config.workers.max(1) as i64;
        let claimed = self
            .repos
            .queue
            .claim_due(limit)
            .await
            .map_err(FerromaError::storage)?;
        if claimed.is_empty() {
            return Ok(0);
        }
        let count = claimed.len();

        // Bounded concurrency, no barrier: each delivery finishes and records itself
        // independently, so one slow remote server does not hold up the batch.
        let mut in_flight = FuturesUnordered::new();
        for entry in claimed {
            let delivery = self.deliver_entry(entry);
            in_flight.push(delivery);
            // Keep the number of simultaneous attempts at the configured width.
            if in_flight.len() >= self.config.workers.max(1) {
                if let Some(()) = in_flight.next().await {
                    // The per-entry work already logged and recorded everything.
                }
            }
        }
        while in_flight.next().await.is_some() {}

        Ok(count)
    }

    /// Deliver one claimed row and record the outcome.
    ///
    /// Never returns `Err`: every failure is recorded *against the row*, because
    /// losing that record would leave the row stuck in `delivering`.
    pub async fn deliver_entry(&self, entry: QueueEntry) -> () {
        let started = Instant::now();
        let queue_id = entry.queue_id();
        let message_id = MessageId::new(entry.message_id);

        let outcome = self.attempt(&entry, message_id).await;
        let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

        self.record_attempt(&entry, &outcome, duration_ms).await;

        match &outcome {
            DeliveryOutcome::Delivered { .. } => {
                self.delivered.fetch_add(1, Ordering::AcqRel);
                if let Err(e) = self
                    .repos
                    .queue
                    .mark_delivered(
                        queue_id,
                        outcome.host(),
                        outcome.code().map(i32::from),
                        Some(outcome.text()),
                    )
                    .await
                {
                    tracing::warn!(error = %e, queue_id = queue_id.get(), "could not mark delivered");
                }
            }
            DeliveryOutcome::Temporary { .. } if self.config.may_retry(entry.attempts) => {
                let next = Utc::now() + self.config.backoff_for_attempt(entry.attempts);
                if let Err(e) = self
                    .repos
                    .queue
                    .mark_retry(
                        queue_id,
                        next,
                        outcome.text(),
                        outcome.host(),
                        outcome.code().map(i32::from),
                        Some(outcome.text()),
                    )
                    .await
                {
                    tracing::warn!(error = %e, queue_id = queue_id.get(), "could not mark retry");
                }
                tracing::info!(
                    queue_id = queue_id.get(),
                    message_id = message_id.get(),
                    recipient = %entry.recipient,
                    attempts = entry.attempts,
                    next_attempt_at = %next,
                    result = "retry",
                    "delivery deferred"
                );
            }
            _ => {
                self.failed.fetch_add(1, Ordering::AcqRel);
                if let Err(e) = self
                    .repos
                    .queue
                    .mark_failed(
                        queue_id,
                        outcome.text(),
                        outcome.host(),
                        outcome.code().map(i32::from),
                        Some(outcome.text()),
                    )
                    .await
                {
                    tracing::warn!(error = %e, queue_id = queue_id.get(), "could not mark failed");
                }
                tracing::info!(
                    queue_id = queue_id.get(),
                    message_id = message_id.get(),
                    recipient = %entry.recipient,
                    attempts = entry.attempts,
                    result = "failed",
                    "delivery abandoned"
                );
                if self.config.bounce_on_failure {
                    self.bounce(&entry, message_id, &outcome).await;
                }
            }
        }

        self.publish(&entry, message_id, &outcome);
    }

    /// Perform one delivery attempt: read the bytes, resolve MX, talk SMTP.
    async fn attempt(&self, entry: &QueueEntry, message_id: MessageId) -> DeliveryOutcome {
        let message = match self.repos.messages.find_by_id(message_id).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                return DeliveryOutcome::transport_failure(
                    format!("message {message_id} no longer exists"),
                    None,
                )
            }
            Err(e) => {
                return DeliveryOutcome::transport_failure(
                    format!("could not read message {message_id}: {e}"),
                    None,
                )
            }
        };

        let body = match self.maildir.read(&message.storage_path) {
            Ok(body) => body,
            Err(e) => {
                return DeliveryOutcome::transport_failure(
                    format!("could not read the stored message: {e}"),
                    None,
                )
            }
        };

        let recipient_address = entry.recipient.trim();
        let Ok(recipient) = EmailAddress::parse(recipient_address) else {
            // A queue row with an unparseable recipient can never succeed.
            return DeliveryOutcome::Permanent {
                code: None,
                text: format!("recipient address is not parseable: {recipient_address}"),
                host: None,
            };
        };

        // A recipient this server hosts never leaves the box. Resolving its MX would
        // point at this very host — or, on a domain with no MX record of its own, at
        // nothing at all — so the message would sit in the queue, exhaust its retries
        // and bounce, and a local user would never receive mail another local user
        // sent them. Delivery goes through the same local path the inbound SMTP
        // listener uses, so quota, the sync journal and `mail.received` all behave
        // identically no matter how the message arrived.
        if self
            .delivery
            .is_local_domain(recipient.domain())
            .await
            .unwrap_or(false)
        {
            return self.deliver_locally(entry, &recipient, &body).await;
        }

        // Past that branch the message leaves this server, which is where an outbound
        // signature belongs: a mailbox we host has no use for one, and a signature is
        // only meaningful to the remote MTA that receives the message.
        let body = self.signed_body(&entry.sender, body).await;

        // Where the message goes: the recipient's MX, or the relay when one is
        // configured and this sender belongs on it. A relay skips MX resolution
        // entirely — that is the whole point of it, since a host with no reverse
        // record is the one that cannot deliver directly.
        let (client, targets) = match self
            .config
            .relay
            .as_ref()
            .filter(|relay| relay.applies_to(&entry.sender))
        {
            Some(relay) => {
                let addresses = match self.resolver.addresses(&relay.host).await {
                    Ok(addresses) => addresses,
                    Err(e) => {
                        return DeliveryOutcome::transport_failure(
                            format!("cannot resolve the relay {}: {e}", relay.host),
                            Some(&relay.host),
                        )
                    }
                };
                if addresses.is_empty() {
                    return DeliveryOutcome::transport_failure(
                        format!("the relay {} has no address", relay.host),
                        Some(&relay.host),
                    );
                }
                (
                    &relay.client,
                    vec![(MxHost::new(0, relay.host.clone()), addresses)],
                )            }
            None => {
                let targets = match self.resolver.delivery_targets(recipient.domain()).await {
                    Ok(targets) => targets,
                    Err(e) => {
                        return DeliveryOutcome::transport_failure(
                            format!("MX lookup failed: {e}"),
                            None,
                        )
                    }
                };
                if targets.is_empty() {
                    return DeliveryOutcome::transport_failure(
                        format!("{} has no mail exchanger", recipient.domain()),
                        None,
                    );
                }
                (&self.client, targets)
            }
        };

        // Preference order, first host that answers wins. A temporary reply from one
        // host moves on to the next; a permanent one is the answer.
        let mut last: Option<DeliveryOutcome> = None;
        for (host, addresses) in targets {
            let outcome = client
                .deliver(&host, &addresses, &entry.sender, &entry.recipient, &body)
                .await;
            match outcome {
                DeliveryOutcome::Temporary { .. } => last = Some(outcome),
                other => return other,
            }
        }
        last.unwrap_or_else(|| {
            DeliveryOutcome::transport_failure("no mail exchanger answered", None)
        })
    }

    /// Borrow the signer cache, recovering from a poisoned lock.
    ///
    /// The cache is a pure optimisation, so a panic elsewhere must not stop this worker
    /// from delivering mail.
    fn signers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<DkimSigner>>> {
        self.signers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sign an outgoing message for its sender's domain, when signing is configured.
    ///
    /// **Best-effort by design.** A key that cannot be read or parsed is logged and the
    /// message is sent unsigned, because the queue exists to get mail delivered and
    /// refusing to send would lose it outright. The consequence is that a missing or
    /// broken key is visible only in the log — `WARN` with the domain, once per domain
    /// per process, not once per message. `ferroma doctor` and the Admin TLS/DKIM panel
    /// are where an operator should be able to see it before mail goes out unsigned;
    /// neither checks outbound signing yet.
    ///
    /// The signature's `d=` is the *sender's* domain, so that is what selects the key
    /// and what the operator's single-domain filter is matched against.
    async fn signed_body(&self, sender: &str, body: Vec<u8>) -> Vec<u8> {
        if !self.config.dkim.enabled {
            return body;
        }
        let Ok(address) = EmailAddress::parse(sender.trim()) else {
            return body;
        };
        let domain = address.domain().to_ascii_lowercase();
        if !signing_requested(&self.config.dkim, &domain) {
            return body;
        }

        // Fast path: the signer for this domain has already been built. The lock is held
        // across the signature because RSA signing is CPU-bound and brief, and it never
        // awaits.
        {
            let cache = self.signers();
            if let Some(entry) = cache.get(&domain) {
                return match entry {
                    Some(signer) => signer.sign_message(&body).unwrap_or_else(|e| {
                        tracing::warn!(domain = %domain, error = %e, "DKIM signing failed; sending unsigned");
                        body
                    }),
                    None => body,
                };
            }
        }

        let fallback_selector = self.config.dkim.selector.clone();
        let Some((pem, selector)) = self.signing_key(&domain, &fallback_selector).await else {
            tracing::warn!(
                domain = %domain,
                "DKIM signing is enabled, but no private key is available for this sender \
                 domain, so outbound mail from it will leave unsigned"
            );
            self.cache_signer(&domain, None);
            return body;
        };

        let built = DkimKey::from_pem(&pem).and_then(|key| {
            DkimSigner::from_key(
                key.with_domain(domain.clone()).with_selector(selector),
                &self.config.dkim,
            )
        });
        let signer = match built {
            Ok(signer) => signer,
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "the DKIM key could not be used; sending unsigned");
                self.cache_signer(&domain, None);
                return body;
            }
        };

        let signed = signer.sign_message(&body);
        self.cache_signer(&domain, Some(signer));
        match signed {
            Ok(signed) => signed,
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "DKIM signing failed; sending unsigned");
                body
            }
        }
    }

    /// The PEM and selector to sign `domain` with.
    ///
    /// The per-domain key the Admin panel manages wins; `[dkim] private_key_path` is the
    /// fallback for a deployment that keeps one key in a file. A domain this server does
    /// not host yields `None`: signing as a domain we do not own produces a signature
    /// that cannot verify, which is worse than no signature at all.
    async fn signing_key(&self, domain: &str, fallback_selector: &str) -> Option<(String, String)> {
        match self.repos.domains.find_by_name(domain).await {
            Ok(Some(row)) => {
                let selector = row
                    .dkim_selector
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| fallback_selector.to_string());
                match row.dkim_private_key.filter(|key| !key.trim().is_empty()) {
                    Some(pem) => Some((pem, selector)),
                    None => self.key_from_file(fallback_selector),
                }
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "could not read this domain's DKIM settings");
                None
            }
        }
    }

    /// Read `[dkim] private_key_path`, when one is configured and readable.
    fn key_from_file(&self, selector: &str) -> Option<(String, String)> {
        let path = self.config.dkim.private_key_path.as_ref()?;
        match std::fs::read_to_string(path) {
            Ok(pem) => Some((pem, selector.to_string())),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "the DKIM private key file could not be read");
                None
            }
        }
    }

    /// Remember the signer built for `domain`, including the negative result.
    fn cache_signer(&self, domain: &str, signer: Option<DkimSigner>) {
        self.signers().insert(domain.to_string(), signer);
    }

    /// Deliver to a mailbox this server hosts.
    ///
    /// The failure split follows RFC 3463, and matches what the inbound SMTP path
    /// tells a peer: an address that does not exist here is permanent, while a full
    /// mailbox and a storage hiccup are temporary. Retrying a full mailbox is the
    /// point — its owner may delete something — whereas bouncing mail that would have
    /// been delivered tomorrow is how a queue loses messages.
    async fn deliver_locally(
        &self,
        entry: &QueueEntry,
        recipient: &EmailAddress,
        body: &[u8],
    ) -> DeliveryOutcome {
        // A null sender (`<>`, a bounce) has no address to record as the sender, which
        // `deliver_raw` already models as `None`.
        let sender = EmailAddress::parse(entry.sender.trim()).ok();
        match self
            .delivery
            .deliver_raw(sender.as_ref(), recipient, INBOX, body)
            .await
        {
            Ok(_) => DeliveryOutcome::Delivered {
                // No SMTP reply was involved: this never went over a wire.
                code: None,
                text: format!("delivered to the local mailbox {recipient}"),
                host: self.config.hostname.clone(),
            },
            Err(FerromaError::NotFound(message)) => DeliveryOutcome::Permanent {
                // The address does not resolve here, and no amount of retrying will
                // change that — the same verdict the inbound listener gives a peer.
                code: None,
                text: message,
                host: None,
            },
            Err(FerromaError::MailboxFull(message)) => DeliveryOutcome::Temporary {
                code: None,
                text: format!("mailbox full: {message}"),
                host: None,
            },
            Err(e) => DeliveryOutcome::transport_failure(
                format!("local delivery to {recipient} failed: {e}"),
                None,
            ),
        }
    }

    /// Append the `delivery_attempts` row.
    async fn record_attempt(&self, entry: &QueueEntry, outcome: &DeliveryOutcome, duration_ms: i32) {
        let new = NewDeliveryAttempt {
            queue_id: entry.queue_id(),
            attempt: entry.attempts,
            remote_mx: outcome.host().map(str::to_string),
            status_code: outcome.code().map(i32::from),
            status_text: Some(outcome.text().to_string()),
            error: match outcome {
                DeliveryOutcome::Delivered { .. } => None,
                _ => Some(outcome.text().to_string()),
            },
            duration_ms: Some(duration_ms),
        };
        if let Err(e) = self.repos.delivery_attempts.record(new).await {
            // The attempt log is bookkeeping; failing to write it must not turn a
            // delivery into a failure.
            tracing::warn!(error = %e, "could not record the delivery attempt");
        }
    }

    /// Publish `delivery.updated`.
    fn publish(&self, entry: &QueueEntry, message_id: MessageId, outcome: &DeliveryOutcome) {
        let Some(bus) = &self.events else {
            return;
        };
        let scope = match entry.user_id {
            Some(user_id) => EventScope::User(UserId::new(user_id)),
            None => EventScope::System,
        };
        let event = Event::DeliveryUpdated(DeliveryUpdated {
            queue_id: entry.queue_id(),
            message_id,
            recipient: entry.recipient.clone(),
            status: outcome.status().to_string(),
            attempts: entry.attempts.max(0) as u32,
            last_error: match outcome {
                DeliveryOutcome::Delivered { .. } => None,
                _ => Some(outcome.text().to_string()),
            },
        });
        bus.publish_nowait(scope, event);
    }

    /// Send a bounce to the envelope sender.
    ///
    /// The bounce goes through the *local* delivery path, so it lands in the
    /// sender's `INBOX` with the same quota accounting, sync-journal entry and
    /// `mail.received` event as any other message. A null sender gets no bounce —
    /// there is nowhere to send it, and bouncing a bounce is how mail loops start.
    async fn bounce(&self, entry: &QueueEntry, message_id: MessageId, outcome: &DeliveryOutcome) {
        let sender = entry.sender.trim();
        if sender.is_empty() || sender == "<>" {
            return;
        }
        let Ok(sender_address) = EmailAddress::parse(sender) else {
            tracing::warn!(queue_id = entry.queue_id().get(), "cannot bounce: the sender is unparseable");
            return;
        };

        let subject = format!("Undelivered Mail Returned to Sender: {}", entry.recipient);
        let text = format!(
            "This is the mail system at {}.\n\n\
             I'm sorry to have to inform you that your message could not\n\
             be delivered to one or more recipients.\n\n\
             <{}>: {}\n\n\
             The message was queued as {} (stored message {}).\n\
             No further attempts will be made.\n",
            self.config.mailer_daemon.split('@').nth(1).unwrap_or("this host"),
            entry.recipient,
            outcome.text(),
            entry.queue_id(),
            message_id,
        );

        let bytes = match MessageBuilder::new()
            .from(&self.config.mailer_daemon)
            .to(sender)
            .subject(&subject)
            .text(&text)
            .domain(
                self.config
                    .mailer_daemon
                    .split('@')
                    .nth(1)
                    .unwrap_or("localhost"),
            )
            .build()
        {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "could not build the bounce message");
                return;
            }
        };

        match self
            .delivery
            .deliver_raw(None, &sender_address, INBOX, &bytes)
            .await
        {
            Ok(bounce_message_id) => tracing::info!(
                queue_id = entry.queue_id().get(),
                bounce_message_id = bounce_message_id.get(),
                recipient = %entry.recipient,
                "bounce delivered to the sender"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                recipient = %entry.recipient,
                "could not deliver the bounce"
            ),
        }
    }
}

/// A handle that stops a running [`QueueWorker`].
#[derive(Debug, Clone)]
pub struct QueueWorkerHandle {
    stop: Arc<AtomicBool>,
}

impl QueueWorkerHandle {
    /// Stop claiming new work. Attempts already in flight finish.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Whether shutdown has been requested.
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Queue one recipient for delivery.
///
/// A free function rather than a method so the API layer and the queue share one
/// definition of "what makes a queue row".
pub async fn enqueue(
    repos: &Repositories,
    message_id: MessageId,
    user_id: Option<UserId>,
    sender: &str,
    recipient: &str,
    max_attempts: i32,
) -> Result<QueueId, FerromaError> {
    let entry = repos
        .queue
        .enqueue(NewQueueEntry {
            message_id,
            user_id,
            sender: sender.to_string(),
            recipient: recipient.to_string(),
            max_attempts,
        })
        .await
        .map_err(FerromaError::storage)?;
    Ok(entry.queue_id())
}

/// The message a queue row points at, for callers that need to render it.
pub async fn load_message(
    repos: &Repositories,
    message_id: MessageId,
) -> Result<Option<Message>, FerromaError> {
    repos
        .messages
        .find_by_id(message_id)
        .await
        .map_err(FerromaError::storage)
}

/// The timestamp a retry is due at, for a caller that wants to display it.
pub fn next_attempt_at(config: &QueueConfigView, attempts: i32, now: DateTime<Utc>) -> DateTime<Utc> {
    now + config.backoff_for_attempt(attempts)
}

/// Whether `[dkim]` asks for a signature over mail from `domain`.
///
/// `enabled` alone signs every local domain; `domain` narrows it to one. Both sides
/// come from configuration and from a parsed address, so the comparison ignores case
/// and surrounding whitespace — `Example.COM` and `example.com` are one domain.
fn signing_requested(config: &ferroma_core::config::DkimConfig, domain: &str) -> bool {
    if !config.enabled {
        return false;
    }
    match config.domain.as_deref().map(str::trim).filter(|only| !only.is_empty()) {
        Some(only) => only.eq_ignore_ascii_case(domain.trim()),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> QueueConfigView {
        QueueConfigView {
            workers: 2,
            max_attempts: 5,
            retry_schedule_secs: vec![60, 300, 900, 3600, 21_600, 86_400],
            poll_interval: std::time::Duration::from_millis(10),
            bounce_on_failure: true,
            mailer_daemon: "MAILER-DAEMON@mx.example.com".to_string(),
            hostname: "mx.example.com".to_string(),
            relay: None,
            dkim: ferroma_core::config::DkimConfig::default(),
        }
    }

    // ------------------------------------------------------------------
    // Backoff
    // ------------------------------------------------------------------

    #[test]
    fn the_relay_travels_from_the_configuration_into_the_worker_view() {
        let mut config = Config::default();
        config.queue.workers = 3;
        assert!(QueueConfigView::from_config(&config).relay.is_none());

        config.queue.relay_host = Some("smtp.relay.example".into());
        config.queue.relay_port = 2525;
        config.queue.relay_tls = "starttls".into();
        config.queue.relay_username = Some("apikey".into());
        config.queue.relay_password = Some("s3cret".into());
        config.queue.relay_from_domains = vec!["z1hwang.cn".into()];

        let view = QueueConfigView::from_config(&config);
        let relay = view.relay.as_ref().expect("a relay");
        assert_eq!(relay.host, "smtp.relay.example");
        assert_eq!(relay.client.config().port, 2525);
        // The decision about *which* messages use it comes from the one
        // implementation, in the core configuration.
        assert!(relay.applies_to("alice@z1hwang.cn"));
        assert!(!relay.applies_to("alice@elsewhere.example"));
    }

    #[test]
    fn the_backoff_follows_the_schedule() {
        let config = view();
        // The attempt number is 1-based and counts the attempt that just ran.
        assert_eq!(config.backoff_for_attempt(1), ChronoDuration::seconds(60));
        assert_eq!(config.backoff_for_attempt(2), ChronoDuration::seconds(300));
        assert_eq!(config.backoff_for_attempt(3), ChronoDuration::seconds(900));
        assert_eq!(config.backoff_for_attempt(4), ChronoDuration::seconds(3600));
        assert_eq!(config.backoff_for_attempt(5), ChronoDuration::seconds(21_600));
    }

    #[test]
    fn the_backoff_saturates_at_the_end_of_the_schedule() {
        let config = view();
        assert_eq!(config.backoff_for_attempt(6), ChronoDuration::seconds(86_400));
        assert_eq!(config.backoff_for_attempt(99), ChronoDuration::seconds(86_400));
        assert_eq!(config.backoff_for_attempt(i32::MAX), ChronoDuration::seconds(86_400));
    }

    #[test]
    fn a_zero_attempt_number_still_gets_the_first_delay() {
        let config = view();
        assert_eq!(config.backoff_for_attempt(0), ChronoDuration::seconds(60));
        assert_eq!(config.backoff_for_attempt(-5), ChronoDuration::seconds(60));
    }

    #[test]
    fn an_empty_schedule_falls_back_to_one_minute() {
        let config = QueueConfigView {
            retry_schedule_secs: Vec::new(),
            ..view()
        };
        assert_eq!(config.backoff_for_attempt(1), ChronoDuration::seconds(60));
        assert_eq!(config.backoff_for_attempt(50), ChronoDuration::seconds(60));
    }

    #[test]
    fn the_default_schedule_matches_the_specification() {
        let config = QueueConfigView::default();
        assert_eq!(
            config.retry_schedule_secs,
            vec![60, 300, 900, 3600, 21_600, 86_400]
        );
        assert_eq!(config.max_attempts, 12);
        assert_eq!(config.workers, 4);
    }

    #[test]
    fn a_retry_is_allowed_only_while_attempts_remain() {
        let config = view();
        assert!(config.may_retry(1));
        assert!(config.may_retry(4));
        assert!(!config.may_retry(5), "the fifth attempt is the last");
        assert!(!config.may_retry(6));
    }

    #[test]
    fn the_retry_schedule_is_strictly_increasing() {
        let config = view();
        for pair in config.retry_schedule_secs.windows(2) {
            assert!(pair[1] > pair[0], "the schedule must never get shorter: {pair:?}");
        }
    }

    #[test]
    fn next_attempt_at_is_now_plus_the_backoff() {
        let config = view();
        let now = Utc::now();
        assert_eq!(next_attempt_at(&config, 1, now), now + ChronoDuration::seconds(60));
        assert_eq!(
            next_attempt_at(&config, 3, now),
            now + ChronoDuration::seconds(900)
        );
    }

    // ------------------------------------------------------------------
    // Config view
    // ------------------------------------------------------------------

    #[test]
    fn the_view_is_built_from_the_config_tree() {
        let mut tree = Config::default();
        tree.queue.workers = 7;
        tree.queue.max_attempts = 3;
        tree.queue.poll_interval_secs = 5;
        tree.queue.bounce_on_failure = false;
        tree.server.hostname = "mx.example.com".to_string();
        let view = QueueConfigView::from_config(&tree);
        assert_eq!(view.workers, 7);
        assert_eq!(view.max_attempts, 3);
        assert_eq!(view.poll_interval, std::time::Duration::from_secs(5));
        assert!(!view.bounce_on_failure);
        assert_eq!(view.mailer_daemon, "MAILER-DAEMON@mx.example.com");
    }

    #[test]
    fn a_zero_worker_count_is_clamped_to_one() {
        let mut tree = Config::default();
        tree.queue.workers = 0;
        tree.queue.poll_interval_secs = 0;
        let view = QueueConfigView::from_config(&tree);
        assert_eq!(view.workers, 1);
        assert_eq!(view.poll_interval, std::time::Duration::from_secs(1));
    }

    #[test]
    fn the_handle_stops_the_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = QueueWorkerHandle {
            stop: Arc::clone(&stop),
        };
        assert!(!handle.is_stopping());
        assert!(!stop.load(Ordering::Acquire));
        handle.shutdown();
        assert!(handle.is_stopping());
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn the_handle_is_cheap_to_clone_and_shares_state() {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = QueueWorkerHandle {
            stop: Arc::clone(&stop),
        };
        let clone = handle.clone();
        clone.shutdown();
        assert!(handle.is_stopping());
    }
}

/// Outbound signing: the policy table, and a sign → verify round trip over exactly the
/// signer the delivery path builds.
///
/// The signer itself is covered in `dkim.rs`; what these pin is the *wiring* — that
/// enabling the switch signs, that the single-domain filter narrows it, and that the
/// signer the queue constructs signs as the sender's own domain and selector.
#[cfg(test)]
mod signing_tests {
    use super::*;
    use crate::dkim::{DkimResult, DkimVerifier};
    use crate::mx::{MockResolver, Resolver};
    use ferroma_core::config::DkimConfig;

    const DOMAIN: &str = "z1hwang.cn";
    const SELECTOR: &str = "default";

    /// Carries every header `DkimConfig::default()` names that a verifier needs.
    const MESSAGE: &[u8] = b"From: Alice <alice@z1hwang.cn>\r\n\
To: Bob <bob@example.net>\r\n\
Subject: signed\r\n\
Message-ID: <1@z1hwang.cn>\r\n\
Date: Tue, 16 Sep 2026 12:00:00 +0000\r\n\
\r\n\
body\r\n";

    fn config(enabled: bool, domain: Option<&str>) -> DkimConfig {
        DkimConfig {
            enabled,
            selector: SELECTOR.to_string(),
            domain: domain.map(str::to_string),
            ..DkimConfig::default()
        }
    }

    /// A key generated the way `ferroma dkim generate` generates one.
    fn generated_pem() -> String {
        use rsa::pkcs8::{EncodePrivateKey, LineEnding};
        let mut rng = rand::thread_rng();
        let key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("an RSA key");
        key.to_pkcs8_pem(LineEnding::LF)
            .expect("PKCS#8 encoding")
            .to_string()
    }

    #[test]
    fn nothing_is_signed_while_signing_is_switched_off() {
        assert!(!signing_requested(&config(false, None), DOMAIN));
        assert!(!signing_requested(&config(false, Some(DOMAIN)), DOMAIN));
    }

    #[test]
    fn enabling_signing_covers_every_local_domain() {
        assert!(signing_requested(&config(true, None), DOMAIN));
        assert!(signing_requested(&config(true, None), "example.net"));
    }

    #[test]
    fn a_single_domain_filter_narrows_signing_and_ignores_case() {
        assert!(signing_requested(&config(true, Some(DOMAIN)), DOMAIN));
        assert!(signing_requested(&config(true, Some(" Z1Hwang.CN ")), DOMAIN));
        assert!(!signing_requested(&config(true, Some("example.net")), DOMAIN));
        // A blank filter is not a filter: treating it as one would match nothing and
        // silently stop all signing, which is the failure this whole change is about.
        assert!(signing_requested(&config(true, Some("   ")), DOMAIN));
    }

    /// This is the assertion that fails if the delivery path stops signing, signs as the
    /// wrong domain, or uses the wrong selector.
    #[tokio::test]
    async fn the_signer_the_queue_builds_produces_a_verifiable_signature() {
        let parsed = DkimKey::from_pem(&generated_pem()).expect("the generated key parses");
        let published = parsed.dns_record(SELECTOR);
        let signer = DkimSigner::from_key(
            parsed.with_domain(DOMAIN).with_selector(SELECTOR),
            &config(true, None),
        )
        .expect("a signer");

        let signed = signer.sign_message(MESSAGE).expect("signs");

        // RFC 6376 §3.7: the signature is prepended and the message is untouched.
        assert!(signed.ends_with(MESSAGE), "the message itself must survive");
        let text = String::from_utf8_lossy(&signed);
        assert!(text.starts_with("DKIM-Signature:"), "{text}");
        assert!(text.contains("d=z1hwang.cn"), "{text}");
        assert!(text.contains("s=default"), "{text}");
        assert!(text.contains("bh="), "{text}");

        let resolver: Arc<dyn Resolver> = Arc::new(
            MockResolver::new()
                .with_txt(&format!("{SELECTOR}._domainkey.{DOMAIN}"), vec![published]),
        );
        let verdict = DkimVerifier::new(resolver).verify(&signed).await;
        assert_eq!(verdict.result, DkimResult::Pass, "{verdict:?}");
        assert_eq!(verdict.domain.as_deref(), Some(DOMAIN));
        assert_eq!(verdict.selector.as_deref(), Some(SELECTOR));
    }
}
