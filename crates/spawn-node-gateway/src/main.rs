//! `spawn-node-gateway`: configured by its environment (see
//! `spawn_node_gateway::config`), serves until killed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use spawn_node_gateway::config::ConfigError;
use spawn_node_gateway::{Config, Gateway, Settings, primary, spawn, system_clock};
use spawn_postgres::{ConnectOptions, PostgresStore};

/// A ledger statement (and a connect) gives up after this.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spawn-node-gateway: {}", error.message);
            ExitCode::from(error.exit_code)
        }
    }
}

fn run() -> Result<(), ConfigError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "usage: spawn-node-gateway [--public-prefix /path]\n\
             environment: SPAWN_NODE_GATEWAY_DATABASE_URL (or $CREDENTIALS_DIRECTORY/spawn-node-gateway.database-url), \
             SPAWN_NODE_GATEWAY_KEY_FILE, SPAWN_NODE_ARTIFACTS, SPAWN_NODE_RESULTS (required); \
             SPAWN_NODE_GATEWAY_LISTEN (127.0.0.1:18096), SPAWN_NODE_GATEWAY_CELLS (c0), SPAWN_NODE_GATEWAY_PUBLIC_PREFIX, SPAWN_NODE_GATEWAY_WORKERS (6)"
        );
        return Ok(());
    }
    let config = Config::from_env_with_credentials(|name| std::env::var(name).ok(), &args)?;
    let fail = |message: String| ConfigError {
        message,
        exit_code: 1,
    };
    let key = primary::load_or_create(&config.key_file).map_err(fail)?;
    std::fs::create_dir_all(&config.results)
        .map_err(|error| fail(format!("{}: {error}", config.results.display())))?;
    if !config.artifacts.is_dir() {
        eprintln!(
            "spawn-node-gateway: warning: {} is not a directory yet",
            config.artifacts.display()
        );
    }
    let url = config.database_url.clone();
    let passfile = config.passfile.clone();
    let gateway = Arc::new(Gateway::new(Settings {
        primary: key,
        cells: config.cells.clone(),
        public_prefix: config.public_prefix.clone(),
        artifacts: config.artifacts.clone(),
        results: config.results.clone(),
        clock: system_clock(),
        connector: Arc::new(move || {
            PostgresStore::connect_with_options(
                &url,
                &ConnectOptions {
                    passfile: passfile.clone(),
                    statement_timeout: Some(STATEMENT_TIMEOUT),
                },
            )
            .map_err(|error| error.to_string())
        }),
    }));
    let listener = TcpListener::bind(config.listen)
        .map_err(|error| fail(format!("listen on {}: {error}", config.listen)))?;
    let addr =
        spawn(listener, &gateway, config.workers).map_err(|error| fail(error.to_string()))?;
    let primary = gateway.primary_key();
    eprintln!(
        "spawn-node-gateway: listening on {addr} (cells {}, workers {}), primary key {} ({})",
        config.cells.join(","),
        config.workers,
        primary.key_id(),
        primary.to_b64u()
    );
    loop {
        std::thread::park();
    }
}
