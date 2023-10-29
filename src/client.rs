use crate::{
    types::{BootstrapMessage, Config},
    utils::{bind_transport, read_lsp_message},
};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{self, BufReader},
    net::UnixStream,
};

/// Run the daemon client.
pub async fn run() -> Result<()> {
    let config = Config::default();

    let socket = UnixStream::connect(config.socket_path)
        .await
        .context("Failed to connect to the daemon. Is it running?")?;

    let (mut socket_writer, mut socket_reader) = bind_transport(socket).split();

    // Send the bootstrap message.
    let message = serde_json::to_vec(&BootstrapMessage::default())
        .context("Failed to serialize bootstrap message.")?;
    socket_writer
        .send(message.into())
        .await
        .context("Failed to send the bootstrap message.")?;

    // Spawn the writer task. Reads from the stdio and writes to the Unix socket.
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(io::stdin());
        while let Some(message) = read_lsp_message(&mut reader).await? {
            let is_exit =
                matches!(&message, lsp_server::Message::Notification(n) if n.method == "exit");

            let bytes = serde_json::to_vec(&message).unwrap().into();
            socket_writer
                .send(bytes)
                .await
                .context("Failed to write.")?;

            if is_exit {
                std::process::exit(0);
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    // We use the blocking API here.
    let mut stdout = std::io::stdout().lock();
    while let Some(Ok(message)) = socket_reader.next().await {
        let message: lsp_server::Message =
            serde_json::from_slice(&message).expect("Invalid message");
        message.write(&mut stdout)?;
    }

    task.await?
}
