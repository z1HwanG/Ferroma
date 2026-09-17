//! The `ferroma` command-line surface.
//!
//! One binary does everything: run the server, inspect the configuration, migrate a
//! database, create the first account, generate DKIM keys, and check that a running
//! instance is healthy. That matters operationally — a container image needs no
//! second tool to be useful, and the Docker healthcheck is `ferroma healthcheck`.
//!
//! ```text
//! ferroma serve                     # run the platform
//! ferroma config check              # validate configuration and exit
//! ferroma config show               # print the effective configuration
//! ferroma migrate                   # apply database migrations
//! ferroma user create …             # administration
//! ferroma domain create …
//! ferroma dkim generate --domain example.com
//! ferroma storage stats|gc|verify
//! ferroma sync prune
//! ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health
//! ferroma version
//! ```

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Ferroma — a Rust-native self-hosted mail platform.
#[derive(Debug, Parser)]
#[command(
    name = "ferroma",
    version,
    about = "Ferroma — a Rust-native self-hosted mail platform",
    long_about = None,
    propagate_version = true,
    disable_help_subcommand = false,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Path to `ferroma.toml`. Defaults to `config/ferroma.toml`, then `ferroma.toml`,
    /// then the configuration compiled into the binary.
    #[arg(long, short = 'c', global = true, env = "FERROMA_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Increase log verbosity. Repeatable: `-v` is `debug`, `-vv` is `trace`.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Decrease log verbosity: `-q` is `warn`, `-qq` is `error`.
    #[arg(long, short = 'q', global = true, action = clap::ArgAction::Count)]
    pub quiet: u8,

    /// Emit logs as newline-delimited JSON.
    #[arg(long, global = true)]
    pub log_json: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// The log filter this invocation should use.
    ///
    /// Explicit flags win over the configured level; `RUST_LOG` is honoured by
    /// `ferroma_core::logging` when neither is given.
    pub fn log_level(&self, configured: &str) -> String {
        match (self.verbose, self.quiet) {
            (0, 0) => configured.to_string(),
            (v, 0) => match v {
                1 => "debug".to_string(),
                _ => "trace".to_string(),
            },
            (0, q) => match q {
                1 => "warn".to_string(),
                _ => "error".to_string(),
            },
            // Both given: the louder request wins, which is the least surprising.
            (v, _) => {
                if v >= 2 {
                    "trace".to_string()
                } else {
                    "debug".to_string()
                }
            }
        }
    }
}

/// Everything the binary can do.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the platform: SMTP, IMAP, the HTTP API, the queue workers and the sync service.
    Serve(ServeArgs),

    /// Configuration utilities.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Apply the embedded database migrations and exit.
    ///
    /// Creates the database first when it does not exist, so a bare PostgreSQL needs
    /// no `psql` step before the first run.
    Migrate,

    /// Database lifecycle utilities.
    #[command(subcommand)]
    Database(DatabaseCommand),

    /// Account administration.
    #[command(subcommand)]
    User(UserCommand),

    /// Domain administration.
    #[command(subcommand)]
    Domain(DomainCommand),

    /// DKIM key management.
    #[command(subcommand)]
    Dkim(DkimCommand),

    /// Mail store maintenance.
    #[command(subcommand)]
    Storage(StorageCommand),

    /// Synchronisation maintenance.
    #[command(subcommand)]
    Sync(SyncCommand),

    /// Check that a running Ferroma answers its health endpoint.
    ///
    /// This is the container healthcheck; it exits non-zero when the API is down or
    /// reports itself degraded.
    Healthcheck(HealthcheckArgs),

    /// Preflight everything `serve` needs, and report what would fail.
    ///
    /// Checks the configuration, the data directory, the database and its migrations,
    /// DNS, every listener port, and the TLS material. Run it before `serve` — or
    /// after a deployment that "started fine" but does not work.
    Doctor,

    /// Print build and protocol information.
    Version,
}

/// `ferroma serve`
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Bind only this subsystem. Repeatable. Default: everything enabled in the config.
    ///
    /// Useful for running the SMTP listener in one container and the API in another.
    #[arg(long, value_name = "NAME", value_parser = ["smtp", "imap", "api", "queue"])]
    pub only: Vec<String>,

    /// Apply migrations before starting. The server does this anyway unless
    /// `database.run_migrations` is false; the flag forces it.
    #[arg(long)]
    pub migrate: bool,

    /// Validate the configuration, print a summary, and exit without binding anything.
    #[arg(long)]
    pub check: bool,
}

/// `ferroma config …`
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse the configuration, validate it, and report every problem found.
    ///
    /// Also probes the resolver, because a DNS lookup is on the critical path of
    /// every inbound message and a resolver that does not answer shows up as
    /// mysteriously slow mail rather than as an error.
    Check {
        /// Probe this name instead of `gmail.com`.
        ///
        /// Use it to measure the lookups inbound authentication will actually make —
        /// for example the domain you host, whose records decide whether your own
        /// users' mail passes SPF.
        #[arg(long, value_name = "DOMAIN")]
        dns_domain: Option<String>,
    },
    /// Print the effective configuration (defaults, file and environment merged).
    Show,
    /// Print the embedded default configuration.
    Default,
}

/// `ferroma database …`
#[derive(Debug, Subcommand)]
pub enum DatabaseCommand {
    /// Create the database named in `database.url` if it does not exist, then migrate.
    ///
    /// This is the first-run command: against a bare PostgreSQL,
    /// `ferroma database init` is all that stands between you and `ferroma serve`.
    Init,
    /// Report the server version, database size, migration state and pool settings.
    Status,
}

/// `ferroma user …`
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create an account, with its primary address and its standard folders.
    Create(UserCreateArgs),
    /// List accounts.
    List(UserListArgs),
    /// Change a password.
    Password(UserPasswordArgs),
    /// Enable or disable an account without deleting anything.
    SetEnabled {
        /// The account's email address.
        email: String,
        /// `true` to enable, `false` to disable.
        #[arg(value_parser = clap::value_parser!(bool), action = clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Grant or remove administrator rights.
    SetAdmin {
        /// The account's email address.
        email: String,
        #[arg(value_parser = clap::value_parser!(bool), action = clap::ArgAction::Set)]
        admin: bool,
    },
    /// Delete an account and everything it owns.
    Delete {
        /// The account's email address.
        email: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// `ferroma user create …`
#[derive(Debug, Args)]
pub struct UserCreateArgs {
    /// The login address, e.g. `alice@example.com`.
    pub email: String,
    /// The password. When omitted it is read from stdin, which keeps it out of the
    /// shell history and out of `ps`.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,
    /// Human-readable name.
    #[arg(long)]
    pub display_name: Option<String>,
    /// Grant administrator rights.
    #[arg(long)]
    pub admin: bool,
    /// Mailbox quota in bytes. Defaults to `limits.mailbox_quota`.
    #[arg(long)]
    pub quota: Option<i64>,
    /// Create the domain automatically when it does not exist yet.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub create_domain: bool,
}

/// `ferroma user list`
#[derive(Debug, Args)]
pub struct UserListArgs {
    /// Maximum rows to print.
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
    /// Rows to skip.
    #[arg(long, default_value_t = 0)]
    pub offset: i64,
    /// Print one address per line, for scripting.
    ///
    /// Deliberately *not* `--quiet`: `-q/--quiet` is a global verbosity flag, and a
    /// subcommand that redefines it makes clap panic at argument-parse time with
    /// "Mismatch between definition and access of `quiet`".
    #[arg(long = "emails-only")]
    pub emails_only: bool,
}

/// `ferroma user password …`
#[derive(Debug, Args)]
pub struct UserPasswordArgs {
    /// The account's email address.
    pub email: String,
    /// The new password. When omitted it is read from stdin.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,
    /// Do not revoke the account's other sessions.
    #[arg(long)]
    pub keep_sessions: bool,
}

/// `ferroma domain …`
#[derive(Debug, Subcommand)]
pub enum DomainCommand {
    /// Add a domain this server accepts mail for.
    Create {
        /// The domain name, e.g. `example.com`.
        name: String,
        /// Optional description shown in the Admin panel.
        #[arg(long)]
        description: Option<String>,
    },
    /// List domains.
    List,
    /// Enable or disable a domain.
    SetEnabled {
        name: String,
        #[arg(value_parser = clap::value_parser!(bool), action = clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Remove a domain. Refuses while it still has addresses unless `--force`.
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Create a forwarding alias, e.g. `sales@example.com` -> `alice@example.com`.
    AddAlias {
        /// The alias address.
        address: String,
        /// Where it forwards to. A bare local part means "same domain".
        target: String,
    },
    /// List a domain's aliases.
    ListAliases { name: String },
}

/// `ferroma dkim …`
#[derive(Debug, Subcommand)]
pub enum DkimCommand {
    /// Generate a key pair for a domain and store it with the domain record.
    Generate {
        /// The domain to sign for.
        ///
        /// A long flag rather than a positional so the documented invocation —
        /// `ferroma dkim generate --domain example.com`, which `.env.example` and
        /// `docs/deployment.md` both show — is the one that works.
        #[arg(long = "domain", short = 'd')]
        domain: String,
        /// The DKIM selector. Defaults to `dkim.selector`.
        #[arg(long)]
        selector: Option<String>,
        /// Also write the private key to this path, for the `[dkim] private_key_path`
        /// setting. Without it the key lives in the database.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Overwrite an existing key.
        #[arg(long)]
        force: bool,
    },
    /// Print the DNS TXT record to publish.
    Show {
        /// The domain.
        #[arg(long = "domain", short = 'd')]
        domain: String,
        /// The selector. Defaults to the domain's stored selector.
        #[arg(long)]
        selector: Option<String>,
    },
}

/// `ferroma storage …`
#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Report how much is stored where.
    Stats,
    /// Check that every message row has its file, and report orphans.
    Verify {
        /// List the individual problems, not just the counts.
        ///
        /// Named `--details` rather than `--verbose`: `-v/--verbose` is a global
        /// verbosity flag, and a subcommand that redefines it makes clap panic when
        /// the arguments are downcast.
        #[arg(long)]
        details: bool,
    },
    /// Delete unreferenced attachment blobs and abandoned `tmp/` files.
    Gc {
        /// Report what would be removed without removing it.
        #[arg(long)]
        dry_run: bool,
    },
}

/// `ferroma sync …`
#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Delete change-log entries and applied operations older than the retention window.
    Prune,
}

/// `ferroma healthcheck …`
#[derive(Debug, Args)]
pub struct HealthcheckArgs {
    /// The health endpoint. Defaults to the API's own loopback address.
    #[arg(long, default_value = "http://127.0.0.1:8080/api/v1/health")]
    pub url: String,
    /// How long to wait for an answer, in seconds.
    #[arg(long, default_value_t = 5)]
    pub timeout: u64,
    /// Print the response body on success too.
    ///
    /// `--show-body`, not `--verbose`: the latter is a global flag and redefining it
    /// in a subcommand makes clap panic during argument downcast.
    #[arg(long = "show-body")]
    pub show_body: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_valid() {
        // clap catches duplicate flags, bad defaults and unregistered subcommands here.
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_serve_with_subsystem_selection() {
        let cli = Cli::try_parse_from(["ferroma", "serve", "--only", "smtp", "--only", "api"]).unwrap();
        match cli.command {
            Command::Serve(args) => {
                assert_eq!(args.only, vec!["smtp", "api"]);
                assert!(!args.check);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_unknown_subsystem() {
        assert!(Cli::try_parse_from(["ferroma", "serve", "--only", "sieve"]).is_err());
    }

    #[test]
    fn parses_config_path_and_verbosity() {
        let cli = Cli::try_parse_from(["ferroma", "-c", "/etc/ferroma.toml", "-vv", "config", "check"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/etc/ferroma.toml")));
        assert_eq!(cli.log_level("info"), "trace");
        match cli.command {
            Command::Config(ConfigCommand::Check { dns_domain }) => assert!(dns_domain.is_none()),
            other => panic!("expected Config::Check, got {other:?}"),
        }

        // The diagnostic that distinguishes "the resolver is fine" from "this
        // particular domain has no records", which is otherwise a silent 60-second
        // stall on every inbound message.
        let cli = Cli::try_parse_from([
            "ferroma",
            "config",
            "check",
            "--dns-domain",
            "example.test",
        ])
        .unwrap();
        match cli.command {
            Command::Config(ConfigCommand::Check { dns_domain }) => {
                assert_eq!(dns_domain.as_deref(), Some("example.test"));
            }
            other => panic!("expected Config::Check, got {other:?}"),
        }
    }

    #[test]
    fn log_level_precedence() {
        let base = Cli::try_parse_from(["ferroma", "version"]).unwrap();
        assert_eq!(base.log_level("warn"), "warn");

        let v1 = Cli::try_parse_from(["ferroma", "-v", "version"]).unwrap();
        assert_eq!(v1.log_level("warn"), "debug");

        let v2 = Cli::try_parse_from(["ferroma", "-vv", "version"]).unwrap();
        assert_eq!(v2.log_level("warn"), "trace");

        let q1 = Cli::try_parse_from(["ferroma", "-q", "version"]).unwrap();
        assert_eq!(q1.log_level("info"), "warn");

        let q2 = Cli::try_parse_from(["ferroma", "-qq", "version"]).unwrap();
        assert_eq!(q2.log_level("info"), "error");

        // Asking for both is contradictory; the louder request wins.
        let both = Cli::try_parse_from(["ferroma", "-v", "-q", "version"]).unwrap();
        assert_eq!(both.log_level("info"), "debug");
    }

    #[test]
    fn user_create_defaults_to_creating_the_domain() {
        let cli = Cli::try_parse_from(["ferroma", "user", "create", "alice@example.com"]).unwrap();
        match cli.command {
            Command::User(UserCommand::Create(args)) => {
                assert_eq!(args.email, "alice@example.com");
                assert!(args.create_domain);
                assert!(args.password.is_none(), "password must be read from stdin when omitted");
                assert!(!args.admin);
            }
            other => panic!("expected User::Create, got {other:?}"),
        }
    }

    #[test]
    fn no_subcommand_shadows_a_global_flag() {
        // `Cli` defines -c/--config, -v/--verbose, -q/--quiet and --log-json globally.
        // A subcommand that redefines any of those names compiles fine and then makes
        // clap panic *at argument-parse time* with "Mismatch between definition and
        // access of `<name>`" — which is exactly how `user list --quiet` shipped once.
        // Parsing one representative command line per subcommand is what catches it.
        for argv in [
            vec!["ferroma", "serve"],
            vec!["ferroma", "config", "check"],
            vec!["ferroma", "config", "check", "--dns-domain", "example.test"],
            vec!["ferroma", "config", "show"],
            vec!["ferroma", "config", "default"],
            vec!["ferroma", "migrate"],
            vec!["ferroma", "database", "init"],
            vec!["ferroma", "database", "status"],
            vec!["ferroma", "user", "create", "a@b.example"],
            vec!["ferroma", "user", "list"],
            vec!["ferroma", "user", "list", "--emails-only"],
            vec!["ferroma", "user", "password", "a@b.example"],
            vec!["ferroma", "user", "set-enabled", "a@b.example", "false"],
            vec!["ferroma", "user", "set-admin", "a@b.example", "true"],
            vec!["ferroma", "user", "delete", "a@b.example", "--yes"],
            vec!["ferroma", "domain", "create", "example.com"],
            vec!["ferroma", "domain", "list"],
            vec!["ferroma", "domain", "set-enabled", "example.com", "true"],
            vec!["ferroma", "domain", "delete", "example.com", "--force"],
            vec!["ferroma", "domain", "add-alias", "a@b.example", "c@d.example"],
            vec!["ferroma", "domain", "list-aliases", "example.com"],
            vec!["ferroma", "dkim", "generate", "--domain", "example.com"],
            vec!["ferroma", "dkim", "show", "--domain", "example.com"],
            vec!["ferroma", "storage", "stats"],
            vec!["ferroma", "storage", "verify", "--details"],
            vec!["ferroma", "storage", "gc", "--dry-run"],
            vec!["ferroma", "sync", "prune"],
            vec!["ferroma", "healthcheck"],
            vec!["ferroma", "version"],
        ] {
            // `try_parse_from` returning Ok is not enough: the downcast happens when
            // the typed struct is built, which is what panicked before.
            let cli = Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} failed to parse: {e}"));
            // Touch the parsed value so the `Args` downcast actually runs.
            let _ = format!("{:?}", cli.command);
        }
    }

    #[test]
    fn requires_a_subcommand() {
        assert!(Cli::try_parse_from(["ferroma"]).is_err());
    }

    #[test]
    fn healthcheck_defaults_to_loopback() {
        let cli = Cli::try_parse_from(["ferroma", "healthcheck"]).unwrap();
        match cli.command {
            Command::Healthcheck(args) => {
                assert_eq!(args.url, "http://127.0.0.1:8080/api/v1/health");
                assert_eq!(args.timeout, 5);
            }
            other => panic!("expected Healthcheck, got {other:?}"),
        }
    }

    #[test]
    fn parses_the_administration_commands() {
        assert!(matches!(
            Cli::try_parse_from(["ferroma", "domain", "add-alias", "sales@example.com", "alice"]).unwrap().command,
            Command::Domain(DomainCommand::AddAlias { .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferroma", "dkim", "generate", "--domain", "example.com"]).unwrap().command,
            Command::Dkim(DkimCommand::Generate { .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferroma", "storage", "gc", "--dry-run"]).unwrap().command,
            Command::Storage(StorageCommand::Gc { dry_run: true })
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferroma", "user", "set-enabled", "alice@example.com", "false"])
                .unwrap()
                .command,
            Command::User(UserCommand::SetEnabled { enabled: false, .. })
        ));
    }
}
