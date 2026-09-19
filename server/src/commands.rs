//! Implementations for every `ferroma` subcommand except `serve`.
//!
//! These are not wrappers around SQL: they go through the same repositories, the
//! same `Maildir` and the same `AuthService` the server uses, so an account created
//! here is byte-for-byte the account the Admin panel would have created — same
//! Argon2id parameters, same six standard folders, same Maildir layout.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};

use ferroma_auth::{AuthService, TokenService};
use ferroma_core::config::Config;
use ferroma_core::{EmailAddress, UserId};
use ferroma_storage::repository::NewMailbox;
use ferroma_storage::{AttachmentStore, Database, Maildir, Repositories};
use ferroma_sync::SyncService;

use crate::cli::{
    DkimCommand, DomainCommand, HealthcheckArgs, StorageCommand, SyncCommand, UserCommand,
    UserCreateArgs,
};

// -----------------------------------------------------------------------------
// Shared helpers
// -----------------------------------------------------------------------------

/// Connect, and migrate unless the configuration says the server owns migrations.
async fn open(config: &Config, migrate: bool) -> Result<Database> {
    let db = Database::connect(&config.database)
        .await
        .with_context(|| format!("connecting to {}", "the configured PostgreSQL"))?;

    if migrate || config.database.run_migrations {
        db.migrate().await.context("applying migrations")?;
    }
    Ok(db)
}

/// An `AuthService` over the given repositories, with production Argon2 parameters.
fn auth_service(config: &Config, repos: Repositories) -> Result<AuthService> {
    let tokens = TokenService::from_config(&config.api, &config.server.hostname)?;
    Ok(AuthService::with_defaults(repos, tokens, config.limits.clone()))
}

fn sync_service(config: &Config, repos: Repositories) -> SyncService {
    SyncService::new(
        repos,
        config.client.sync_page_size,
        config.client.tombstone_retention_days,
    )
}

/// Read a secret from `--flag`, or from stdin when the flag is absent.
///
/// Reading from stdin rather than prompting keeps the password out of shell history
/// and out of `ps`, which matters more than the convenience of a prompt:
/// `printf '%s' "$PW" | ferroma user create …`.
fn secret(flag: Option<&String>, what: &str) -> Result<String> {
    if let Some(value) = flag {
        if value.is_empty() {
            bail!("{what} must not be empty");
        }
        return Ok(value.clone());
    }

    if std::io::stdin().is_terminal() {
        eprint!("{what} (read from stdin; end with a newline): ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .with_context(|| format!("reading {what} from stdin"))?;
    let line = line.trim_end_matches(['\n', '\r']).to_string();
    if line.is_empty() {
        bail!("{what} must not be empty");
    }
    Ok(line)
}

/// Confirm a destructive action. `--yes` skips it; a non-interactive shell without
/// `--yes` refuses rather than guessing.
fn confirm(yes: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("{prompt}: refusing to proceed non-interactively; pass --yes to confirm");
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if !line.trim().eq_ignore_ascii_case("y") {
        bail!("aborted");
    }
    Ok(())
}

fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// -----------------------------------------------------------------------------
// config
// -----------------------------------------------------------------------------

/// `ferroma config check`
pub fn config_check(config: &Config, dns_domain: Option<&str>) -> Result<ExitCode> {
    // `Config::load` already validated; reaching here means it passed. Say what was
    // checked, so a green result is informative rather than merely reassuring.
    println!("configuration is valid");
    println!("  hostname     {}", config.server.hostname);
    println!("  data dir     {}", config.server.data_dir.display());
    println!("  maildir      {}", config.maildir_root().display());
    println!("  attachments  {}", config.attachment_root().display());
    println!(
        "  listeners    smtp={} imap={} api={} tls={}",
        if config.smtp.enabled { "on" } else { "off" },
        if config.imap.enabled { "on" } else { "off" },
        if config.api.enabled { "on" } else { "off" },
        if config.tls.enabled { "on" } else { "off" },
    );
    println!(
        "  database     {}",
        ferroma_storage::database::redact(&config.database.url)
    );
    println!(
        "  limits       max message {}, max recipients {}, mailbox quota {}",
        human_bytes(config.limits.max_message_size as i64),
        config.limits.max_recipients,
        human_bytes(config.limits.mailbox_quota as i64),
    );

    if config.api.jwt_secret.is_none() {
        println!();
        println!("warning: api.jwt_secret is unset, so sessions will not survive a restart.");
        println!("         Set FERROMA_JWT_SECRET before running in production.");
    }

    check_dns(config, dns_domain)?;
    Ok(ExitCode::SUCCESS)
}

/// Probe the resolver, because a silent DNS failure shows up as mysteriously slow
/// inbound mail rather than as an error.
///
/// The inbound SPF/DKIM/DMARC step runs *before* the SMTP reply, so its DNS lookups
/// are on the critical path of every message. A resolver that never answers is paid
/// for in `timeout_secs x (attempts + 1)` per nameserver until the policy budget
/// expires — which looks like "mail takes a minute to arrive" and gives no hint why.
/// Asking once at configuration time turns that into a sentence.
fn check_dns(config: &Config, domain: Option<&str>) -> Result<()> {
    let policy_on = config.policy.spf_enabled || config.policy.dmarc_enabled || config.dkim.verify_inbound;
    if !policy_on {
        println!();
        println!("dns          not checked: inbound authentication is disabled, so no");
        println!("             lookup is on the message path");
        return Ok(());
    }

    let runtime = tokio::runtime::Runtime::new()?;
    // Hoisted out of the async block so both the success and failure messages can
    // name what was actually queried.
    let probe_name = domain.unwrap_or("gmail.com").to_string();

    let probe = runtime.block_on(async {
        use ferroma_smtp::mx::{HickoryResolver, Resolver as _};

        let resolver = HickoryResolver::new(&config.dns).map_err(|err| format!("{err}"))?;
        // The budget is the resolver's own configured patience plus a margin, so a
        // hanging lookup is reported quickly instead of at the policy deadline.
        let budget = std::time::Duration::from_secs(config.dns.timeout_secs.clamp(1, 10) + 2);
        let started = std::time::Instant::now();
        match tokio::time::timeout(budget, resolver.mx(&probe_name)).await {
            Ok(Ok(lookup)) => Ok((started.elapsed(), lookup.len())),
            Ok(Err(err)) => Err(format!("{err}")),
            Err(_) => Err(format!("no answer within {}s", budget.as_secs())),
        }
    });

    println!();
    match probe {
        Ok((elapsed, hosts)) => {
            println!(
                "dns          ok ({} MX host(s) for {} in {} ms)",
                hosts,
                probe_name,
                elapsed.as_millis()
            );
            if elapsed.as_millis() > 1000 {
                println!(
                    "             note: that is slow. Every inbound message pays this once"
                );
                println!("             per lookup before it is acknowledged.");
            }
        }
        Err(reason) => {
            let worst = config.dns.timeout_secs
                * (config.dns.attempts as u64 + 1)
                * (4 + config.policy.spf_max_lookups as u64);
            println!("dns          FAILED for {probe_name}: {reason}");
            println!();
            println!("warning: the resolver did not answer. Inbound authentication runs before the");
            println!("         SMTP reply, so every message will stall for up to {}s while it tries.", worst.min(60));
            println!("         Either point [dns] resolvers at a working server, lower");
            println!("         dns.timeout_secs, or disable the policy step by clearing");
            println!("         policy.spf_enabled / policy.dmarc_enabled and dkim.verify_inbound.");
        }
    }
    Ok(())
}

/// `ferroma config show`
pub fn config_show(config: &Config) -> Result<ExitCode> {
    print!("{}", config.to_toml()?);
    Ok(ExitCode::SUCCESS)
}

// -----------------------------------------------------------------------------
// migrate
// -----------------------------------------------------------------------------

/// `ferroma migrate`
pub fn migrate(config: &Config) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // Create the database first. Without this, a fresh PostgreSQL answers
        // `database "ferroma" does not exist` and the operator has to go and find
        // `psql` — which is the first thing that goes wrong on a new install.
        create_database_if_missing(config).await?;

        let db = open(config, true).await?;
        let (total, applied) = db.migration_status().await?;
        println!("{applied} of {total} migrations applied");
        if applied < total {
            bail!("{} migration(s) still pending", total - applied);
        }
        println!("{}", db.server_version().await?);
        Ok(())
    })
}

/// Create the configured database when it does not exist yet.
async fn create_database_if_missing(config: &Config) -> Result<bool> {
    let name = Database::database_name(&config.database.url)?;
    match Database::ensure_database_exists(&config.database.url).await {
        Ok(true) => {
            println!("created database {name}");
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(err) => Err(anyhow!("{err}")),
    }
}

/// `ferroma database …`
pub fn database(config: &Config, command: &crate::cli::DatabaseCommand) -> Result<ExitCode> {
    use crate::cli::DatabaseCommand;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        match command {
            DatabaseCommand::Init => {
                let name = Database::database_name(&config.database.url)?;
                create_database_if_missing(config).await?;

                let db = open(config, true).await?;
                let (total, applied) = db.migration_status().await?;
                println!("database  {name} ({} of {total} migrations applied)", applied);
                if applied < total {
                    bail!("{} migration(s) still pending", total - applied);
                }
                println!();
                println!("Next:");
                println!("  ferroma domain create <your-domain>");
                println!("  ferroma user create you@<your-domain> --admin");
                println!("  ferroma serve");
                Ok(ExitCode::SUCCESS)
            }
            DatabaseCommand::Status => {
                // Connect without migrating: a status command must not change anything.
                let db = Database::connect(&config.database)
                    .await
                    .map_err(|e| anyhow!("{e}"))?;
                println!("url          {}", db.redacted_url());
                println!("server       {}", db.server_version().await?);
                println!("size         {}", human_bytes(db.size_bytes().await?));
                let (total, applied) = db.migration_status().await?;
                println!("migrations   {applied}/{total}");
                let stats = db.pool_stats();
                println!("pool         {}/{} ({} idle)", stats.size, stats.max, stats.idle);
                match db.health().await {
                    Ok(()) => println!("health       ok"),
                    Err(err) => println!("health       FAILED: {err}"),
                }
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

// -----------------------------------------------------------------------------
// user
// -----------------------------------------------------------------------------

/// `ferroma user …`
pub fn user(config: &Config, command: &UserCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let db = open(config, false).await?;
        let repos = db.repositories();

        match command {
            UserCommand::Create(args) => user_create(config, &db, &repos, args).await,
            UserCommand::List(args) => {
                let users = repos.users.list(args.limit, args.offset).await?;
                if args.emails_only {
                    for user in users {
                        println!("{}", user.email);
                    }
                } else {
                    println!(
                        "{:<32} {:>10} {:>10} {:>6} {:>5}",
                        "EMAIL", "USED", "QUOTA", "ADMIN", "STATE"
                    );
                    for user in users {
                        println!(
                            "{:<32} {:>10} {:>10} {:>6} {:>5}",
                            user.email,
                            human_bytes(user.used_bytes),
                            human_bytes(user.quota_bytes),
                            if user.is_admin { "yes" } else { "no" },
                            if user.enabled { "on" } else { "off" },
                        );
                    }
                }
                let total = repos.users.count().await?;
                if !args.emails_only {
                    println!("\n{total} account(s)");
                }
                Ok(ExitCode::SUCCESS)
            }
            UserCommand::Password(args) => {
                let user = repos
                    .users
                    .find_by_email(&args.email)
                    .await?
                    .ok_or_else(|| anyhow!("no such account: {}", args.email))?;
                let password = secret(args.password.as_ref(), "new password")?;
                let auth = auth_service(config, repos.clone())?;
                let hash = auth.hash_password(&password).await?;
                repos.users.update_password(UserId::new(user.id), &hash).await?;
                let revoked = if args.keep_sessions {
                    0
                } else {
                    repos.sessions.revoke_all_for_user(UserId::new(user.id)).await?
                };
                println!(
                    "password updated for {}{}",
                    user.email,
                    if revoked > 0 {
                        format!("; {revoked} session(s) revoked")
                    } else {
                        String::new()
                    }
                );
                Ok(ExitCode::SUCCESS)
            }
            UserCommand::SetEnabled { email, enabled } => {
                let user = repos
                    .users
                    .find_by_email(email)
                    .await?
                    .ok_or_else(|| anyhow!("no such account: {email}"))?;
                repos.users.set_enabled(UserId::new(user.id), *enabled).await?;
                if !enabled {
                    repos.sessions.revoke_all_for_user(UserId::new(user.id)).await?;
                }
                println!("{} is now {}", user.email, if *enabled { "enabled" } else { "disabled" });
                Ok(ExitCode::SUCCESS)
            }
            UserCommand::SetAdmin { email, admin } => {
                let user = repos
                    .users
                    .find_by_email(email)
                    .await?
                    .ok_or_else(|| anyhow!("no such account: {email}"))?;
                // Refuse to remove the last administrator: it locks everyone out of
                // the Admin panel, and there is no recovery path from the CLI that
                // does not involve editing the database by hand.
                if !admin && user.is_admin && repos.users.count_admins().await? <= 1 {
                    bail!("{} is the last administrator; promote someone else first", user.email);
                }
                repos.users.set_admin(UserId::new(user.id), *admin).await?;
                println!("{} admin={}", user.email, admin);
                Ok(ExitCode::SUCCESS)
            }
            UserCommand::Delete { email, yes } => {
                let user = repos
                    .users
                    .find_by_email(email)
                    .await?
                    .ok_or_else(|| anyhow!("no such account: {email}"))?;
                if user.is_admin && repos.users.count_admins().await? <= 1 {
                    bail!("{} is the last administrator; refusing to delete", user.email);
                }
                confirm(
                    *yes,
                    &format!(
                        "delete {} and every address, folder and message it owns?",
                        user.email
                    ),
                )?;
                repos.users.delete(UserId::new(user.id)).await?;
                println!("deleted {} (id {})", user.email, user.id);
                println!("note: the Maildir on disk was left in place; see `ferroma storage verify`");
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

async fn user_create(
    config: &Config,
    db: &Database,
    repos: &Repositories,
    args: &UserCreateArgs,
) -> Result<ExitCode> {
    let address = EmailAddress::parse(&args.email)?;
    let password = secret(args.password.as_ref(), "password")?;

    // The address's domain has to exist before the mailbox can reference it.
    let domain = match repos.domains.find_by_name(address.domain()).await? {
        Some(domain) => domain,
        None if args.create_domain => {
            let domain = repos
                .domains
                .create(address.domain(), None)
                .await
                ?;
            println!("created domain {}", domain.name);
            domain
        }
        None => bail!(
            "domain {} does not exist; create it with `ferroma domain create {}` or pass --create-domain",
            address.domain(),
            address.domain()
        ),
    };

    let auth = auth_service(config, repos.clone())?;
    let quota = args.quota.or(Some(config.limits.mailbox_quota as i64));
    let user = auth
        .create_user(
            &address.to_lowercase(),
            &password,
            args.display_name.as_deref(),
            args.admin,
            !args.disabled,
            quota,
        )
        .await?;

    let mailbox = repos
        .mailboxes
        .create(NewMailbox {
            user_id: UserId::new(user.id),
            domain_id: ferroma_core::DomainId::new(domain.id),
            local_part: address.local_part().to_ascii_lowercase(),
            display_name: args.display_name.clone(),
            is_primary: true,
            quota_bytes: None,
        })
        .await
        ?;

    // The six standard IMAP folders, and the Maildir behind them, so the account can
    // receive mail the moment it exists.
    let folders = repos
        .folders
        .ensure_standard(ferroma_core::MailboxId::new(mailbox.id))
        .await
        ?;

    let maildir = Maildir::new(
        config.maildir_root(),
        config.storage.fsync_on_write,
        config.storage.layout,
    );
    maildir
        .ensure_mailbox(address.domain(), address.local_part())
        .map_err(|e| anyhow!("creating the Maildir: {e}"))?;

    println!("created account {}", user.email);
    println!("  user id     {}", user.id);
    println!("  address     {address}");
    println!("  quota       {}", human_bytes(user.quota_bytes));
    println!("  folders     {}", folders.len());
    println!("  maildir     {}", maildir.mailbox_dir(address.domain(), address.local_part())?.display());
    if args.admin {
        println!("  role        administrator");
    }
    let _ = db;
    Ok(ExitCode::SUCCESS)
}

// -----------------------------------------------------------------------------
// domain
// -----------------------------------------------------------------------------

/// `ferroma domain …`
pub fn domain(config: &Config, command: &DomainCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let db = open(config, false).await?;
        let repos = db.repositories();

        match command {
            DomainCommand::Create { name, description } => {
                let domain = repos
                    .domains
                    .create(name, description.as_deref())
                    .await
                    ?;
                println!("created domain {} (id {})", domain.name, domain.id);
                println!();
                println!("publish these records for {}:", domain.name);
                println!("  {} MX 10 {}", domain.name, config.server.hostname);
                println!("  {} TXT \"v=spf1 mx -all\"", domain.name);
                println!("  _dmarc.{} TXT \"v=DMARC1; p=quarantine; rua=mailto:postmaster@{}\"", domain.name, domain.name);
                println!();
                println!("then run `ferroma domain dns {}` to check them", domain.name);
                Ok(ExitCode::SUCCESS)
            }
            DomainCommand::List => {
                let domains = repos.domains.list().await?;
                println!("{:<32} {:>6} {:>8}", "DOMAIN", "STATE", "ALIASES");
                for domain in &domains {
                    let aliases = repos
                        .aliases
                        .list_by_domain(ferroma_core::DomainId::new(domain.id))
                        .await?
                        .len();
                    println!(
                        "{:<32} {:>6} {:>8}",
                        domain.name,
                        if domain.enabled { "on" } else { "off" },
                        aliases
                    );
                }
                println!("\n{} domain(s)", domains.len());
                Ok(ExitCode::SUCCESS)
            }
            DomainCommand::SetEnabled { name, enabled } => {
                let domain = repos
                    .domains
                    .find_by_name(name)
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {name}"))?;
                repos
                    .domains
                    .set_enabled(ferroma_core::DomainId::new(domain.id), *enabled)
                    .await?;
                println!("{} is now {}", domain.name, if *enabled { "enabled" } else { "disabled" });
                Ok(ExitCode::SUCCESS)
            }
            DomainCommand::Delete { name, force } => {
                let domain = repos
                    .domains
                    .find_by_name(name)
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {name}"))?;
                let addresses = repos
                    .mailboxes
                    .list_by_domain(ferroma_core::DomainId::new(domain.id))
                    .await?;
                if !addresses.is_empty() && !force {
                    bail!(
                        "{} still owns {} address(es); delete them first or pass --force",
                        domain.name,
                        addresses.len()
                    );
                }
                confirm(*force, &format!("delete domain {} and its {} address(es)?", domain.name, addresses.len()))?;
                repos
                    .domains
                    .delete(ferroma_core::DomainId::new(domain.id))
                    .await?;
                println!("deleted domain {}", domain.name);
                Ok(ExitCode::SUCCESS)
            }
            DomainCommand::AddAlias { address, target } => {
                let address = EmailAddress::parse(address)?;
                let domain = repos
                    .domains
                    .find_by_name(address.domain())
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {}", address.domain()))?;
                let alias = repos
                    .aliases
                    .create(
                        ferroma_core::DomainId::new(domain.id),
                        address.local_part(),
                        target,
                    )
                    .await
                    ?;
                println!("{} -> {} (id {})", address, alias.target, alias.id);
                Ok(ExitCode::SUCCESS)
            }
            DomainCommand::ListAliases { name } => {
                let domain = repos
                    .domains
                    .find_by_name(name)
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {name}"))?;
                let aliases = repos
                    .aliases
                    .list_by_domain(ferroma_core::DomainId::new(domain.id))
                    .await?;
                for alias in &aliases {
                    println!(
                        "{}@{} -> {}{}",
                        alias.local_part,
                        domain.name,
                        alias.target,
                        if alias.enabled { "" } else { "  (disabled)" }
                    );
                }
                println!("\n{} alias(es)", aliases.len());
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

// -----------------------------------------------------------------------------
// dkim
// -----------------------------------------------------------------------------

/// `ferroma dkim …`
pub fn dkim(config: &Config, command: &DkimCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let db = open(config, false).await?;
        let repos = db.repositories();

        match command {
            DkimCommand::Generate {
                domain,
                selector,
                out,
                force,
            } => {
                let record = repos
                    .domains
                    .find_by_name(domain)
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {domain}"))?;
                if record.dkim_private_key.is_some() && !force {
                    bail!(
                        "{} already has a DKIM key (selector {}); pass --force to replace it. \
                         Rotating a key while the old TXT record is still published will make \
                         your mail fail DKIM verification until DNS catches up.",
                        record.name,
                        record.dkim_selector.as_deref().unwrap_or("unknown")
                    );
                }

                let selector = selector
                    .clone()
                    .unwrap_or_else(|| config.dkim.selector.clone());

                // 2048-bit RSA-SHA256, the size every receiver accepts.
                let mut rng = rand::thread_rng();
                let private = rsa::RsaPrivateKey::new(&mut rng, 2048)
                    .context("generating an RSA key")?;
                let public = rsa::RsaPublicKey::from(&private);

                use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
                let private_pem = private
                    .to_pkcs8_pem(LineEnding::LF)
                    .context("encoding the private key")?;
                let public_der = public
                    .to_public_key_der()
                    .context("encoding the public key")?;

                use base64::Engine as _;
                let p = base64::engine::general_purpose::STANDARD.encode(public_der.as_bytes());
                let record_value = format!("v=DKIM1; k=rsa; p={p}");

                repos
                    .domains
                    .set_dkim(
                        ferroma_core::DomainId::new(record.id),
                        Some(&selector),
                        Some(private_pem.as_str()),
                        Some(&record_value),
                    )
                    .await
                    ?;

                if let Some(path) = out {
                    std::fs::write(path, private_pem.as_bytes())
                        .with_context(|| format!("writing {}", path.display()))?;
                    // A private key that anyone can read is not a private key.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                    }
                    println!("private key written to {}", path.display());
                    println!("point [dkim] private_key_path at it, or rely on the database copy");
                }

                println!("generated a 2048-bit DKIM key for {}", record.name);
                println!();
                println!("publish this record:");
                println!("  {selector}._domainkey.{}  TXT  \"{}\"", record.name, record_value);
                println!();
                println!("then check it with `ferroma dkim show {}`", record.name);
                Ok(ExitCode::SUCCESS)
            }
            DkimCommand::Show { domain, selector } => {
                let record = repos
                    .domains
                    .find_by_name(domain)
                    .await?
                    .ok_or_else(|| anyhow!("no such domain: {domain}"))?;
                let selector = selector
                    .clone()
                    .or_else(|| record.dkim_selector.clone())
                    .unwrap_or_else(|| config.dkim.selector.clone());
                let value = record.dkim_public_key.ok_or_else(|| {
                    anyhow!(
                        "{} has no DKIM key yet; run `ferroma dkim generate --domain {}`",
                        record.name,
                        record.name
                    )
                })?;
                println!("{selector}._domainkey.{}  TXT  \"{value}\"", record.name);
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

// -----------------------------------------------------------------------------
// storage
// -----------------------------------------------------------------------------

/// `ferroma storage …`
pub fn storage(config: &Config, command: &StorageCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let db = open(config, false).await?;
        let repos = db.repositories();
        let maildir = Maildir::new(
            config.maildir_root(),
            config.storage.fsync_on_write,
            config.storage.layout,
        );
        let attachments = AttachmentStore::new(config.attachment_root(), config.storage.fsync_on_write);

        match command {
            StorageCommand::Stats => {
                let users = repos.users.count().await?;
                let domains = repos.domains.count().await?;
                let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
                    .fetch_one(db.pool())
                    .await?;
                let bytes: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(size_bytes), 0)::bigint FROM messages",
                )
                .fetch_one(db.pool())
                .await?;

                println!("accounts        {users}");
                println!("domains         {domains}");
                println!("messages        {messages}");
                println!("message bytes   {}", human_bytes(bytes));
                println!("database        {}", human_bytes(db.size_bytes().await?));
                println!("attachments     {}", human_bytes(attachments.total_size()? as i64));
                println!("maildir root    {}", maildir.root().display());
                let (total, applied) = db.migration_status().await?;
                println!("migrations      {applied}/{total}");
                Ok(ExitCode::SUCCESS)
            }
            StorageCommand::Verify { details } => {
                // Every row must have its file, and every file should have its row.
                // A row without a file is a message the user can see but not open;
                // a file without a row is space nobody will ever reclaim.
                let rows: Vec<(i64, String, i64)> =
                    sqlx::query_as("SELECT id, storage_path, size_bytes FROM messages")
                        .fetch_all(db.pool())
                        .await?;

                let mut missing = Vec::new();
                let mut size_mismatch = Vec::new();
                for (id, path, expected) in &rows {
                    match std::fs::metadata(maildir.absolute(path)?) {
                        Ok(meta) => {
                            if meta.len() != *expected as u64 {
                                size_mismatch.push((*id, path.clone(), *expected, meta.len()));
                            }
                        }
                        Err(_) => missing.push((*id, path.clone())),
                    }
                }

                println!("checked   {} message row(s)", rows.len());
                println!("missing   {}", missing.len());
                println!("size      {} mismatch(es)", size_mismatch.len());

                if *details {
                    for (id, path) in missing.iter().take(50) {
                        println!("  missing: message {id} -> {path}");
                    }
                    for (id, path, expected, actual) in size_mismatch.iter().take(50) {
                        println!("  size: message {id} {path}: row {expected}, file {actual}");
                    }
                }

                let swept = maildir.sweep_tmp(3600)?;
                println!("swept     {swept} abandoned tmp/ file(s)");

                if missing.is_empty() && size_mismatch.is_empty() {
                    println!("\nmail store is consistent");
                    Ok(ExitCode::SUCCESS)
                } else {
                    println!("\nmail store is NOT consistent; see the entries above");
                    Ok(ExitCode::FAILURE)
                }
            }
            StorageCommand::Gc { dry_run } => {
                let keep: std::collections::HashSet<String> =
                    repos.attachments.referenced_paths().await?.into_iter().collect();

                if *dry_run {
                    // Count without deleting: `gc` with an empty keep-set would be a
                    // disaster if the caller meant "dry run" and we deleted anyway.
                    let mut would_remove = 0usize;
                    let mut bytes = 0u64;
                    for entry in walkdir::WalkDir::new(attachments.root()).into_iter().flatten() {
                        if !entry.file_type().is_file() {
                            continue;
                        }
                        let name = entry.file_name().to_string_lossy().to_string();
                        if keep.contains(&name) {
                            continue;
                        }
                        would_remove += 1;
                        bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                    println!(
                        "would remove {would_remove} blob(s), reclaiming {}",
                        human_bytes(bytes as i64)
                    );
                    println!("{} referenced blob(s) would be kept", keep.len());
                    return Ok(ExitCode::SUCCESS);
                }

                let removed = attachments.gc(&keep)?;
                let swept = maildir.sweep_tmp(3600)?;
                println!("removed {removed} unreferenced attachment blob(s)");
                println!("swept   {swept} abandoned tmp/ file(s)");
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

// -----------------------------------------------------------------------------
// sync
// -----------------------------------------------------------------------------

/// `ferroma sync …`
pub fn sync(config: &Config, command: &SyncCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let db = open(config, false).await?;
        let repos = db.repositories();
        let service = sync_service(config, repos);

        match command {
            SyncCommand::Prune => {
                let changes = service.prune().await?;
                let operations = service.prune_operations().await?;
                println!("pruned {changes} change-log entry(ies)");
                println!("pruned {operations} recorded operation(s)");
                println!(
                    "retention window: {} day(s), from client.tombstone_retention_days",
                    config.client.tombstone_retention_days
                );
                println!();
                println!("a client whose cursor predates what remains will be told to resync;");
                println!("that is the designed behaviour, not data loss.");
                Ok(ExitCode::SUCCESS)
            }
        }
    })
}

// -----------------------------------------------------------------------------
// doctor
// -----------------------------------------------------------------------------

/// The verdict of one preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Warn,
    Fail,
    /// Not applicable to this configuration, so not a problem either way.
    Skip,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Warn => "warn",
            Verdict::Fail => "FAIL",
            Verdict::Skip => "skip",
        }
    }
}

struct Check {
    name: &'static str,
    verdict: Verdict,
    detail: String,
    /// Printed under the check when the verdict is not `ok`.
    hint: Option<String>,
}

/// `ferroma doctor`
///
/// Every check here exists because it has already gone wrong once: a database that
/// was never created, a port another process had claimed on loopback, a resolver that
/// blackholes names with no records, a TLS pair that does not match. `serve` survives
/// all four (it binds `0.0.0.0`, it accepts without a policy header, it starts without
/// TLS) — which is exactly why they need a command that says so out loud.
pub fn doctor(config: &Config) -> Result<ExitCode> {
    let mut checks = vec![
        check_data_dir(config),
        check_database(config),
        check_ports(config),
        check_tls(config),
    ];

    if config.api.jwt_secret.is_none() {
        checks.push(Check {
            name: "jwt-secret",
            verdict: Verdict::Warn,
            detail: "api.jwt_secret is unset".into(),
            hint: Some(
                "every session is invalidated when the process restarts. Set \
                 FERROMA_JWT_SECRET to a random 48-byte value."
                    .into(),
            ),
        });
    } else {
        checks.push(Check {
            name: "jwt-secret",
            verdict: Verdict::Ok,
            detail: "configured".into(),
            hint: None,
        });
    }

    for warning in crate::tls::insecure_warnings(&config.tls) {
        checks.push(Check {
            name: "tls-policy",
            verdict: Verdict::Warn,
            detail: warning,
            hint: None,
        });
    }

    // Print.
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    println!();
    for check in &checks {
        println!(
            "{:<width$}  {:<5} {}",
            check.name,
            check.verdict.label(),
            check.detail,
            width = width
        );
        if let Some(hint) = &check.hint {
            for line in hint.lines() {
                println!("{:<width$}        {line}", "", width = width);
            }
        }
    }

    let failed = checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    let warned = checks.iter().filter(|c| c.verdict == Verdict::Warn).count();
    println!();
    if failed > 0 {
        println!("{failed} check(s) would stop the server working, {warned} warning(s).");
        return Ok(ExitCode::FAILURE);
    }
    if warned > 0 {
        println!("no blocking problems, {warned} warning(s) worth reading.");
    } else {
        println!("everything checks out.");
    }
    Ok(ExitCode::SUCCESS)
}

fn check_data_dir(config: &Config) -> Check {
    let dir = config.data_dir();
    let probe = dir.join(".ferroma-doctor");

    let outcome = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&probe, b"ok"));
    let _ = std::fs::remove_file(&probe);

    match outcome {
        Ok(()) => Check {
            name: "data-dir",
            verdict: Verdict::Ok,
            detail: format!("{} is writable", dir.display()),
            hint: None,
        },
        Err(err) => Check {
            name: "data-dir",
            verdict: Verdict::Fail,
            detail: format!("{} is not writable: {err}", dir.display()),
            hint: Some(
                "the Maildir, attachments and TLS material all live here. In Docker this \
                 is the mounted volume, which has to be writable by uid 10001."
                    .into(),
            ),
        },
    }
}

fn check_database(config: &Config) -> Check {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            return Check {
                name: "database",
                verdict: Verdict::Fail,
                detail: format!("cannot start a runtime: {err}"),
                hint: None,
            }
        }
    };

    let name = Database::database_name(&config.database.url).unwrap_or_else(|_| "?".into());

    runtime.block_on(async {
        // Create it if it is missing: `serve` does not, so a check that only reported
        // the absence would leave the operator to work out the fix.
        match Database::ensure_database_exists(&config.database.url).await {
            Ok(true) => {
                return Check {
                    name: "database",
                    verdict: Verdict::Warn,
                    detail: format!("database {name} did not exist; it has been created"),
                    hint: Some("the migrations have not been applied yet — run `ferroma migrate`".into()),
                }
            }
            Ok(false) => {}
            Err(err) => {
                return Check {
                    name: "database",
                    verdict: Verdict::Fail,
                    detail: format!("{err}"),
                    hint: Some(
                        "check database.url, that PostgreSQL is running, and that the \
                         credentials are right."
                            .into(),
                    ),
                }
            }
        }

        let db = match Database::connect(&config.database).await {
            Ok(db) => db,
            Err(err) => {
                return Check {
                    name: "database",
                    verdict: Verdict::Fail,
                    detail: format!("{err}"),
                    hint: None,
                }
            }
        };

        let result = match db.migration_status().await {
            Ok((total, applied)) if applied < total => Check {
                name: "database",
                verdict: Verdict::Fail,
                detail: format!("{applied} of {total} migrations applied"),
                hint: Some("run `ferroma migrate` (or `ferroma database init`)".into()),
            },
            Ok((total, _)) => Check {
                name: "database",
                verdict: Verdict::Ok,
                detail: format!("{name}, {total} migration(s) applied"),
                hint: None,
            },
            Err(err) => Check {
                name: "database",
                verdict: Verdict::Fail,
                detail: format!("cannot read the migration state: {err}"),
                hint: None,
            },
        };
        db.close().await;
        result
    })
}

/// Check every listener port, including the case that looks fine but is not.
///
/// Binding `0.0.0.0:P` succeeds even when another process holds `127.0.0.1:P`,
/// because they are different addresses. The server therefore starts, reports
/// success, and serves *the other process* to anything on loopback — a mail client on
/// the same host talking to `localhost:P` reaches a stranger. So after binding, this
/// connects back to loopback and confirms the connection actually lands on us.
fn check_ports(config: &Config) -> Check {
    let mut wanted: Vec<(&'static str, String, u16)> = Vec::new();
    if config.smtp.enabled {
        wanted.push(("smtp", config.smtp.host.clone(), config.smtp.port));
        wanted.push(("submission", config.smtp.host.clone(), config.smtp.submission_port));
        if config.smtp.smtps_port != 0 {
            wanted.push(("smtps", config.smtp.host.clone(), config.smtp.smtps_port));
        }
    }
    if config.imap.enabled {
        wanted.push(("imap", config.imap.host.clone(), config.imap.port));
        if config.imap.imaps_port != 0 {
            wanted.push(("imaps", config.imap.host.clone(), config.imap.imaps_port));
        }
    }
    if config.api.enabled {
        wanted.push(("http", config.api.host.clone(), config.api.port));
        if config.api.tls_port != 0 {
            wanted.push(("https", config.api.host.clone(), config.api.tls_port));
        }
    }

    let mut taken = Vec::new();
    let mut shadowed = Vec::new();

    for (label, host, port) in &wanted {
        match std::net::TcpListener::bind((host.as_str(), *port)) {
            Ok(listener) => {
                if let Some(other) = loopback_shadowed(&listener, *port) {
                    shadowed.push(format!("{label} {host}:{port} (loopback reaches {other})"));
                }
            }
            // The OS message is the same for every port and says nothing a reader
            // needs; the address is the information.
            Err(_) => taken.push(format!("{label} {host}:{port}")),
        }
    }

    if !taken.is_empty() {
        return Check {
            name: "ports",
            verdict: Verdict::Fail,
            detail: format!("in use: {}", taken.join(", ")),
            hint: Some(
                "stop the process holding them, or change smtp.port / imap.port / \
                 api.port. The server would exit on startup."
                    .into(),
            ),
        };
    }
    if !shadowed.is_empty() {
        return Check {
            name: "ports",
            verdict: Verdict::Warn,
            detail: format!("reachable on loopback by another process: {}", shadowed.join(", ")),
            hint: Some(
                "binding 0.0.0.0 succeeds while another process holds 127.0.0.1 on the \
                 same port, so a client on this host reaches that process instead. \
                 Change the port, or bind a specific address."
                    .into(),
            ),
        };
    }

    Check {
        name: "ports",
        verdict: Verdict::Ok,
        detail: format!(
            "{} listener(s) available: {}",
            wanted.len(),
            wanted
                .iter()
                .map(|(label, _, port)| format!("{label}:{port}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        hint: None,
    }
}

/// When the listener is on a wildcard address, connect to loopback and see whether
/// the accepted connection is ours.
///
/// Returns `Some(description)` when something *else* answered, `None` when the
/// listener is unshadowed or the test is inconclusive (a specific bind address, or a
/// connection that neither succeeded nor was accepted).
fn loopback_shadowed(listener: &std::net::TcpListener, port: u16) -> Option<String> {
    let local = listener.local_addr().ok()?;
    if !local.ip().is_unspecified() {
        // Bound to a specific address: nothing can shadow it.
        return None;
    }
    listener.set_nonblocking(true).ok()?;

    let peer = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().ok()?,
        std::time::Duration::from_millis(300),
    );

    match peer {
        Err(_) => {
            // Nothing on loopback at all, so nothing can be reaching a stranger.
            let _ = listener.set_nonblocking(false);
            None
        }
        Ok(_stream) => {
            // Something accepted. Was it us? Accept with a short deadline.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let ours = listener.accept().is_ok();
            let _ = listener.set_nonblocking(false);
            if ours {
                None
            } else {
                Some("an unidentified process".to_string())
            }
        }
    }
}

fn check_tls(config: &Config) -> Check {
    if !config.tls.enabled {
        return Check {
            name: "tls",
            verdict: Verdict::Skip,
            detail: "disabled: STARTTLS, SMTPS, IMAPS and HTTPS are unavailable".into(),
            hint: None,
        };
    }
    match crate::tls::build(config) {
        Ok(Some(material)) => {
            let source = match material.source() {
                crate::tls::CertificateSource::PemFile { cert, .. } => {
                    format!("{}", cert.display())
                }
                crate::tls::CertificateSource::SelfSignedGenerated => {
                    "self-signed, generated at boot".to_string()
                }
            };
            Check {
                name: "tls",
                verdict: if material.source().is_trusted() {
                    Verdict::Ok
                } else {
                    Verdict::Warn
                },
                detail: format!(
                    "{source} covers {:?}",
                    material.subject_alt_names()
                ),
                hint: if material.source().is_trusted() {
                    None
                } else {
                    Some(
                        "no client can verify this certificate, and remote servers may \
                         refuse to deliver. Configure tls.cert_path and tls.key_path."
                            .into(),
                    )
                },
            }
        }
        Ok(None) => Check {
            name: "tls",
            verdict: Verdict::Skip,
            detail: "disabled".into(),
            hint: None,
        },
        Err(err) => Check {
            name: "tls",
            verdict: Verdict::Fail,
            detail: format!("{err}"),
            hint: Some("the server would exit on startup".into()),
        },
    }
}

// -----------------------------------------------------------------------------
// healthcheck
// -----------------------------------------------------------------------------

/// `ferroma healthcheck`
///
/// Deliberately dependency-free: it speaks HTTP/1.1 over a `TcpStream` so the image
/// needs no `curl`, and it exits non-zero on anything that is not `200`, which is
/// what makes it usable as a Docker `HEALTHCHECK`.
pub fn healthcheck(args: &HealthcheckArgs) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let url = url::Url::parse(&args.url).with_context(|| format!("invalid URL {}", args.url))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("URL has no host: {}", args.url))?;
        let port = url.port_or_known_default().unwrap_or(80);
        let path = if url.path().is_empty() { "/" } else { url.path() };

        let deadline = std::time::Duration::from_secs(args.timeout.max(1));
        let attempt = async {
            let mut stream = tokio::net::TcpStream::connect((host, port))
                .await
                .with_context(|| format!("connecting to {host}:{port}"))?;
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: ferroma-healthcheck\r\nAccept: application/json\r\n\r\n"
            );
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream.write_all(request.as_bytes()).await?;
            stream.flush().await?;

            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            anyhow::Ok(response)
        };

        let response = tokio::time::timeout(deadline, attempt)
            .await
            .map_err(|_| anyhow!("{} did not answer within {}s", args.url, args.timeout))??;

        let text = String::from_utf8_lossy(&response);
        let status_line = text.lines().next().unwrap_or_default().to_string();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("malformed HTTP response: {status_line:?}"))?;

        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.trim())
            .unwrap_or_default();

        if args.show_body {
            println!("{status_line}");
            if !body.is_empty() {
                println!("{body}");
            }
        }

        if status == 200 {
            // `degraded` is reported with 503, so a 200 already means healthy; this
            // guards against a server that reports the string without the code.
            if body.contains("\"status\":\"degraded\"") || body.contains("\"status\": \"degraded\"") {
                eprintln!("ferroma reported itself degraded");
                return Ok(ExitCode::FAILURE);
            }
            println!("ok");
            Ok(ExitCode::SUCCESS)
        } else {
            eprintln!("health endpoint returned {status}");
            if !body.is_empty() {
                eprintln!("{body}");
            }
            Ok(ExitCode::FAILURE)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_labels_are_distinct_and_short() {
        // The column has to line up in a terminal; a longer label would push the
        // detail out of alignment.
        for (verdict, expected) in [
            (Verdict::Ok, "ok"),
            (Verdict::Warn, "warn"),
            (Verdict::Fail, "FAIL"),
            (Verdict::Skip, "skip"),
        ] {
            assert_eq!(verdict.label(), expected);
            assert!(expected.len() <= 5);
        }
    }

    #[test]
    fn a_specific_bind_address_cannot_be_shadowed() {
        // Bound to 127.0.0.1, so no other process can be receiving loopback traffic
        // on this port: the check has nothing to report.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        assert!(loopback_shadowed(&listener, port).is_none());
    }

    #[test]
    fn a_wildcard_listener_that_answers_loopback_is_not_shadowed() {
        // The ordinary case: we bound 0.0.0.0, and the connection we make to
        // loopback really is accepted by us. Reporting this as shadowed would make
        // `doctor` cry wolf on every healthy server.
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bind wildcard");
        let port = listener.local_addr().unwrap().port();
        assert!(
            loopback_shadowed(&listener, port).is_none(),
            "our own listener must not be mistaken for a stranger"
        );
    }

    #[test]
    fn a_wildcard_port_held_on_loopback_by_someone_else_is_reported() {
        // The bug this check exists for. A process holding 127.0.0.1:P shadows our
        // 0.0.0.0:P for anything on loopback, and binding still succeeds — so the only
        // way to notice is to connect back and see who answers.
        //
        // Whether that pair is even reachable is a platform property, and the two
        // families disagree. Windows allows the overlapping bind, which is where the
        // detector earns its keep. Linux refuses it with `EADDRINUSE`: `SO_REUSEADDR`
        // does not admit a live wildcard/specific pair, so a port whose wildcard bind
        // the kernel rejects is one where the shadowing has already been prevented.
        // The test asserts whichever of the two this host actually does, instead of
        // demanding the Windows outcome everywhere.
        for _ in 0..8 {
            let squatter = match std::net::TcpListener::bind("127.0.0.1:0") {
                Ok(listener) => listener,
                Err(_) => continue,
            };
            let port = squatter.local_addr().expect("squatter address").port();

            let ours = match std::net::TcpListener::bind(("0.0.0.0", port)) {
                Ok(listener) => listener,
                Err(err) => {
                    // Not a wildcard listener squatting on the port — an unrelated
                    // process holding 0.0.0.0:P would have refused the squatter too.
                    assert_eq!(
                        err.kind(),
                        std::io::ErrorKind::AddrInUse,
                        "the wildcard bind failed for an unexpected reason: {err}"
                    );
                    continue;
                }
            };

            // Keep the squatter accepting, so a connection to loopback succeeds.
            let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = accepted.clone();
            std::thread::spawn(move || {
                for stream in squatter.incoming() {
                    if stream.is_err() {
                        break;
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });

            let reported = loopback_shadowed(&ours, port);
            assert!(
                reported.is_some(),
                "a loopback squatter must be reported, not silently accepted"
            );
            return;
        }

        // Every port this host handed us also refused the wildcard bind, so the
        // overlapping pair cannot exist here and nothing can shadow loopback.
        // `a_specific_bind_address_cannot_be_shadowed` covers the other half of the
        // contract on this platform.
    }

    #[test]
    fn a_free_port_is_reported_as_available() {
        // No listener at all: the check must not claim a conflict.
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(loopback_shadowed(&std::net::TcpListener::bind("0.0.0.0:0").unwrap(), port).is_none());
    }
}
