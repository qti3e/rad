//! Even though the main purpose of this tool is to keep rust-analyzer in the background, it
//! can be used as a multiplexer. being a 'good' multiplexer is the secondary objective of
//! this project.

use anyhow::{Context, Result};
use bytes::BytesMut;
use futures::StreamExt;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicU32, Ordering},
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{error, info};

use tokio::{
    io::BufReader,
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};

use crate::{
    types::{BootstrapMessage, Config, SessionKey},
    utils::read_lsp_message,
};

/// The writer we use to send stuff to the client.
pub type ClientFramedWriter = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

pub struct NewConnection {
    pub id: ClientId,
    pub writer: ClientFramedWriter,
    pub bootstrap: BootstrapMessage,
    pub request: lsp_server::Request,
}

pub enum Task {
    NewConnection(NewConnection),
    ClientDropped(ClientId),
    ServerShutdown(ServerId),
    ClientMessage(ClientId, lsp_server::Message),
    ServerMessage(ServerId, lsp_server::Message),
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq)]
pub struct ClientId(u32);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq)]
pub struct ServerId(u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ServerDescriptor {
    bin: String,
    args: Vec<String>,
    workspace: PathBuf,
}

#[derive(Default)]
pub struct GlobalState {
    pub config: Config,
}

// Spawn a server process and return the assigned id to it and the stdin. The kill signal
// can be used to kill the server. We pass the [`ChildStdin`], even though we don't need
// it just as a safety merit.
fn spawn_server(
    mut command: Command,
    mut kill: oneshot::Receiver<ChildStdin>,
    tx: mpsc::Sender<Task>,
) -> Result<(ServerId, ChildStdin)> {
    let id = ServerId::new();

    // Spawn the process.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn the child process.")?;

    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();

    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);

        while let Some(message) = read_lsp_message(&mut reader).await? {
            if let Err(e) = tx2.send(Task::ServerMessage(id, message)).await {
                error!("Could not send message to event loop.");
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    tokio::spawn(async move {
        tokio::select! {
            _ = &mut kill => {
                info!("Killing server instance: {id:?}");
                child.start_kill();
                child.wait().await;
            },
            Ok(_) = child.wait() => {
                error!("Server instance was killed.");
            }
            else => {
                error!("Unexpected error while waiting for shutdown.");
            }
        }

        info!("Server instance shutdown: {id:?}");
        tx.send(Task::ServerShutdown(id)).await;
    });

    Ok((id, stdin))
}

fn extract_server_info(
    default_bin: &str,
    bootstrap: BootstrapMessage,
    initialize: lsp_server::Message,
) -> Result<(ServerDescriptor, Command)> {
    todo!()
}

impl ClientId {
    pub fn new() -> Self {
        static CLIENT_ID: AtomicU32 = AtomicU32::new(0);
        Self(CLIENT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl ServerId {
    pub fn new() -> Self {
        static SERVER_ID: AtomicU32 = AtomicU32::new(0);
        Self(SERVER_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl GlobalState {
    pub async fn handle_task(&mut self, task: Task) -> Result<()> {
        match task {
            Task::NewConnection(args) => self.handle_new_connection(args).await,
            Task::ClientDropped(id) => self.handle_client_drop(id).await,
            Task::ServerShutdown(id) => self.handle_server_drop(id).await,
            Task::ClientMessage(id, lsp_server::Message::Request(req)) => {
                self.handle_client_request(id, req).await
            }
            Task::ClientMessage(id, lsp_server::Message::Response(res)) => {
                self.handle_client_response(id, res).await
            }
            Task::ClientMessage(id, lsp_server::Message::Notification(not)) => {
                self.handle_client_notification(id, not).await
            }
            Task::ServerMessage(id, lsp_server::Message::Request(req)) => {
                self.handle_server_request(id, req).await
            }
            Task::ServerMessage(id, lsp_server::Message::Response(res)) => {
                self.handle_server_response(id, res).await
            }
            Task::ServerMessage(id, lsp_server::Message::Notification(not)) => {
                self.handle_server_notification(id, not).await
            }
        }
    }

    pub async fn handle_new_connection(&mut self, conn: NewConnection) -> Result<()> {
        todo!()
    }

    pub async fn handle_client_request(
        &mut self,
        id: ClientId,
        req: lsp_server::Request,
    ) -> Result<()> {
        todo!()
    }

    pub async fn handle_client_drop(&mut self, id: ClientId) -> Result<()> {
        todo!()
    }

    pub async fn handle_server_drop(&mut self, id: ServerId) -> Result<()> {
        todo!()
    }

    pub async fn handle_client_response(
        &mut self,
        id: ClientId,
        req: lsp_server::Response,
    ) -> Result<()> {
        todo!()
    }

    pub async fn handle_client_notification(
        &mut self,
        id: ClientId,
        req: lsp_server::Notification,
    ) -> Result<()> {
        todo!()
    }

    pub async fn handle_server_request(
        &mut self,
        id: ServerId,
        req: lsp_server::Request,
    ) -> Result<()> {
        todo!()
    }

    pub async fn handle_server_response(
        &mut self,
        id: ServerId,
        req: lsp_server::Response,
    ) -> Result<()> {
        todo!()
    }

    pub async fn handle_server_notification(
        &mut self,
        id: ServerId,
        req: lsp_server::Notification,
    ) -> Result<()> {
        todo!()
    }
}
