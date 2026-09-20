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

/// Adopt the configuration values the first-run wizard stored in the `settings` table.
///
/// The wizard is a browser form, and it can only persist what it can reach: the
/// database. Its `server.hostname` and `api.public_url` rows are therefore read back
/// here, before any service is built, so the operator does not have to put them in
/// `ferroma.toml` or the environment to finish an install.
///
/// Precedence is `ferroma.toml` / environment, then these rows, then the built-in
/// default — a row never overrides a value the operator stated. That is decidable
/// without recording where each value came from: a value that still equals the default
/// is one nobody set.
async fn apply_stored_settings(config: &mut Config, repos: &Repositories) {
    let default = Config::default();

    if config.server.hostname == default.server.hostname {
        if let Some(hostname) = stored_string(repos, "server.hostname").await {
            tracing::info!(hostname, "adopting the hostname stored by the setup wizard");
            config.server.hostname = hostname;
        }
    }

    if config.api.public_url == default.api.public_url {
        if let Some(public_url) = stored_string(repos, "api.public_url").await {
            tracing::info!(public_url, "adopting the public URL stored by the setup wizard");
            config.api.public_url = public_url;
        }
    }

    // The listener and TLS are the same arrangement: the wizard is a browser form, so it
    // can only persist these, and they are adopted on the next start — unless the
    // deployment stated them itself, which always wins.
    if config.api.host == default.api.host {
        if let Some(host) = stored_string(repos, "api.host").await {
            tracing::info!(host, "adopting the API listen address stored by the setup wizard");
            config.api.host = host;
        }
    }

    if config.api.port == default.api.port {
        if let Some(port) = stored_u16(repos, "api.port").await {
            tracing::info!(port, "adopting the API port stored by the setup wizard");
            config.api.port = port;
        }
    }

    // TLS is the one group whose stored value can make an instance unusable. A path that
    // no longer exists — a volume that was not mounted, a typo saved from the wizard,
    // material rotated out from under the server — turns a strict configuration check
    // into a refusal to start, and that locks the operator out of the very console that
    // would let them fix it. So only settings that can actually be used are adopted;
    // anything else is a warning, and TLS stays off. A value stated by the deployment is
    // still validated strictly, because that one the operator can see and change.
    let stored_cert = stored_string(repos, "tls.cert_path").await;
    let stored_key = stored_string(repos, "tls.key_path").await;
    let material_usable = match (&stored_cert, &stored_key) {
        (Some(cert), Some(key)) => {
            std::path::Path::new(cert).is_file() && std::path::Path::new(key).is_file()
        }
        _ => false,
    };

    if config.tls.cert_path.is_none() && config.tls.key_path.is_none() {
        if let (Some(cert), Some(key)) = (&stored_cert, &stored_key) {
            if material_usable {
                tracing::info!(cert, key, "adopting the TLS material stored by the setup wizard");
                config.tls.cert_path = Some(cert.into());
                config.tls.key_path = Some(key.into());
            } else {
                tracing::warn!(
                    cert,
                    key,
                    "ignoring the TLS material stored by the setup wizard: it is not on disk"
                );
            }
        }
    }

    if config.tls.enabled == default.tls.enabled {
        if let Some(enabled) = stored_bool(repos, "tls.enabled").await {
            // Enabling is adopted when the material is usable or when none was stored at
            // all — the latter is the self-signed fallback, which is a working server.
            let has_stored_material = stored_cert.is_some() || stored_key.is_some();
            if !enabled || material_usable || !has_stored_material {
                tracing::info!(enabled, "adopting the TLS switch stored by the setup wizard");
                config.tls.enabled = enabled;
            } else {
                tracing::warn!(
                    "not enabling TLS from the stored setting: the certificate it names is \
                     not on disk"
                );
            }
        }
    }
}

/// One setting, when it is a number that fits the field it belongs to.
async fn stored_u16(repos: &Repositories, key: &str) -> Option<u16> {
    match repos.settings.get(key).await {
        Ok(Some(value)) => value.as_u64().and_then(|number| u16::try_from(number).ok()),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(key, error = %err, "could not read a stored setting");
            None
        }
    }
}

/// One setting, when it is a boolean.
async fn stored_bool(repos: &Repositories, key: &str) -> Option<bool> {
    match repos.settings.get(key).await {
        Ok(Some(value)) => value.as_bool(),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(key, error = %err, "could not read a stored setting");
            None
        }
    }
}

/// One setting, when it is a non-blank string.
async fn stored_string(repos: &Repositories, key: &str) -> Option<String> {
    match repos.settings.get(key).await {
        Ok(Some(serde_json::Value::String(value))) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Ok(_) => None,
        Err(err) => {
            // A settings read failure must not stop a server from starting: the file and
            // the environment are still a complete configuration without this row.
            tracing::warn!(key, error = %err, "could not read a stored setting");
            None
        }
    }
}

/// Give `api.jwt_secret` a value, keeping the generated one across restarts.
///
/// Configuring the secret in the environment is the right thing in production, and the
/// server still prefers it. What it no longer does when the variable is absent is
/// generate a secret and throw it away: that logged every session out on every restart,
/// and it made `FERROMA_JWT_SECRET` a startup item nobody could skip. The generated
/// secret is written to `<data_dir>/jwt_secret` with owner-only permissions and reused.
fn ensure_jwt_secret(config: &mut Config) -> Result<()> {
    if config
        .api
        .jwt_secret
        .as_deref()
        .map(str::trim)
        .is_some_and(|secret| !secret.is_empty())
    {
        return Ok(());
    }

    let path = config.server.data_dir.join("jwt_secret");
    if let Some(existing) = std::fs::read_to_string(&path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value.len() >= 32)
    {
        config.api.jwt_secret = Some(existing);
        return Ok(());
    }

    let secret = ferroma_auth::TokenService::generate_secret();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_private_file(&path, &secret)
        .with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(
        path = %path.display(),
        "api.jwt_secret is not configured: generated one and stored it for later restarts"
    );
    config.api.jwt_secret = Some(secret);
    Ok(())
}

/// Write a small secret file that only its owner may read.
#[cfg(unix)]
pub(crate) fn write_private_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")
}

/// The same, for a host without POSIX permissions.
#[cfg(not(unix))]
fn write_private_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{contents}\n"))
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

/// Bind the HTTP port before the database exists.
///
/// The bootstrap page *is* the API's port: the operator is told one address, and the page
/// they open there is the one that changes what the process does next.
async fn bind_api(config: &Config) -> Result<tokio::net::TcpListener> {
    let address = format!("{}:{}", config.api.host, config.api.port);
    tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("binding the HTTP API to {address}"))
}

/// Where the database connection came from, which decides how a failure is handled.
///
/// A deployment that *stated* an address gets a fast, loud failure — that is a mistake in a
/// file or an environment variable, and the operator can see it. An address this server
/// remembered from the wizard, or the absence of one, gets the bootstrap page instead: there
/// is nothing to quote at a log reader, and the tool that fixes it can be served right here.
enum DatabaseSource {
    /// From `ferroma.toml`, `FERROMA_DATABASE__URL`, or a flag.
    Stated,
    /// From `<data_dir>/database.json`, written by the wizard.
    Remembered(String),
    /// Nowhere yet.
    Missing,
}

/// Classify the configured connection.
fn database_source(config: &Config) -> DatabaseSource {
    let default = ferroma_core::config::DatabaseConfig::default();
    if config.database.url.trim() != default.url.trim() {
        return DatabaseSource::Stated;
    }
    match crate::bootstrap::stored_url(&config.server.data_dir) {
        Some(url) if !url.trim().is_empty() => DatabaseSource::Remembered(url),
        _ => DatabaseSource::Missing,
    }
}

/// What the connect attempt told us, in one place so bootstrap mode can quote it.
async fn database_reachable(url: &str) -> std::result::Result<(), String> {
    let mut database_config = ferroma_core::config::DatabaseConfig {
        url: url.to_string(),
        ..Default::default()
    };
    database_config.max_connections = 2;
    match Database::connect(&database_config).await {
        Ok(_) => Ok(()),
        Err(err) => Err(err.to_string()),
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
    // The address may come from the wizard's file rather than from a flag, and when there
    // is none at all this is where the operator is handed the page that asks for one. The
    // listener it already bound comes back with it, so the API carries on serving on the
    // same socket instead of racing a rebind.
    let mut config = config.clone();
    let mut bootstrap_listener: Option<tokio::net::TcpListener> = None;
    match database_source(&config) {
        DatabaseSource::Stated => {}
        DatabaseSource::Remembered(url) => match database_reachable(&url).await {
            Ok(()) => {
                tracing::info!(url = %crate::bootstrap::redact_url(&url), "using the database the setup page remembered");
                config.database.url = url;
            }
            Err(problem) => {
                let listener = bind_api(&config).await?;
                let (url, listener) = crate::bootstrap::run(config.clone(), Some(problem), listener).await?;
                config.database.url = url;
                bootstrap_listener = Some(listener);
            }
        },
        DatabaseSource::Missing => {
            let listener = bind_api(&config).await?;
            let (url, listener) = crate::bootstrap::run(config.clone(), None, listener).await?;
            config.database.url = url;
            bootstrap_listener = Some(listener);
        }
    }

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

    // From here on the configuration is a local copy, because two things may complete it
    // at startup: the values the first-run wizard stored in the `settings` table, and a
    // signing secret provisioned into the data directory so a session survives a restart.
    // Only the values nobody stated explicitly are adopted; see `apply_stored_settings`.
    apply_stored_settings(&mut config, &repos).await;
    ensure_jwt_secret(&mut config)?;

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
    let tls = tls::build(&config)?;
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
        let client = SmtpClient::new(SmtpClientConfig::from_config(&config));
        let queue_view = QueueConfigView::from_config(&config);
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
        let imap_config = imap_server_config(&config);

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

        // Bootstrap mode already bound this port and served the page that produced the
        // database; taking that listener back means the port is never briefly unlistened.
        let listener = match bootstrap_listener.take() {
            Some(listener) => listener,
            None => tokio::net::TcpListener::bind(&address)
                .await
                .with_context(|| format!("binding the HTTP API to {address}"))?,
        };
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
    fn a_generated_jwt_secret_is_kept_and_reused() {
        // Regression: an unconfigured `api.jwt_secret` was generated and thrown away at
        // every boot, which logged every session out on every restart and made
        // `FERROMA_JWT_SECRET` an unavoidable startup item.
        let dir = std::env::temp_dir().join(format!("ferroma-jwt-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut first = Config::default();
        first.server.data_dir = dir.clone();
        ensure_jwt_secret(&mut first).expect("a secret must be provisioned");
        let secret = first.api.jwt_secret.clone().expect("the config must carry it");
        assert!(secret.len() >= 32, "a short secret is refused by TokenService");

        // The next boot reads the same file rather than minting a new one.
        let mut second = Config::default();
        second.server.data_dir = dir.clone();
        ensure_jwt_secret(&mut second).expect("the stored secret must be reused");
        assert_eq!(second.api.jwt_secret.as_deref(), Some(secret.as_str()));

        // A configured secret always wins: the file is a convenience, not an override.
        let mut configured = Config::default();
        configured.server.data_dir = dir.clone();
        configured.api.jwt_secret = Some("a-configured-secret-long-enough-to-pass".to_string());
        ensure_jwt_secret(&mut configured).expect("nothing to do");
        assert_eq!(
            configured.api.jwt_secret.as_deref(),
            Some("a-configured-secret-long-enough-to-pass")
        );

        // On a POSIX host the file must not be world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("jwt_secret"))
                .expect("the secret file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the secret must be owner-only: {mode:o}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

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
