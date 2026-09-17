//! `ferroma serve` — starting the platform.
//!
//! One process hosts every subsystem, wired through the same repositories, the same
//! `Maildir`, the same attachment store, the same event bus and the same auth
//! service:
//!
//! ```text
//!   SMTP listener ──► DeliveryService ──► Maildir + PostgreSQL ──► EventBus
//!   IMAP listener ◄── repositories + Maildir
//!   Queue workers ──► SmtpClient ──► remote MX        ◄── EventBus (delivery.updated)
//!   HTTP API      ◄── AppState ──► all of the above
//! ```
//!
//! Sharing the services is the point, not an optimisation: a message delivered over
//! SMTP must appear over IMAP and be visible to the API's sync cursor immediately,
//! and that only holds because there is exactly one storage layer and one bus.
//!
//! # Shutdown
//!
//! `SIGINT`/`SIGTERM` flips a `watch` channel. Every subsystem already takes a
//! receiver: the SMTP server stops accepting and lets in-flight transactions finish,
//! the queue workers finish the delivery they are on, IMAP says `* BYE`, and the HTTP
//! server drains. The process exits once they have, within
//! `server.shutdown_timeout_secs`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::watch;

use ferroma_api::state::AppState;
use ferroma_auth::{AuthService, TokenService};
use ferroma_core::config::Config;
use ferroma_events::{EventBus, EventBusConfig};
use ferroma_imap::{ImapServer, ImapServerConfig};
use ferroma_smtp::client::{SmtpClient, SmtpClientConfig};
use ferroma_smtp::delivery::DeliveryService;
use ferroma_smtp::mx::{HickoryResolver, MxResolver};
use ferroma_smtp::queue::{QueueConfigView, QueueWorker};
use ferroma_smtp::server::{SmtpServer, SmtpServerConfig};
use ferroma_storage::{AttachmentStore, Database, Maildir, Repositories};
use ferroma_sync::SyncService;

use crate::cli::ServeArgs;
use crate::tls;

/// Which subsystems to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    smtp: bool,
    imap: bool,
    api: bool,
    queue: bool,
}

impl Selection {
    /// Everything the configuration enables, or only what `--only` names.
    fn resolve(config: &Config, only: &[String]) -> Self {
        if only.is_empty() {
            return Selection {
                smtp: config.smtp.enabled,
                imap: config.imap.enabled,
                api: config.api.enabled,
                queue: config.queue.enabled,
            };
        }
        let has = |name: &str| only.iter().any(|o| o == name);
        Selection {
            smtp: has("smtp"),
            imap: has("imap"),
            api: has("api"),
            queue: has("queue"),
        }
    }

    fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.smtp {
            names.push("smtp");
        }
        if self.imap {
            names.push("imap");
        }
        if self.api {
            names.push("api");
        }
        if self.queue {
            names.push("queue");
        }
        names
    }
}

/// Run the platform until a shutdown signal arrives.
///
/// `log_sink` is the same handle the `tracing` layer installed at boot fills, and it
/// becomes the buffer `GET /api/v1/logs` reads. It is passed in rather than created
/// here because the layer has to be composed into the subscriber before this point.
pub fn run(config: &Config, args: &ServeArgs, log_sink: ferroma_api::LogSink) -> Result<ExitCode> {
    for warning in tls::insecure_warnings(&config.tls) {
        tracing::warn!("{warning}");
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if config.server.worker_threads > 0 {
        builder.worker_threads(config.server.worker_threads);
    }
    let runtime = builder.build().context("building the Tokio runtime")?;

    runtime.block_on(serve(config, args, log_sink))
}

/// The IMAP server's view of the configuration.
///
/// The TLS fields have to be carried across here rather than only attached
/// afterwards: `ImapServer::new` validates the configuration, and `with_tls` —
/// which supplies the acceptor — runs after that. Leaving them at their defaults
/// is what made `imaps_port = 993` fail at boot with
/// `imap: imaps_port is set but TLS is disabled`, on a server whose certificate
/// had just been loaded successfully.
fn imap_server_config(config: &Config) -> ImapServerConfig {
    ImapServerConfig {
        host: config.imap.host.clone(),
        port: config.imap.port,
        imaps_port: config.imap.imaps_port,
        banner: config.imap.banner.clone(),
        require_tls_for_login: config.imap.require_tls_for_login,
        idle_timeout_secs: config.imap.idle_timeout_secs,
        max_idle_secs: config.imap.max_idle_secs,
        enable_idle: config.imap.enable_idle,
        enable_move: config.imap.enable_move,
        max_append_size: config.imap.max_append_size,
        max_fetch_messages: config.limits.max_fetch_messages,
        tls_enabled: config.tls.enabled,
        tls_cert_path: config.tls.cert_path.clone(),
        tls_key_path: config.tls.key_path.clone(),
        maildir_root: config.maildir_root(),
        fsync_on_write: config.storage.fsync_on_write,
    }
}

async fn serve(config: &Config, args: &ServeArgs, log_sink: ferroma_api::LogSink) -> Result<ExitCode> {
    let selection = Selection::resolve(config, &args.only);
    if selection.names().is_empty() {
        anyhow::bail!(
            "nothing to start: every subsystem is disabled by the configuration and --only named none"
        );
    }

    // --- storage ------------------------------------------------------------
    let database = Arc::new(Database::connect(&config.database).await.map_err(|err| {
        let message = err.to_string();
        // The single most common first-run failure. Name the command that fixes it
        // rather than making the operator work out that they need `psql`.
        if message.contains("does not exist") {
            anyhow!(
                "{message}\n\n\
                 The database has not been created yet. Run:\n    \
                 ferroma database init\n\
                 (or `ferroma migrate`, which creates it too), then start the server again."
            )
        } else {
            anyhow!("{message}")
        }
    })?);
    if args.migrate || config.database.run_migrations {
        database.migrate().await.context("applying migrations")?;
    }
    let (migrations_total, migrations_applied) = database.migration_status().await?;
    let repos: Repositories = database.repositories();

    let maildir = Maildir::new(
        config.maildir_root(),
        config.storage.fsync_on_write,
        config.storage.layout,
    );
    let attachments = AttachmentStore::new(config.attachment_root(), config.storage.fsync_on_write);

    // --- shared services ----------------------------------------------------
    let events = Arc::new(EventBus::new(EventBusConfig::default()));
    let tokens = TokenService::from_config(&config.api, &config.server.hostname)?;
    let auth = Arc::new(AuthService::with_defaults(
        repos.clone(),
        tokens.clone(),
        config.limits.clone(),
    ));
    let sync = Arc::new(SyncService::new(
        repos.clone(),
        config.client.sync_page_size,
        config.client.tombstone_retention_days,
    ));

    // --- TLS ----------------------------------------------------------------
    let tls = tls::build(config)?;
    if let Some(material) = &tls {
        if !material.source().is_trusted() {
            // A self-signed certificate on a public MX is worse than no TLS: clients
            // cannot verify it, and remote servers may refuse delivery outright.
            tracing::warn!(
                "TLS is using a certificate no client can verify; this is only \
                 appropriate for local development"
            );
        }
    }
    let acceptor = tls.as_ref().map(|material| material.acceptor());

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

    // --- DNS ------------------------------------------------------------------
    // One resolver, shared: the outbound queue uses it to find MX hosts, and the
    // inbound path uses it to evaluate SPF, DKIM and DMARC. Sharing it means one
    // cache and one place where `[dns]` is honoured.
    let resolver: Option<Arc<MxResolver>> = if selection.smtp || selection.queue {
        let hickory = HickoryResolver::new(&config.dns).context("building the DNS resolver")?;
        Some(Arc::new(MxResolver::new(Arc::new(hickory), &config.dns)))
    } else {
        None
    };

    // --- SMTP ---------------------------------------------------------------
    let delivery = Arc::new(
        DeliveryService::new(
            repos.clone(),
            maildir.clone(),
            attachments.clone(),
            config.server.hostname.clone(),
        )
        .with_event_bus((*events).clone()),
    );

    if selection.smtp {
        let mut smtp_config = SmtpServerConfig::new(
            config.clone(),
            repos.clone(),
            Arc::clone(&delivery),
        )
        .with_auth(Arc::clone(&auth))
        .with_events((*events).clone());
        if let Some(acceptor) = acceptor.clone() {
            smtp_config = smtp_config.with_tls(acceptor);
        }
        if let Some(resolver) = resolver.clone() {
            // Turns on the inbound policy step: SPF, DKIM verification, DMARC and the
            // `Authentication-Results:` header. Without it, mail is still delivered —
            // it is simply not authenticated, which is why this is not an error.
            smtp_config = smtp_config.with_resolver(resolver);
        }

        let handle = SmtpServer::new(smtp_config)
            .start()
            .await
            .context("starting the SMTP listeners")?;

        let addresses: Vec<String> = handle.local_addrs().iter().map(|a| a.to_string()).collect();
        tracing::info!(listeners = ?addresses, "SMTP listening");
        println!("smtp      {}", addresses.join(", "));

        // The listener threads own their own shutdown watch; subscribing here keeps
        // the handle alive for the lifetime of the process.
        let mut smtp_shutdown = handle.subscribe_shutdown();
        tasks.push((
            "smtp",
            tokio::spawn(async move {
                let _ = smtp_shutdown.changed().await;
                handle.shutdown();
            }),
        ));
    } else {
        tracing::info!("SMTP is disabled");
    }

    // --- outbound queue -----------------------------------------------------
    if selection.queue {
        let resolver = resolver
            .clone()
            .expect("the resolver is built whenever the queue is selected");
        let client = SmtpClient::new(SmtpClientConfig::from_config(config));
        let queue_view = QueueConfigView::from_config(config);
        // Named in the startup banner: an operator who configured a relay has to be
        // able to see that it is carrying the mail, rather than the MX path.
        let relay_host = queue_view.relay.as_ref().map(|relay| relay.host.clone());
        let worker = QueueWorker::new(
            client,
            resolver,
            repos.clone(),
            maildir.clone(),
            Arc::clone(&delivery),
            queue_view,
        )
        .with_event_bus((*events).clone());

        match relay_host {
            Some(host) => println!(
                "queue     {} worker(s), outbound via relay {host}",
                config.queue.workers
            ),
            None => println!("queue     {} worker(s)", config.queue.workers),
        }
        // Clone *before* the spawn: an `async move` block takes ownership, and the
        // API listener further down still needs the receiver.
        let queue_shutdown = shutdown_rx.clone();
        tasks.push((
            "queue",
            tokio::spawn(async move { worker.run(queue_shutdown).await }),
        ));
    }

    // --- IMAP ---------------------------------------------------------------
    if selection.imap {
        let imap_config = imap_server_config(config);

        let mut server = ImapServer::new(
            imap_config,
            Arc::new(repos.clone()),
            Arc::new(maildir.clone()),
        )
        .context("configuring the IMAP server")?
        .with_events(Arc::clone(&events));
        if let Some(acceptor) = acceptor.clone() {
            server = server.with_tls(acceptor);
        }

        println!(
            "imap      {}:{} (starttls), {}:{} (tls)",
            config.imap.host, config.imap.port, config.imap.host, config.imap.imaps_port
        );

        let server = Arc::new(server);
        let mut imap_shutdown = shutdown_rx.clone();
        tasks.push((
            "imap",
            tokio::spawn(async move {
                if let Err(err) = server
                    .serve(async move {
                        let _ = imap_shutdown.changed().await;
                    })
                    .await
                {
                    tracing::error!(error = %err, "the IMAP server stopped with an error");
                }
            }),
        ));
    }

    // --- HTTP API -----------------------------------------------------------
    if selection.api {
        let state = AppState::new(
            repos.clone(),
            Arc::new(config.clone()),
            tokens,
            Arc::clone(&auth),
            Arc::clone(&events),
            sync,
        )
        .with_database(Arc::clone(&database))
        // The ring the `tracing` layer at boot is filling, so `GET /api/v1/logs`
        // answers with what this process has actually logged.
        .with_log_sink(log_sink);

        let router = ferroma_api::build(state);
        let address = format!("{}:{}", config.api.host, config.api.port);

        let listener = tokio::net::TcpListener::bind(&address)
            .await
            .with_context(|| format!("binding the HTTP API to {address}"))?;
        let bound = listener.local_addr()?;
        println!("http      http://{bound}{}", config.api.base_path);
        if config.api.serve_frontend {
            println!("webmail   http://{bound}/");
            println!("admin     http://{bound}/admin");
        }

        let mut api_shutdown = shutdown_rx.clone();
        tasks.push((
            "api",
            tokio::spawn(async move {
                let result = axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = api_shutdown.changed().await;
                    })
                    .await;
                if let Err(err) = result {
                    tracing::error!(error = %err, "the HTTP server stopped with an error");
                }
            }),
        ));
    }

    // --- ready --------------------------------------------------------------
    println!();
    println!(
        "{} {} ready — {}",
        config.server.name,
        ferroma_core::VERSION,
        tls::summary(tls.as_ref())
    );
    println!(
        "database  {} ({} of {} migrations applied, pool {}/{})",
        database.redacted_url(),
        migrations_applied,
        migrations_total,
        database.pool_stats().size,
        database.pool_stats().max,
    );
    println!("started   {}", selection.names().join(", "));
    println!("press Ctrl-C to stop");

    // --- wait for a signal --------------------------------------------------
    wait_for_shutdown().await;
    tracing::info!("shutdown requested");
    println!("\nshutting down…");

    let _ = shutdown_tx.send(true);

    let grace = Duration::from_secs(config.server.shutdown_timeout_secs.max(1));
    for (name, task) in tasks {
        match tokio::time::timeout(grace, task).await {
            Ok(Ok(())) => tracing::debug!(subsystem = name, "stopped"),
            Ok(Err(err)) => tracing::warn!(subsystem = name, error = %err, "task ended badly"),
            Err(_) => tracing::warn!(
                subsystem = name,
                "did not stop within {}s; abandoning it",
                grace.as_secs()
            ),
        }
    }

    database.close().await;
    println!("stopped");
    Ok(ExitCode::SUCCESS)
}

/// Resolve on `SIGINT` or `SIGTERM`.
///
/// SIGTERM is what `docker stop` and systemd send, so handling only Ctrl-C would make
/// every container shutdown a hard kill after the grace period.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!(error = %err, "cannot listen for SIGTERM; falling back to Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enabled_imaps_port_reaches_the_imap_configuration() {
        // Regression: the TLS fields used to keep their defaults, so a server that
        // had just loaded its certificate still refused to start with
        // "imap: imaps_port is set but TLS is disabled" the moment `imaps_port` was
        // configured — which is exactly what a production deployment does.
        let mut config = Config::default();
        config.imap.imaps_port = 993;
        config.tls.enabled = true;
        config.tls.cert_path = Some(std::path::PathBuf::from("/etc/ferroma/tls/fullchain.pem"));
        config.tls.key_path = Some(std::path::PathBuf::from("/etc/ferroma/tls/privkey.pem"));

        let imap = imap_server_config(&config);
        assert_eq!(imap.imaps_port, 993);
        assert!(imap.tls_enabled, "tls.enabled must not be dropped");
        assert_eq!(
            imap.tls_cert_path.as_deref(),
            Some(std::path::Path::new("/etc/ferroma/tls/fullchain.pem"))
        );

        // `validate` also insists the PEM files exist, which they do not in a test;
        // the rule this guards is the one above it.
        if let Err(err) = imap.validate() {
            assert!(
                !err.contains("imaps_port is set but TLS is disabled"),
                "the TLS flag was dropped again: {err}"
            );
        }
    }

    #[test]
    fn an_empty_only_list_follows_the_configuration() {
        let mut config = Config::default();
        config.smtp.enabled = true;
        config.imap.enabled = false;
        config.api.enabled = true;
        config.queue.enabled = false;

        let selection = Selection::resolve(&config, &[]);
        assert!(selection.smtp);
        assert!(!selection.imap);
        assert!(selection.api);
        assert!(!selection.queue);
        assert_eq!(selection.names(), vec!["smtp", "api"]);
    }

    #[test]
    fn only_names_exactly_what_it_lists() {
        // `--only smtp` must not also start the API just because the config enables it.
        let config = Config::default();
        let selection = Selection::resolve(&config, &["smtp".to_string()]);
        assert!(selection.smtp);
        assert!(!selection.imap && !selection.api && !selection.queue);
        assert_eq!(selection.names(), vec!["smtp"]);
    }

    #[test]
    fn only_can_name_several_subsystems() {
        let config = Config::default();
        let selection = Selection::resolve(&config, &["imap".to_string(), "queue".to_string()]);
        assert_eq!(selection.names(), vec!["imap", "queue"]);
    }

    #[test]
    fn nothing_selected_is_visible_to_the_caller() {
        let mut config = Config::default();
        config.smtp.enabled = false;
        config.imap.enabled = false;
        config.api.enabled = false;
        config.queue.enabled = false;
        assert!(Selection::resolve(&config, &[]).names().is_empty());
    }
}
