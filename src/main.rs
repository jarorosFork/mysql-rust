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
    // In the future: load from CLI args / env / a config file.
    let config = Config::default();

    if let Err(err) = run(config).await {
        eprintln!("fatal: {err}");
        process::exit(1);
    }
}

async fn run(config: Config) -> mysql_rust::Result<()> {
    let server = Server::new(config);
    server.run().await
}
