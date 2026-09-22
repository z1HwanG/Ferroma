//! Runtime supervisor for the SMTP and IMAP listener families.
//!
//! The Admin API owns a small command handle; this task owns sockets. Keeping that
//! boundary in the binary lets `ferroma-api` remain independent of the SMTP/IMAP crates.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ferroma_api::state::{
    ListenerControl, ManagedListener, ManagedListenerState, ManagedListenerStates,
};
use ferroma_core::{FerromaError, Result};
use ferroma_imap::{ImapServer, ImapServerConfig};
use ferroma_smtp::server::{SmtpServer, SmtpServerConfig, SmtpServerHandle};
use ferroma_storage::{Maildir, Repositories};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::TlsAcceptor;

/// API-facing handle for the listener supervisor.
#[derive(Clone)]
pub struct RuntimeListenerControl {
    commands: mpsc::Sender<Command>,
    states: watch::Receiver<ManagedListenerStates>,
}

impl std::fmt::Debug for RuntimeListenerControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeListenerControl")
            .field("states", &*self.states.borrow())
            .finish_non_exhaustive()
    }
}

impl ListenerControl for RuntimeListenerControl {
    fn states(&self) -> ManagedListenerStates {
        *self.states.borrow()
    }

    fn set_enabled(
        &self,
        listener: ManagedListener,
        enabled: bool,
    ) -> Pin<Box<dyn Future<Output = Result<ManagedListenerState>> + Send + '_>> {
        Box::pin(async move {
            let (reply, received) = oneshot::channel();
            self.commands
                .send(Command::Set {
                    listener,
                    enabled,
                    reply,
                })
                .await
                .map_err(|_| {
                    FerromaError::Conflict("listener supervisor is no longer running".into())
                })?;
            received.await.map_err(|_| {
                FerromaError::Conflict(
                    "listener supervisor stopped before applying the change".into(),
                )
            })?
        })
    }
}

/// Configuration retained by the supervisor so it can bind again after a toggle.
#[derive(Clone)]
pub struct ListenerFactories {
    /// SMTP listener factory input.
    pub smtp: Option<SmtpServerConfig>,
    /// IMAP listener configuration.
    pub imap: Option<ImapServerConfig>,
    /// Shared repositories for a fresh IMAP server.
    pub repos: Repositories,
    /// Shared Maildir for a fresh IMAP server.
    pub maildir: Maildir,
    /// TLS acceptor, when TLS was configured at boot.
    pub tls: Option<TlsAcceptor>,
    /// Shared event bus for IMAP IDLE notifications.
    pub events: Arc<ferroma_events::EventBus>,
}

enum Command {
    Set {
        listener: ManagedListener,
        enabled: bool,
        reply: oneshot::Sender<Result<ManagedListenerState>>,
    },
}

struct Running {
    smtp: Option<SmtpServerHandle>,
    imap: Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)>,
}

/// Start the socket-owning supervisor and return its API-facing control handle.
pub async fn start(
    factories: ListenerFactories,
    smtp_enabled: bool,
    imap_enabled: bool,
    jmap_enabled: bool,
) -> Result<Arc<RuntimeListenerControl>> {
    let smtp_available = factories.smtp.is_some();
    let imap_available = factories.imap.is_some();
    let initial = ManagedListenerStates {
        smtp: ManagedListenerState {
            available: smtp_available,
            enabled: false,
        },
        imap: ManagedListenerState {
            available: imap_available,
            enabled: false,
        },
        // JMAP rides the already-running HTTP API, so it has no socket factory of its
        // own. The supervisor changes the routing gate immediately.
        jmap: ManagedListenerState {
            available: true,
            enabled: jmap_enabled,
        },
    };
    let (state_tx, state_rx) = watch::channel(initial);
    let (command_tx, mut command_rx) = mpsc::channel(8);
    let control = Arc::new(RuntimeListenerControl {
        commands: command_tx,
        states: state_rx,
    });

    let mut running = Running {
        smtp: None,
        imap: None,
    };
    if smtp_enabled && smtp_available {
        running.smtp = Some(start_smtp(&factories).await?);
        publish(&state_tx, ManagedListener::Smtp, true);
    }
    if imap_enabled && imap_available {
        running.imap = Some(start_imap(&factories).await?);
        publish(&state_tx, ManagedListener::Imap, true);
    }

    tokio::spawn(async move {
        while let Some(Command::Set {
            listener,
            enabled,
            reply,
        }) = command_rx.recv().await
        {
            let current = *state_tx.borrow();
            let available = match listener {
                ManagedListener::Smtp => current.smtp.available,
                ManagedListener::Imap => current.imap.available,
                ManagedListener::Jmap => current.jmap.available,
            };
            let result = if !available {
                Err(FerromaError::Conflict(format!(
                    "{} is disabled for this process by its startup selection",
                    listener.as_str()
                )))
            } else if current_enabled(current, listener) == enabled {
                Ok(current_state(current, listener))
            } else {
                apply(&factories, &mut running, listener, enabled, &state_tx).await
            };
            let _ = reply.send(result);
        }

        if let Some(handle) = running.smtp.take() {
            handle.shutdown_and_wait().await;
        }
        if let Some((shutdown, task)) = running.imap.take() {
            let _ = shutdown.send(true);
            let _ = task.await;
        }
    });
    Ok(control)
}

async fn apply(
    factories: &ListenerFactories,
    running: &mut Running,
    listener: ManagedListener,
    enabled: bool,
    states: &watch::Sender<ManagedListenerStates>,
) -> Result<ManagedListenerState> {
    match (listener, enabled) {
        (ManagedListener::Smtp, true) => {
            running.smtp = Some(start_smtp(factories).await?);
        }
        (ManagedListener::Smtp, false) => {
            if let Some(handle) = running.smtp.take() {
                handle.shutdown_and_wait().await;
            }
        }
        (ManagedListener::Imap, true) => {
            running.imap = Some(start_imap(factories).await?);
        }
        (ManagedListener::Imap, false) => {
            if let Some((shutdown, task)) = running.imap.take() {
                let _ = shutdown.send(true);
                let _ = task.await;
            }
        }
        // JMAP is an HTTP route gate, not an independently bound listener.
        (ManagedListener::Jmap, _) => {}
    }
    publish(states, listener, enabled);
    Ok(current_state(*states.borrow(), listener))
}

async fn start_smtp(factories: &ListenerFactories) -> Result<SmtpServerHandle> {
    let config = factories
        .smtp
        .clone()
        .ok_or_else(|| FerromaError::Config("SMTP listener is unavailable".into()))?;
    SmtpServer::new(config).start().await
}

async fn start_imap(
    factories: &ListenerFactories,
) -> Result<(watch::Sender<bool>, tokio::task::JoinHandle<()>)> {
    let config = factories
        .imap
        .clone()
        .ok_or_else(|| FerromaError::Config("IMAP listener is unavailable".into()))?;
    let mut server = ImapServer::new(
        config,
        Arc::new(factories.repos.clone()),
        Arc::new(factories.maildir.clone()),
    )?
    .with_events(Arc::clone(&factories.events));
    if let Some(acceptor) = factories.tls.clone() {
        server = server.with_tls(acceptor);
    }
    let server = Arc::new(server);
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        if let Err(error) = server.serve_with_shutdown(receiver).await {
            tracing::error!(%error, "the IMAP listener stopped with an error");
        }
    });
    Ok((shutdown, task))
}

fn publish(
    sender: &watch::Sender<ManagedListenerStates>,
    listener: ManagedListener,
    enabled: bool,
) {
    sender.send_modify(|states| match listener {
        ManagedListener::Smtp => states.smtp.enabled = enabled,
        ManagedListener::Imap => states.imap.enabled = enabled,
        ManagedListener::Jmap => states.jmap.enabled = enabled,
    });
}

fn current_enabled(states: ManagedListenerStates, listener: ManagedListener) -> bool {
    current_state(states, listener).enabled
}

fn current_state(states: ManagedListenerStates, listener: ManagedListener) -> ManagedListenerState {
    match listener {
        ManagedListener::Smtp => states.smtp,
        ManagedListener::Imap => states.imap,
        ManagedListener::Jmap => states.jmap,
    }
}
