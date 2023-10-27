use anyhow::{Context, Result};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use std::{collections::HashMap, process::Stdio, sync::atomic::AtomicUsize};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{broadcast, mpsc, Mutex},
};
use tracing::{debug, error, info, trace};
use triomphe::Arc;

use lsp_server::{Message, RequestId};

use crate::{
    types::{BootstrapMessage, SessionKey},
    utils::{bind_transport, encode_lsp_message, read_lsp_message},
};

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

struct DaemonInner {
    default_ra: String,
    /// We want to allow only one concurrent creation of sessions. And holding the lock on a
    /// non-exiting key in DashMap seems risky (`entry` API has a warning about possible
    /// deadlock). So we can just use a mutex over everything.
    sessions: Mutex<HashMap<SessionKey, SessionHandle>>,
}

#[derive(Clone)]
pub struct SessionHandle(Arc<Session>);

struct Session {
    _child: Child,
    writer: Mutex<RequestSender>,
    next_client_id: AtomicUsize,
    client: DashMap<usize, mpsc::Sender<Message>>,
}

/// An opaque client id, which can not be cloned or copied.
#[derive(Debug, Hash, PartialEq, PartialOrd, Eq, Ord)]
struct ClientId(usize);

/// Isolated and self-containing struct that can be used to write messages to an instance of an
/// spawned server.
struct RequestSender {
    /// On each send we increment this number and modify the request id to be the newly
    /// generated request id. We return back that request id to the caller.
    next_request_id: i32,
    stdin: ChildStdin,
}

impl Daemon {
    fn new(default_ra: String) -> Self {
        Self {
            inner: Arc::new(DaemonInner {
                default_ra,
                sessions: Default::default(),
            }),
        }
    }
}

impl Daemon {
    /// Attach a new client to the session.
    pub async fn attach_client<T: AsyncRead + AsyncWrite + Send + 'static>(
        &self,
        io: T,
    ) -> Result<()> {
        let (mut writer, mut reader) = bind_transport(io).split();

        // Read the bootstrap message.
        let bootstrap = reader.next().await.context("Disconnected")??;
        let bootstrap: BootstrapMessage =
            serde_json::from_slice(&bootstrap).context("Invalid bootstrap message")?;

        // Now that we have the bootstrap message, extract the session key and the command
        // that would be required to spawn the server (if not already spawned.)
        let (sk, cmd) = bootstrap
            .parse(&self.inner.default_ra)
            .context("Could not process bootstrap message.")?;

        // Get the session that the client wishes to connect to.
        let session = {
            let mut guard = self.inner.sessions.lock().await;
            match guard.entry(sk) {
                std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let handle = spawn_server(cmd).context("Failed to spawn the server.")?;
                    e.insert(handle.clone());
                    handle
                }
            }
        };

        todo!()

        Ok(())
    }
}

impl Session {
    /// Send the given message to the server process. If the message is a request returns the
    /// request id that was assigned to this message.
    async fn send(&self, mut message: Message) -> Result<Option<RequestId>> {
        let mut guard = self.writer.lock().await;

        let result = match &mut message {
            Message::Request(req) => {
                let new_id = guard.next_request_id;
                guard.next_request_id += 1;
                req.id = new_id.into();
                Some(RequestId::from(new_id))
            }
            Message::Response(_) => None,
            Message::Notification(_) => None,
        };

        let encoded = encode_lsp_message(message);
        guard.stdin.write_all(&encoded).await?;

        Ok(result)
    }
}

/// Spawn the server child process along with the reader loop.
pub fn spawn_server(mut command: Command) -> Result<SessionHandle> {
    // Spawn the process.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // TODO
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn the child process.")?;

    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    let session = Arc::new(Session {
        _child: child,
        writer: Mutex::new(RequestSender {
            next_request_id: 1,
            stdin,
        }),
        next_client_id: AtomicUsize::new(0),
        client: DashMap::new(),
    });

    // This task will read a message constantly from the stdout of the spawned
    // process and forwards it to the client that should receive it.
    let task_session = session.clone();
    tokio::spawn(async move {
        let session = task_session;
        let mut reader = BufReader::new(stdout);

        while let Some(message) = read_lsp_message(&mut reader).await? {
            trace!("> {}", serde_json::to_string(&message).unwrap());

            // TODO: redirect.
        }

        Ok::<(), anyhow::Error>(())
    });

    tokio::spawn(async move {});

    Ok(SessionHandle(session))
}
