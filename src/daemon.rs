use anyhow::{bail, Context, Result};
use futures::StreamExt;
use tokio::{
    fs,
    net::{UnixListener, UnixStream},
    sync::mpsc,
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{error, info, trace};

use crate::{
    server::{ClientId, GlobalState, NewConnection, Task},
    types::BootstrapMessage,
};

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();

    info!("Running the daemon");

    // Create the global state. This loads the default configuration.
    let mut state = GlobalState::default();

    let parent = state.config.socket_path.parent().unwrap();
    let _ = fs::create_dir_all(parent).await;

    // Test to see if another instance of the daemon is running by making a
    // connection.
    {
        let socket = UnixStream::connect(&state.config.socket_path).await;
        if socket.is_ok() {
            bail!("Daemon already running.");
        }
    }

    // Make sure the file is removed.
    let _ = fs::remove_file(&state.config.socket_path).await;

    let listener = UnixListener::bind(&state.config.socket_path)
        .context("Failed to listen to the unix socket.")?;
    info!("Listening on {:?}", state.config.socket_path);

    let (tx, mut rx) = mpsc::channel::<Task>(2048);
    state.set_sender(tx.clone());

    // This task is responsible for listening for new connections.
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let id = ClientId::new();
                if let Err(e) = handle_client(id, tx.clone(), stream).await {
                    error!("Client dropped with error: {e}");
                }
                let _ = tx.send(Task::ClientDropped(id)).await;
            });
        }
    });

    while let Some(task) = rx.recv().await {
        if let Err(e) = state.handle_task(task).await {
            error!("Error handling task: {e}")
        }
    }

    Ok(())
}

async fn handle_client(id: ClientId, tx: mpsc::Sender<Task>, stream: UnixStream) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = FramedRead::new(reader, LengthDelimitedCodec::new());
    let writer = FramedWrite::new(writer, LengthDelimitedCodec::new());

    let bytes = reader
        .next()
        .await
        .context("Could not get the initial message.")??;
    let bootstrap: BootstrapMessage =
        serde_json::from_slice(&bytes).context("Invalid bootstrap message.")?;

    // Should be an 'initialize' request.
    let bytes = reader
        .next()
        .await
        .context("Could not get the first request.")??;
    let request: lsp_server::Request =
        serde_json::from_slice(&bytes).context("Invalid LSP request.")?;

    tx.send(Task::NewConnection(NewConnection {
        id,
        writer,
        bootstrap,
        request,
    }))
    .await
    .context("Failed to send task.")?;

    while let Some(next) = reader.next().await {
        let bytes = next.context("Failed to read length delimited message.")?;
        let message: lsp_server::Message =
            serde_json::from_slice(&bytes).context("Invalid LSP message.")?;

        if matches!(&message, lsp_server::Message::Request(req) if req.method == "shutdown") {
            break;
        }

        tx.send(Task::ClientMessage(id, message))
            .await
            .context("Failed to send task")?;
    }

    Ok(())
}
