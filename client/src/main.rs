//! Ferroma desktop client entry point.
//!
//! The client core lives in the library; this binary hosts it headlessly so the
//! core can be exercised — and scripted — without a GUI:
//!
//! ```text
//! ferroma-client account add alice@example.com --password …
//! ferroma-client account list
//! ferroma-client sync
//! ferroma-client list INBOX --limit 20
//! ferroma-client read 4821
//! ferroma-client send --to bob@example.net --subject Hi --body hello
//! ferroma-client search 'from:bob has:attachment'
//! ferroma-client outbox
//! ferroma-client devices
//! ferroma-client config path
//! ```
//!
//! Every subcommand accepts `--data-dir` (or `FERROMA_DATA_DIR`) and `--json`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ferroma_client::account::AccountId;
use ferroma_client::error::ClientError;
use ferroma_client::operations::{FcpSubmitter, OperationFlusher};
use ferroma_client::outbox::{NewOutboxItem, OutboxState};
use ferroma_client::ui::{ClientHandle, MessageView};
use ferroma_core::config::LogFormat;
use serde_json::json;

/// The official Ferroma desktop client.
#[derive(Debug, Parser)]
#[command(name = "ferroma-client", version, about, long_about = None)]
struct Cli {
    /// Where the local cache lives. Defaults to the platform data directory.
    #[arg(long, global = true, env = "FERROMA_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    /// How chatty the logs are (`error`, `warn`, `info`, `debug`, `trace`).
    #[arg(long, global = true, default_value = "warn", env = "FERROMA_LOG_LEVEL")]
    log: String,

    /// Do not retry failed requests (useful in scripts).
    #[arg(long, global = true)]
    no_retry: bool,

    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the configured accounts.
    #[command(subcommand)]
    Account(AccountCommand),

    /// Sync every account, or one with `--account`.
    Sync {
        /// Only this account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// List the messages of a folder (by name or id).
    List {
        /// The folder name (`INBOX`) or its numeric id.
        folder: String,
        /// How many messages to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Skip this many messages.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Read one message, downloading its body if needed.
    Read {
        /// The message id.
        id: i64,
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Queue a message in the outbox and try to send it now.
    Send {
        /// Recipients, comma separated (repeatable).
        #[arg(long, value_delimiter = ',', required = true)]
        to: Vec<String>,
        /// The subject.
        #[arg(long, default_value = "")]
        subject: String,
        /// The plain-text body.
        #[arg(long, default_value = "")]
        body: String,
        /// The `From:` address; defaults to the account's primary address.
        #[arg(long)]
        from: Option<String>,
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Search the local cache, falling back to the server when it is empty.
    Search {
        /// The query, e.g. `from:bob subject:invoice has:attachment`.
        query: String,
        /// Maximum results.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Show the outbox and flush whatever is queued.
    Outbox {
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
        /// Only show the counts, do not flush.
        #[arg(long)]
        counts_only: bool,
    },

    /// List the devices signed in to an account.
    Devices {
        /// Which account.
        #[arg(long)]
        account: Option<i64>,
    },

    /// Inspect the client's configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}

/// Account subcommands.
#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Add an account, discovering the server when `--server` is omitted.
    Add {
        /// The address to sign in with.
        email: String,
        /// The password. Prefer `FERROMA_PASSWORD` or the prompt over argv.
        #[arg(long, env = "FERROMA_PASSWORD")]
        password: Option<String>,
        /// A friendly name for the sidebar.
        #[arg(long)]
        name: Option<String>,
        /// Skip autodiscovery and use this FCP base URL.
        #[arg(long)]
        server: Option<String>,
    },
    /// List the configured accounts.
    List,
    /// Remove an account and its whole cache.
    Remove {
        /// The account id.
        id: i64,
    },
    /// Pause or resume an account's syncing.
    Pause {
        /// The account id.
        id: i64,
        /// Resume instead of pausing.
        #[arg(long)]
        resume: bool,
    },
}

/// Configuration subcommands.
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the data directory and the cache file it holds.
    Path,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let _ = ferroma_core::logging::init(&cli.log, LogFormat::Text);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {}", err.user_message());
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), ClientError> {
    let data_dir = match &cli.data_dir {
        Some(dir) => dir.clone(),
        None => ferroma_client::ui::default_data_dir()?,
    };
    let handle = ClientHandle::open(&data_dir).await?;
    let json = cli.json;

    match cli.command {
        Command::Config(ConfigCommand::Path) => {
            let db = handle
                .database()
                .path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(in memory)".to_string());
            if json {
                print_json(&json!({ "data_dir": data_dir, "database": db }))?;
            } else {
                println!("data dir: {data_dir:?}");
                println!("database: {db}");
            }
            Ok(())
        }

        Command::Account(AccountCommand::Add {
            email,
            password,
            name,
            server,
        }) => {
            let password = match password {
                Some(password) => password,
                None => prompt_password(&email)?,
            };
            let account = handle
                .accounts()
                .add(&email, &password, name, server)
                .await?;
            if json {
                print_json(&account_json(&account))
            } else {
                println!(
                    "added account {} <{}> at {}",
                    account.id, account.email, account.base_url
                );
                Ok(())
            }
        }

        Command::Account(AccountCommand::List) => {
            let accounts = handle.list_accounts().await?;
            if json {
                let items: Vec<_> = accounts.iter().map(account_json).collect();
                print_json(&json!({ "accounts": items }))
            } else if accounts.is_empty() {
                println!("no accounts; try `ferroma-client account add you@example.com`");
                Ok(())
            } else {
                for account in accounts {
                    println!(
                        "{}  {:<32} {:<40} {}",
                        account.id,
                        account.email,
                        account.base_url,
                        if account.paused { "paused" } else { "active" }
                    );
                }
                Ok(())
            }
        }

        Command::Account(AccountCommand::Remove { id }) => {
            handle.accounts().remove(AccountId(id)).await?;
            if json {
                print_json(&json!({ "removed": id }))
            } else {
                println!("removed account {id} and its cache");
                Ok(())
            }
        }

        Command::Account(AccountCommand::Pause { id, resume }) => {
            let account = handle.accounts().pause(AccountId(id), !resume).await?;
            if json {
                print_json(&account_json(&account))
            } else {
                println!(
                    "account {} is now {}",
                    account.id,
                    if account.paused { "paused" } else { "active" }
                );
                Ok(())
            }
        }

        Command::Sync { account } => {
            let accounts = match account {
                Some(id) => vec![
                    handle
                        .accounts()
                        .get(AccountId(id))
                        .await?
                        .ok_or_else(|| ClientError::not_found(format!("no account {id}")))?,
                ],
                None => handle.list_accounts().await?,
            };
            if accounts.is_empty() {
                return Err(ClientError::not_found("no accounts to sync"));
            }
            let mut summary = Vec::new();
            for account in accounts {
                let result = handle.sync(account.id).await?;
                summary.push(json!({
                    "account_id": account.id.get(),
                    "applied": result.applied,
                    "streams": result.streams(),
                    "resynced_folders": result.resynced_folders,
                }));
                if !json {
                    println!(
                        "{}: applied {} change(s) across {} stream(s)",
                        account.email,
                        result.applied,
                        result.streams()
                    );
                }
            }
            if json {
                print_json(&json!({ "synced": summary }))
            } else {
                Ok(())
            }
        }

        Command::List {
            folder,
            limit,
            offset,
            account,
        } => {
            let account = resolve_account(&handle, account).await?;
            let folder_id = resolve_folder(&handle, account, &folder).await?;
            let messages = handle.messages(account, folder_id, limit, offset).await?;
            if json {
                let items: Vec<_> = messages.iter().map(message_json).collect();
                print_json(&json!({ "messages": items, "folder_id": folder_id }))
            } else if messages.is_empty() {
                println!("(no messages)");
                Ok(())
            } else {
                for message in messages {
                    println!(
                        "{:>8}  {}  {:<48} {}",
                        message.id,
                        message.internal_date.as_deref().unwrap_or("-"),
                        truncate(&message.list_label(), 48),
                        if message.is_read() { "" } else { "*" }
                    );
                }
                Ok(())
            }
        }

        Command::Read { id, account } => {
            let account = resolve_account(&handle, account).await?;
            let message = handle.open_message(account, id).await?;
            if json {
                print_json(&message_json(&message))
            } else {
                print_message(&message);
                Ok(())
            }
        }

        Command::Send {
            to,
            subject,
            body,
            from,
            account,
        } => {
            let account_id = resolve_account(&handle, account).await?;
            let account = handle
                .accounts()
                .get(account_id)
                .await?
                .ok_or_else(|| ClientError::not_found("the account disappeared"))?;
            let from_address = match from {
                Some(from) => from,
                None => primary_address(&handle, account.id)
                    .await?
                    .unwrap_or_else(|| account.email.clone()),
            };
            let item = NewOutboxItem::simple(account.id.get(), &from_address, &to, &subject, &body);
            let queued = handle.send(item).await?;

            // Try to send it right away. A failure leaves it in the outbox, which
            // is the whole point of the Outbox state machine.
            let submitted = flush_outbox(&handle, account.id, cli.no_retry).await;
            let applied = match &submitted {
                Ok(summary) => summary.operations_applied,
                Err(_) => 0,
            };
            if applied > 0 {
                println!("also sent {applied} queued operation(s)");
            }
            let sent_now = matches!(&submitted, Ok(summary) if summary.sent > 0);
            let detail = match &submitted {
                Ok(summary) if summary.sent > 0 => None,
                Ok(_) => Some("the server did not accept it yet".to_string()),
                Err(err) => Some(err.user_message()),
            };
            if json {
                print_json(&json!({
                    "outbox_id": queued.id,
                    "operation_id": queued.operation_id,
                    "state": queued.state.as_str(),
                    "sent_now": sent_now,
                    "detail": detail,
                }))
            } else {
                println!("queued as outbox item {} ({})", queued.id, queued.state);
                match detail {
                    None => println!("handed to the server"),
                    Some(reason) => println!(
                        "not sent yet ({reason}); it stays in the outbox and will be retried"
                    ),
                }
                Ok(())
            }
        }

        Command::Search {
            query,
            limit,
            account,
        } => {
            let account = resolve_account(&handle, account).await?;
            let results = handle.search(account, &query, limit).await?;
            if json {
                let hits: Vec<_> = results
                    .hits
                    .iter()
                    .map(|hit| {
                        json!({
                            "message_id": hit.message_id,
                            "folder_id": hit.folder_id,
                            "subject": hit.subject,
                            "from": hit.from_address,
                            "flags": hit.flags,
                            "internal_date": hit.internal_date,
                        })
                    })
                    .collect();
                print_json(&json!({
                    "source": match results.source {
                        ferroma_client::SearchSource::Local => "local",
                        ferroma_client::SearchSource::Server => "server",
                    },
                    "total": results.total,
                    "hits": hits,
                }))
            } else if results.hits.is_empty() {
                println!("(no matches)");
                Ok(())
            } else {
                for hit in &results.hits {
                    println!(
                        "{:>8}  {:<40}  {}",
                        hit.message_id,
                        truncate(&hit.subject, 40),
                        hit.from_address
                    );
                }
                println!("{} match(es)", results.total);
                Ok(())
            }
        }

        Command::Outbox {
            account,
            counts_only,
        } => {
            let account = resolve_account(&handle, account).await?;
            if !counts_only {
                let _ = flush_outbox(&handle, account, cli.no_retry).await;
            }
            let counts = handle.outbox_counts(account).await?;
            let items = if counts_only {
                Vec::new()
            } else {
                handle.outbox().list(account.get()).await?
            };
            if json {
                let rows: Vec<_> = items
                    .iter()
                    .map(|item| {
                        json!({
                            "id": item.id,
                            "state": item.state.as_str(),
                            "subject": item.subject,
                            "to": item.to,
                            "attempts": item.attempts,
                            "last_error": item.last_error,
                        })
                    })
                    .collect();
                print_json(&json!({
                    "draft": counts.draft,
                    "pending": counts.pending,
                    "uploading": counts.uploading,
                    "queued": counts.queued,
                    "sending": counts.sending,
                    "sent": counts.sent,
                    "failed": counts.failed,
                    "retrying": counts.retrying,
                    "summary": counts.summary(),
                    "items": rows,
                }))
            } else {
                println!("发件箱: {}", counts.summary());
                for item in items {
                    println!(
                        "{:>5}  {:<10} {:<40} {}",
                        item.id,
                        item.state,
                        truncate(&item.subject, 40),
                        item.to.join(", ")
                    );
                    if let Some(error) = &item.last_error {
                        println!("       last error: {error}");
                    }
                }
                Ok(())
            }
        }

        Command::Devices { account } => {
            let account = resolve_account(&handle, account).await?;
            let client = handle.accounts().client(account).await?;
            let devices = client.devices().await?;
            if json {
                let rows: Vec<_> = devices
                    .iter()
                    .map(|device| {
                        json!({
                            "id": device.id,
                            "device_uid": device.device_uid,
                            "name": device.name,
                            "platform": device.platform,
                            "client_version": device.client_version,
                            "last_seen_at": device.last_seen_at,
                            "last_ip": device.last_ip,
                            "revoked": device.revoked,
                        })
                    })
                    .collect();
                print_json(&json!({ "devices": rows }))
            } else {
                for device in devices {
                    println!(
                        "{:>5}  {:<24} {:<10} {:<10} {}",
                        device.id,
                        device.name.unwrap_or_else(|| "(unnamed)".into()),
                        device.platform.unwrap_or_default(),
                        device.client_version.unwrap_or_default(),
                        if device.revoked { "revoked" } else { "active" }
                    );
                }
                Ok(())
            }
        }
    }
}

/// What one flush pass did.
struct FlushSummary {
    /// Operations the server accepted and that were removed from the queue.
    operations_applied: usize,
    /// Outbox items the server accepted.
    sent: usize,
    /// Outbox items that failed and are waiting to be retried.
    failed: usize,
}

/// Flush the outbox and the offline queue for one account.
async fn flush_outbox(
    handle: &ClientHandle,
    account: AccountId,
    no_retry: bool,
) -> Result<FlushSummary, ClientError> {
    let retry = if no_retry {
        ferroma_client::RetryPolicy::none()
    } else {
        ferroma_client::RetryPolicy::default()
    };
    let client = handle.accounts().client_with_retry(account, retry).await?;

    // Offline operations first: they are the ones the user already made.
    let flusher = OperationFlusher::new(
        handle.database().clone(),
        std::sync::Arc::new(FcpSubmitter::new(client.clone())),
    );
    let report = flusher.flush(account.get()).await?;
    let mut summary = FlushSummary {
        operations_applied: report.applied.len(),
        sent: 0,
        failed: 0,
    };

    // Then whatever is sitting in the outbox.
    for state in [OutboxState::Pending, OutboxState::Retrying] {
        for item in handle.outbox().list_in_state(account.get(), state).await? {
            let mut current = item;
            if current.state == OutboxState::Pending {
                let next = if current.attachment_ids.is_empty() {
                    OutboxState::Queued
                } else {
                    OutboxState::Uploading
                };
                current = handle.outbox().transition(current.id, next).await?;
            }
            if current.state == OutboxState::Uploading {
                current = handle
                    .outbox()
                    .transition(current.id, OutboxState::Queued)
                    .await?;
            }
            let sending = handle
                .outbox()
                .transition(current.id, OutboxState::Sending)
                .await?;
            let request = ferroma_client::SendRequest {
                operation_id: sending.operation_id.clone(),
                mailbox_id: sending.mailbox_id,
                from: Some(sending.from_address.clone()),
                to: sending.to.clone(),
                cc: sending.cc.clone(),
                bcc: sending.bcc.clone(),
                subject: sending.subject.clone(),
                text: sending.text_body.clone(),
                html: sending.html_body.clone(),
                attachment_ids: sending.attachment_ids.clone(),
                in_reply_to: sending.in_reply_to.clone(),
                references: sending.references.clone(),
            };
            match client.send(&request).await {
                Ok(response) => {
                    handle
                        .outbox()
                        .mark_sent(sending.id, response.message_id, Some(response.queued))
                        .await?;
                    summary.sent += 1;
                }
                Err(err) => {
                    handle
                        .outbox()
                        .record_failure(sending.id, &err.user_message(), err.is_retryable())
                        .await?;
                    summary.failed += 1;
                }
            }
        }
    }

    if let Some(blocked) = report.blocked_on {
        tracing::warn!(operation = %blocked, "the offline queue is blocked; will retry later");
    }
    Ok(summary)
}

fn account_json(account: &ferroma_client::Account) -> serde_json::Value {
    json!({
        "id": account.id.get(),
        "email": account.email,
        "display_name": account.display_name,
        "base_url": account.base_url,
        "paused": account.paused,
        "sync_window_days": account.sync_window_days,
        "cache_limit_bytes": account.cache_limit_bytes,
        "protocol_version": account.protocol_version,
        "server_version": account.server_version,
        "last_sync_at": account.last_sync_at.map(|at| at.to_rfc3339()),
        "last_error": account.last_error,
    })
}

fn message_json(message: &MessageView) -> serde_json::Value {
    json!({
        "id": message.id,
        "folder_id": message.folder_id,
        "subject": message.subject,
        "from": message.from_address,
        "from_name": message.from_name,
        "to": message.to_summary,
        "snippet": message.snippet,
        "flags": message.flags,
        "internal_date": message.internal_date,
        "has_attachments": message.has_attachments,
        "attachment_count": message.attachment_count,
        "body_state": message.body_state,
        "read": message.is_read(),
        "flagged": message.is_flagged(),
    })
}

fn print_message(message: &MessageView) {
    println!("From:    {}", message.list_label());
    if let Some(to) = &message.to_summary {
        println!("To:      {to}");
    }
    if let Some(date) = &message.internal_date {
        println!("Date:    {date}");
    }
    println!("Flags:   {}", if message.flags.is_empty() { "-" } else { &message.flags });
    println!();
    if let Some(body) = &message.text_body {
        println!("{body}");
    } else if message.html_body.is_some() {
        println!("(HTML only; use --json to get the markup)");
    } else {
        println!("(body not cached)");
    }
}

async fn resolve_account(handle: &ClientHandle, id: Option<i64>) -> Result<AccountId, ClientError> {
    match id {
        Some(id) => Ok(AccountId(id)),
        None => handle
            .list_accounts()
            .await?
            .first()
            .map(|account| account.id)
            .ok_or_else(|| ClientError::not_found("no accounts configured")),
    }
}

/// Resolve a folder by name (case-insensitive) or by numeric id.
async fn resolve_folder(
    handle: &ClientHandle,
    account: AccountId,
    folder: &str,
) -> Result<i64, ClientError> {
    if let Ok(id) = folder.parse::<i64>() {
        return Ok(id);
    }
    let folders = handle.folders(account).await?;
    folders
        .iter()
        .find(|view| view.name.eq_ignore_ascii_case(folder))
        .map(|view| view.id)
        .or_else(|| {
            folders
                .iter()
                .find(|view| view.name.to_ascii_lowercase().ends_with(&folder.to_ascii_lowercase()))
                .map(|view| view.id)
        })
        .ok_or_else(|| ClientError::not_found(format!("no folder named {folder:?}")))
}

/// The account's primary address, for the `From:` line.
async fn primary_address(
    handle: &ClientHandle,
    account: AccountId,
) -> Result<Option<String>, ClientError> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT address FROM mailboxes WHERE account_id = ? ORDER BY is_primary DESC, id ASC LIMIT 1",
    )
    .bind(account.get())
    .fetch_optional(handle.database().pool())
    .await?;
    Ok(row.map(|row| row.get::<String, _>("address")))
}

/// Read a password from stdin without echoing it.
fn prompt_password(email: &str) -> Result<String, ClientError> {
    use std::io::Write;
    print!("password for {email}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']).to_string();
    if password.is_empty() {
        return Err(ClientError::invalid(
            "no password given (use --password, FERROMA_PASSWORD, or the prompt)",
        ));
    }
    Ok(password)
}

fn print_json(value: &serde_json::Value) -> Result<(), ClientError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|err| ClientError::parse(format!("could not render json: {err}")))?
    );
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let head: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("cli parses")
    }

    #[test]
    fn the_subcommands_parse() {
        let cli = parse(&["ferroma-client", "config", "path"]);
        assert!(matches!(cli.command, Command::Config(ConfigCommand::Path)));

        let cli = parse(&["ferroma-client", "account", "list"]);
        assert!(matches!(cli.command, Command::Account(AccountCommand::List)));

        let cli = parse(&["ferroma-client", "list", "INBOX", "--limit", "5"]);
        match cli.command {
            Command::List { folder, limit, .. } => {
                assert_eq!(folder, "INBOX");
                assert_eq!(limit, 5);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = parse(&[
            "ferroma-client",
            "send",
            "--to",
            "a@b.c,c@d.e",
            "--subject",
            "Hi",
            "--body",
            "hello",
        ]);
        match cli.command {
            Command::Send { to, subject, body, .. } => {
                assert_eq!(to, vec!["a@b.c".to_string(), "c@d.e".to_string()]);
                assert_eq!(subject, "Hi");
                assert_eq!(body, "hello");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn global_flags_are_accepted_anywhere() {
        let cli = parse(&["ferroma-client", "--json", "--data-dir", "D:/tmp", "account", "list"]);
        assert!(cli.json);
        assert_eq!(cli.data_dir, Some(PathBuf::from("D:/tmp")));

        let cli = parse(&["ferroma-client", "outbox", "--json"]);
        assert!(cli.json);
        assert!(!cli.no_retry);
    }

    #[test]
    fn send_requires_a_recipient() {
        assert!(Cli::try_parse_from(["ferroma-client", "send", "--subject", "x"]).is_err());
    }

    #[tokio::test]
    async fn a_folder_can_be_resolved_by_name_id_or_suffix() {
        let dir = TempDir::new().expect("temp dir");
        let handle = ClientHandle::open(dir.path().join("data")).await.expect("open");
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(handle.database().pool())
        .await
        .expect("account");
        sqlx::query(
            "INSERT INTO folders (account_id, id, mailbox_id, name) VALUES (1, 5, 3, 'Archive/Newsletter')",
        )
        .execute(handle.database().pool())
        .await
        .expect("folder");

        // A numeric argument is an id, never a folder name.
        assert_eq!(resolve_folder(&handle, AccountId(1), "5").await.expect("id"), 5);
        assert_eq!(
            resolve_folder(&handle, AccountId(1), "archive/newsletter").await.expect("name"),
            5
        );
        assert_eq!(
            resolve_folder(&handle, AccountId(1), "newsletter").await.expect("suffix"),
            5
        );
        assert!(resolve_folder(&handle, AccountId(1), "nope").await.is_err());
    }

    #[tokio::test]
    async fn an_account_is_resolved_to_the_first_one_by_default() {
        let dir = TempDir::new().expect("temp dir");
        let handle = ClientHandle::open(dir.path().join("data")).await.expect("open");
        assert!(resolve_account(&handle, None).await.is_err(), "no accounts yet");
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (7, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(handle.database().pool())
        .await
        .expect("account");
        assert_eq!(resolve_account(&handle, None).await.expect("first").get(), 7);
        assert_eq!(resolve_account(&handle, Some(3)).await.expect("explicit").get(), 3);
    }

    #[tokio::test]
    async fn the_primary_address_is_read_from_the_cache() {
        let dir = TempDir::new().expect("temp dir");
        let handle = ClientHandle::open(dir.path().join("data")).await.expect("open");
        sqlx::query(
            "INSERT INTO accounts (id, email, base_url, device_uid, created_at, updated_at)
             VALUES (1, 'a@b.c', 'http://x/api/v1/client', 'dev', 'now', 'now')",
        )
        .execute(handle.database().pool())
        .await
        .expect("account");
        sqlx::query(
            "INSERT INTO mailboxes (account_id, id, address, is_primary) VALUES (1, 3, 'a@b.c', 1)",
        )
        .execute(handle.database().pool())
        .await
        .expect("mailbox");
        sqlx::query(
            "INSERT INTO mailboxes (account_id, id, address, is_primary) VALUES (1, 4, 'alias@b.c', 0)",
        )
        .execute(handle.database().pool())
        .await
        .expect("mailbox");
        assert_eq!(
            primary_address(&handle, AccountId(1)).await.expect("address"),
            Some("a@b.c".to_string())
        );
    }

    #[test]
    fn truncation_is_by_characters_not_bytes() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("电子邮件地址", 3), "电子…");
    }

    #[test]
    fn a_message_renders_without_bodies() {
        let view = MessageView {
            account_id: AccountId(1),
            id: 1,
            folder_id: Some(5),
            subject: "Hi".into(),
            from_address: "bob@example.net".into(),
            from_name: Some("Bob".into()),
            to_summary: Some("alice@example.com".into()),
            snippet: None,
            flags: "seen".into(),
            internal_date: Some("2026-09-16T09:12:44Z".into()),
            has_attachments: false,
            attachment_count: 0,
            body_state: "pending".into(),
            text_body: None,
            html_body: None,
        };
        let value = message_json(&view);
        assert_eq!(value["id"], 1);
        assert_eq!(value["read"], true);
        assert_eq!(value["flagged"], false);
        print_message(&view);
    }
}
