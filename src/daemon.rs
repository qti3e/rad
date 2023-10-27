use anyhow::{bail, Context, Result};
use tokio::{
    fs,
    net::{UnixListener, UnixStream},
};
use tracing::info;

use crate::types::Config;

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();

    info!("Running the daemon");
    let config = Config::default();

    let parent = config.socket_path.parent().unwrap();
    let _ = fs::create_dir_all(parent).await;

    // Test to see if another instance of the daemon is running by making a
    // connection.
    {
        let socket = UnixStream::connect(&config.socket_path).await;
        if socket.is_ok() {
            bail!("Daemon already running.");
        }
    }

    // Make sure the file is removed.
    let _ = fs::remove_file(&config.socket_path).await;

    let listener =
        UnixListener::bind(&config.socket_path).context("Failed to listen to the unix socket.")?;
    info!("Listening on {:?}", config.socket_path);

    while let Ok((_stream, addr)) = listener.accept().await {
        info!("Accepted client {addr:?}");
        let _bin = config.rust_analyzer_bin.clone();
        tokio::spawn(async move {
            // match handle_connection(session_map, bin, stream).await {
            //     Ok(_) => info!("Client detached"),
            //     Err(e) => error!("Connection failed: {e:?}"),
            // }
        });
    }

    Ok(())
}
