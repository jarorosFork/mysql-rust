//! Binary entry point for the `mysql-rust` server.
//!
//! Keep this thin: parse config, hand off to the library, and turn any
//! error into a non-zero exit code. All real logic lives in the library
//! crate (`src/lib.rs` and its modules) so it can be unit-tested.

use std::process;

use mysql_rust::config::Config;
use mysql_rust::server::Server;

#[tokio::main]
async fn main() {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("fatal: {err}");
            process::exit(1);
        }
    };

    // Without at least one account the server denies every login, which is a
    // confusing first-run experience — point the way to the env vars.
    if config.users.is_empty() {
        eprintln!(
            "warning: no accounts configured, so every login will be denied.\n\
             Set MYSQLRUST_USER (and MYSQLRUST_PASSWORD) to create one, e.g.:\n  \
             MYSQLRUST_USER=alice MYSQLRUST_PASSWORD=s3cret cargo run"
        );
    }

    if let Err(err) = run(config).await {
        eprintln!("fatal: {err}");
        process::exit(1);
    }
}

async fn run(config: Config) -> mysql_rust::Result<()> {
    let server = Server::new(config);
    server.run().await
}
