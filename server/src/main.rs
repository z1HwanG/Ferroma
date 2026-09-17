//! The `ferroma` binary.
//!
//! One executable does everything: run the platform, validate the configuration,
//! migrate a database, administer accounts and domains, generate DKIM keys, check
//! the mail store, and answer the container healthcheck. Nothing here is a thin
//! wrapper around something else — the operator commands talk to the same
//! repositories the server does, so `ferroma user create` and the Admin panel
//! cannot disagree about what creating an account means.
//!
//! The command surface is defined in [`cli`]; the implementations are in
//! [`commands`].

mod cli;
mod commands;
mod serve;
mod tls;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;
use ferroma_core::config::{Config, LogFormat};

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            // Commands parse arguments before they have logging set up, so the
            // failure has to be printable on its own.
            eprintln!("ferroma: {err}");
            let mut source = std::error::Error::source(&*err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    // `config default` and `version` must work even when the configuration on disk is
    // broken — they are what an operator reaches for when it is.
    match &cli.command {
        cli::Command::Version => {
            println!("{}", ferroma_core::version::build_info());
            return Ok(ExitCode::SUCCESS);
        }
        cli::Command::Config(cli::ConfigCommand::Default) => {
            print!("{}", Config::DEFAULT_TOML);
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let config = Config::load(cli.config.as_deref())?;

    let log_level = cli.log_level(&config.server.log_level);
    let format = if cli.log_json {
        LogFormat::Json
    } else {
        config.server.log_format
    };
    ferroma_core::logging::init(&log_level, format)?;

    tracing::debug!(config = ?cli.config, "configuration loaded");

    match &cli.command {
        cli::Command::Version | cli::Command::Config(cli::ConfigCommand::Default) => {
            unreachable!("handled before the configuration was loaded")
        }
        cli::Command::Config(cli::ConfigCommand::Check { dns_domain }) => {
            commands::config_check(&config, dns_domain.as_deref())
        }
        cli::Command::Config(cli::ConfigCommand::Show) => commands::config_show(&config),
        cli::Command::Serve(args) => serve::run(&config, args),
        cli::Command::Migrate => commands::migrate(&config).map(|()| ExitCode::SUCCESS),
        cli::Command::Database(command) => commands::database(&config, command),
        cli::Command::User(command) => commands::user(&config, command),
        cli::Command::Domain(command) => commands::domain(&config, command),
        cli::Command::Dkim(command) => commands::dkim(&config, command),
        cli::Command::Storage(command) => commands::storage(&config, command),
        cli::Command::Sync(command) => commands::sync(&config, command),
        cli::Command::Healthcheck(args) => commands::healthcheck(args),
        cli::Command::Doctor => commands::doctor(&config),
    }
}
