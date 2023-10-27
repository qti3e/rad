use anyhow::Result;
use std::env;

#[tokio::main]
async fn main() {
    // Only start the sub command if we are a daemon.
    let exit: Result<()> = if env::args().nth(1) == Some("daemon".to_string()) {
        rad::daemon::run().await
    } else {
        rad::client::run().await
    };

    match exit {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::exit(-1);
        }
    }
}
