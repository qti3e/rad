//! Even though the main purpose of this tool is to keep rust-analyzer in the background, it
//! can be used as a multiplexer. being a 'good' multiplexer is the secondary objective of
//! this project.

// TODO:
// Document management : Can be improved.
// Server garbage collection
// Window progress bar
// Better logs (Terminal UI)

use anyhow::{anyhow, bail, Context, Result};
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use fxhash::{FxHashMap, FxHashSet};
use lsp_types::notification::Notification;
use serde::{Deserialize, Serialize};
use std::{
    process::Stdio,
    sync::atomic::{AtomicU32, Ordering},
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{error, info};

use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};

use crate::{
    types::{BootstrapMessage, Config, SessionKey},
    utils::{encode_lsp_message, find_project_root, read_lsp_message},
};

/// The writer we use to send stuff to the client. Maybe later we can make this some
/// sort of generic and support a non-UDS communication to the client.
pub type ClientFramedWriter = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

#[derive(Default)]
pub struct GlobalState {
    pub config: Config,
    tx: Option<mpsc::Sender<Task>>,
    client: FxHashMap<ClientId, ClientState>,
    server: FxHashMap<ServerId, ServerState>,
    server_descriptors: FxHashMap<ServerDescriptor, ServerId>,
}

struct ClientState {
    /// The writer half of the socket for this client, this is how we send stuff to the client.
    writer: ClientFramedWriter,
    /// Active server that this client is connected to.
    server_id: ServerId,
    // Have we received `initialized` notification from this client or not?
    is_initialized: bool,
    /// The request id that the client used to send the 'initialize' request.
    initialize_request_id: lsp_server::RequestId,
    /// The information that we cared about extracted from the `initialize` request sent by this
    /// client.
    info: ClientLspInfo,
    /// The next request id that we can use when sending an out going request.
    next_request_id: i32,
    /// Map each request that we sent to the client to the request id on the server side.
    request_id_map: FxHashMap<lsp_server::RequestId, lsp_server::RequestId>,
    /// If the client is not the primary client and sends us a `workspace/didChangeConfiguration`
    /// notification we do not send it to the server, because the current primary is in charge.
    ///
    /// Instead we keep it here so that we can send it to the server once this connection becomes
    /// primary.
    last_change_configuration_notification: Option<lsp_server::Notification>,
}

/// Implements [`From<lsp_types::InitializeParams>`] and can let us track the info we care
/// about which we can gather from that, mostly about client's capabilities.
struct ClientLspInfo {
    work_done_progress: bool,
}

struct ServerState {
    /// This is how we send messages to the server.
    stdin: ChildStdin,
    /// The kill switch which we can use to kill the child process for this server.
    kill_sender: oneshot::Sender<ChildStdin>,
    /// All of the clients that are connected to this server. The insertion order here matters.
    /// The first connection in this set is meant to be the primary.
    connections: Vec<ClientId>,
    /// The result of the initialization of this server. If all of the connections of the server
    /// drop while this value is still [`None`] we drop the server and do not keep it.
    initialize_result: Option<lsp_types::InitializeResult>,
    // Have we received `initialized` notification from any of the clients yet?
    is_initialized: bool,
    /// The next request id that we can use when sending an out going request.
    next_request_id: i32,
    /// Map each request id that we used to send a request to the language server to the client
    /// that made this request and the request id that the client used to do so. We need this
    /// data on the way back.
    request_id_map: FxHashMap<lsp_server::RequestId, (ClientId, lsp_server::RequestId)>,
    /// The requests that we have sent to the (primary) client and are waiting for the responses.
    ///
    /// The life time of a server request is bit more complex than client's request, and needs
    /// proper care.
    ///
    /// A 'client' is known to be the 'primary' client if it is the oldest client, a server may
    /// not always have an active client, but if it does we treat `self.connections[0]` as the
    /// oldest client.
    ///
    /// Since we have to answer all of the server's requests, we try to do the best we can, which
    /// involves:
    ///
    /// 1. Insert the request to this map
    /// 2. If no primary:
    ///     2.1. return
    /// 3. Send the request to the primary
    ///
    /// When a client sends a response back we always check here, and make sure that the request
    /// is still pending from the server's point of view before sending the response to the server.
    ///
    /// When we have a new primary (either the current primary drops the connection which in case
    /// the next one in the list of connections becomes the new primary, or when we have a new
    /// new connection when there was none at that time), we should resend these request to the
    /// new primary because there is no chance that the old primary (if it existed) is going to
    /// send back the responses - because it is dead.
    pending_server_requests: FxHashMap<lsp_server::RequestId, lsp_server::Request>,
    /// The capabilities that are registered by the server using `client/registerCapability`, the
    /// registration is only stored when we get a confirmation from the first primary connection
    /// that we sent this request to.
    registered_capabilities: Vec<lsp_types::Registration>,
    /// The open documents by the current clients. Currently, one limitation we have is the fact
    /// that we support only one client on each document right now. But this can be improved to
    /// support more than that.
    document: FxHashMap<lsp_types::Url, OpenDocument>,
}

struct OpenDocument {
    /// The current client that has ownership over this text document.
    opened_by: ClientId,
    /// The is_modified flag determines if the content is changed without
    /// being stored on the disk. We reject new connections if that's the
    /// case.
    is_modified: bool,
}

#[derive(Debug)]
pub enum Task {
    NewConnection(NewConnection),
    ClientMessage(ClientId, lsp_server::Message),
    ServerMessage(ServerId, lsp_server::Message),
    ClientDropped(ClientId),
    ServerShutdown(ServerId),
}

#[derive(Debug)]
pub struct NewConnection {
    pub id: ClientId,
    pub writer: ClientFramedWriter,
    pub bootstrap: BootstrapMessage,
    pub request: lsp_server::Request,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ServerDescriptor {
    bin: String,
    args: Vec<String>,
    workspace: String,
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub struct ClientId(u32);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub struct ServerId(u32);

impl GlobalState {
    pub fn set_sender(&mut self, sender: mpsc::Sender<Task>) {
        assert!(self.tx.is_none());
        self.tx = Some(sender);
    }

    pub fn sender(&self) -> &mpsc::Sender<Task> {
        self.tx.as_ref().expect("Sender to be set.")
    }

    pub async fn handle_task(&mut self, task: Task) -> Result<()> {
        match task {
            Task::NewConnection(args) => self.handle_new_connection(args).await,
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
            Task::ClientDropped(id) => self.drop_client(id).await,
            Task::ServerShutdown(id) => self.handle_server_drop(id).await,
        }
    }

    // DONE
    pub async fn handle_new_connection(&mut self, conn: NewConnection) -> Result<()> {
        info!("New connection");

        // We have a new client connection. We are in one of the following situations:
        // 1. The client is connecting to a new project.
        // 2. The client is connecting to an already existing project.
        //  2.1. There is no active clients. They have all disconnect before.
        //  2.2. There is one or more active clients.

        let client_id = conn.id;
        let (req_id, params) = conn
            .request
            .extract::<lsp_types::InitializeParams>("initialize")
            .context("Expected 'initialize' request.")?;

        let (server_descriptor, command) =
            extract_server_info(&self.config.rust_analyzer_bin, conn.bootstrap, &params)
                .context("Failed to figure out project descriptor")?;

        // Find the server id for this description or assign a new one.
        let (existed, sid) = self
            .server_descriptors
            .get(&server_descriptor)
            .map(|sid| (true, *sid))
            .unwrap_or_else(|| (false, ServerId::new()));

        // Create the empty client state struct but we only insert it in a tailing block of code in
        // the cfg to ensure consistency.
        let client_state = ClientState {
            writer: conn.writer,
            server_id: sid,
            is_initialized: false,
            initialize_request_id: req_id.clone(),
            info: ClientLspInfo::from(&params),
            next_request_id: 0,
            request_id_map: FxHashMap::default(),
            last_change_configuration_notification: None,
        };

        if existed {
            let server_state = self.server.get_mut(&sid).unwrap();

            // The following code up the return statement doesn't have any early exits to
            // perform the insertion.
            self.client.insert(client_id, client_state);

            // Send the initialize response to the client if it does exist. There is a small
            // possibility that the result might not exists if we have a second connection that
            // comes in before the server has responded to the first one. But we don't need to
            // care about that.
            if let Some(result) = &server_state.initialize_result {
                let message = lsp_server::Message::Response(lsp_server::Response {
                    id: req_id,
                    result: Some(serde_json::to_value(&result).unwrap()),
                    error: None,
                });

                self.client
                    .get_mut(&client_id)
                    .unwrap()
                    .write_message(&message)
                    .await;
            }

            return Ok(());
        }

        // If we're here it means that the server did not exist and that we need to create
        // both the server and the client entries.

        let (kill_tx, kill_rx) = oneshot::channel::<ChildStdin>();
        let stdin = spawn_server(sid, command, kill_rx, self.sender().clone())
            .context("Failed to spawn the language server.")?;

        let mut server_state = ServerState {
            stdin,
            kill_sender: kill_tx,
            // This client id will be inserted when we get back 'initialized' notification from
            // the client.
            connections: vec![],
            initialize_result: None,
            is_initialized: false,
            next_request_id: 0,
            request_id_map: FxHashMap::default(),
            pending_server_requests: FxHashMap::default(),
            registered_capabilities: Vec::new(),
            document: FxHashMap::default(),
        };

        // We can send the 'initialize' request to the server.
        server_state
            .send_request(
                client_id,
                lsp_server::Request {
                    id: req_id,
                    method: "initialize".into(),
                    params: serde_json::to_value(hijack_initialize_params(params)).unwrap(),
                },
            )
            .await;

        // No more early returns from the line to the `Ok(())`. Perform insertion.
        self.client.insert(client_id, client_state);
        self.server.insert(sid, server_state);
        self.server_descriptors.insert(server_descriptor, sid);

        Ok(())
    }

    // DONE
    pub async fn handle_client_request(
        &mut self,
        id: ClientId,
        req: lsp_server::Request,
    ) -> Result<()> {
        info!("Client request: {}", req.method);

        let client = self.client.get_mut(&id).context("Client not found.")?;

        if req.method == "shutdown" {
            client
                .write_message(&lsp_server::Response {
                    id: req.id,
                    result: Some(serde_json::Value::Null),
                    error: None,
                })
                .await;
            return Ok(());
        }

        let server = self.server.get_mut(&client.server_id).unwrap();
        server.send_request(id, req).await;

        // TODO: The client is sending a request which may involve a document. Make sure the client
        // has ownership over that document. If not we should respond to the client here with an
        // error.

        Ok(())
    }

    // DONE
    pub async fn handle_client_response(
        &mut self,
        id: ClientId,
        mut res: lsp_server::Response,
    ) -> Result<()> {
        let client = self.client.get_mut(&id).context("Client not found.")?;
        let server = self.server.get_mut(&client.server_id).unwrap();

        // Re assign the response id to what we used to send it out to this client.
        let original_id = client
            .request_id_map
            .remove(&res.id)
            .context("Request id not found.")?;
        info!("Client response {} (org={})", res.id, original_id);
        res.id = original_id;

        // Check to see if the server is still interested in hearing back about this response.
        if let Some(req) = server.pending_server_requests.remove(&res.id) {
            let is_ok = res.error.is_none();

            server
                .write_message(lsp_server::Message::Response(res))
                .await;

            if is_ok && req.method == "client/registerCapability" {
                let params: lsp_types::RegistrationParams = serde_json::from_value(req.params)
                    .context("Failed to deserialize 'client/registerCapability' request.")?;

                server
                    .registered_capabilities
                    .extend(params.registrations.into_iter());
            } else if is_ok && req.method == "client/unregisterCapability" {
                let params: lsp_types::UnregistrationParams = serde_json::from_value(req.params)
                    .context("Failed to deserialize 'client/unregisterCapability' request.")?;

                let unregistered = params
                    .unregisterations
                    .iter()
                    .map(|u| (&u.id, &u.method))
                    .collect::<FxHashSet<_>>();

                server
                    .registered_capabilities
                    .retain(|v| !unregistered.contains(&(&v.id, &v.method)));
            }
        }

        Ok(())
    }

    pub async fn handle_client_notification(
        &mut self,
        id: ClientId,
        not: lsp_server::Notification,
    ) -> Result<()> {
        // This should have already been handled as `handle_client_drop` by
        // the caller. This should never reach this far.
        assert_ne!(not.method, "exit");

        let result = self.handle_client_notification_inner(id, not).await;

        if result.is_err() {
            let _ = self.drop_client(id).await;
        }

        result
    }

    // WIP
    async fn handle_client_notification_inner(
        &mut self,
        id: ClientId,
        mut not: lsp_server::Notification,
    ) -> Result<()> {
        info!("Client notification: {}", not.method);

        let client = self.client.get_mut(&id).context("Client not found.")?;
        let server = self.server.get_mut(&client.server_id).unwrap();
        let is_primary = server.connections.len() == 0 || server.connections[0] == id;

        if not.method == "initialized" {
            client.is_initialized = true;
            server.connections.push(id);

            if !server.is_initialized {
                server.is_initialized = true;
                server
                    .write_message(lsp_server::Message::Notification(not))
                    .await;
            }

            // If connection is 'primary' during this stage, it is a new primary
            // which we should process.
            if is_primary {
                let server_id = client.server_id;
                self.new_primary(server_id).await;
            }

            return Ok(());
        }

        // Primary has the ownership over configuration
        if !is_primary && not.method == "workspace/didChangeConfiguration" {
            client.last_change_configuration_notification = Some(not);
            return Ok(());
        }

        if let Some(params) =
            parse_notification::<lsp_types::notification::DidOpenTextDocument>(&not)?
        {
            if server.document.contains_key(&params.text_document.uri) {
                bail!("Document is already open by another client.");
            }

            server.document.insert(
                params.text_document.uri,
                OpenDocument {
                    opened_by: id,
                    is_modified: false,
                },
            );
        }

        if let Some(params) =
            parse_notification::<lsp_types::notification::DidChangeTextDocument>(&not)?
        {
            let doc = server
                .document
                .get_mut(&params.text_document.uri)
                .with_context(|| format!("Text document not open: {}", params.text_document.uri))?;

            if doc.opened_by != id {
                bail!("Text document not owned by the client.");
            }

            doc.is_modified = true;
        }

        if let Some(params) =
            parse_notification::<lsp_types::notification::DidCloseTextDocument>(&not)?
        {
            let doc = server
                .document
                .get_mut(&params.text_document.uri)
                .with_context(|| format!("Text document not open: {}", params.text_document.uri))?;

            if doc.opened_by != id {
                bail!("Text document not owned by the client.");
            }

            server.document.remove(&params.text_document.uri);
        }

        if let Some(params) =
            parse_notification::<lsp_types::notification::DidSaveTextDocument>(&not)?
        {
            let doc = server
                .document
                .get_mut(&params.text_document.uri)
                .with_context(|| format!("Text document not open: {}", params.text_document.uri))?;

            if doc.opened_by != id {
                bail!("Text document not owned by the client.");
            }

            doc.is_modified = false;
        }

        if let Some(params) = parse_notification::<lsp_types::notification::DidRenameFiles>(&not)? {
            for rename in params.files {
                let old_uri = lsp_types::Url::parse(&rename.old_uri)
                    .context("Failed to parse document URI")?;
                let new_uri = lsp_types::Url::parse(&rename.new_uri)
                    .context("Failed to parse document URI")?;

                let doc = server
                    .document
                    .get_mut(&old_uri)
                    .with_context(|| format!("Text document not open: {}", old_uri))?;

                if doc.opened_by != id {
                    bail!("Text document not owned by the client.");
                }

                let doc = server.document.remove(&old_uri).unwrap();
                server.document.insert(new_uri, doc);
            }
        }

        if not.method == "$/cancelRequest" {
            #[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
            pub struct CancelParams {
                pub id: lsp_server::RequestId,
            }

            let mut params: CancelParams = serde_json::from_value(not.params)
                .context("Failed to parse client notification")?;

            let request_id = if let Some(request_id) =
                server.get_server_request_id_from_client_request_id(id, &params.id)
            {
                request_id.clone()
            } else {
                error!(
                    "Cancellation request ignored: Couldn't find the request id ({})",
                    params.id
                );
                return Ok(());
            };

            params.id = request_id;
            not.params = serde_json::to_value(params).unwrap();
        }

        server
            .write_message(lsp_server::Message::Notification(not))
            .await;

        Ok(())
    }

    // WIP
    pub async fn handle_server_request(
        &mut self,
        id: ServerId,
        req: lsp_server::Request,
    ) -> Result<()> {
        info!("Server request: {}", req.method);

        let server = self.server.get_mut(&id).context("Server not found.")?;

        if req.method.starts_with("window/workDoneProgress/") {
            if req.method == "window/workDoneProgress/create" {
                // TODO: Notify the clients.
            }

            if req.method == "window/workDoneProgress/cancel" {
                // TODO: Notify the clients.
            }

            server
                .write_message(lsp_server::Message::Response(lsp_server::Response {
                    id: req.id,
                    result: Some(serde_json::Value::Null),
                    error: None,
                }))
                .await;

            return Ok(());
        }

        if let Some(client_id) = server.connections.get(0) {
            server.pending_server_requests.insert(req.id.clone(), req);
            let client = self.client.get_mut(client_id).unwrap();
        } else {
            server.pending_server_requests.insert(req.id.clone(), req);
        }

        Ok(())
    }

    // DONE
    pub async fn handle_server_response(
        &mut self,
        id: ServerId,
        mut res: lsp_server::Response,
    ) -> Result<()> {
        let server = self.server.get_mut(&id).context("Server not found.")?;

        // If 'initialize_result' is none, we are expecting it as the first response the server
        // sends us.
        if server.initialize_result.is_none() {
            // Normally, we would care about this for a unicast req-res. But 'initialize' is sent
            // from the server to multiple receivers. That is why we keep `initialize_request_id`
            // on each client.
            server.request_id_map.remove(&res.id);

            // Send this response to every client that is still connected regardless of it being
            // an error or not. They deserve to see it as it exactly is.
            //
            // We can not use `server.connection` here because the connections are only added
            // after the client sends the 'initialized' notification which only happens after
            // it gets what we're sending here. So right now we can naively just go through
            // the list of all the clients.
            for client_state in self
                .client
                .values_mut()
                .filter(|client| client.server_id == id)
            {
                let req_id = client_state.initialize_request_id.clone();
                res.id = req_id;
                client_state.write_message(&res).await;
            }

            let mut err_message: &str;

            if let Some(e) = &res.error {
                err_message = &e.message;
            } else {
                if let Some(Ok(result)) = res
                    .result
                    .map(serde_json::from_value::<lsp_types::InitializeResult>)
                {
                    server.initialize_result = Some(result);
                    return Ok(());
                }

                err_message = "Invalid 'InitializeResult' from the server.";
            };

            // If we're here the server has responded to 'initialize' request with an
            // error. There is no good keeping either the client nor the server. Just
            // drop the server.
            self.drop_server(id).await;

            bail!("Initialization of the server failed: '{}'", err_message);
        }

        // Right now we don't have any specific logic monitoring the requests. But in future
        // if we ever need any this is where we can place those.

        // A normal response: We just see who sent this and forward it.
        let (client_id, org_id) = server
            .request_id_map
            .remove(&res.id)
            .context("Could not find request.")?;

        info!("Server response (id={})", org_id);

        // Client might have been disconnected.
        let client = self
            .client
            .get_mut(&client_id)
            .context("Client not found: dropping response.")?;

        // Change the response id to the original id and write it to the user.
        res.id = org_id;
        client.write_message(&res).await;

        Ok(())
    }

    // WIP
    pub async fn handle_server_notification(
        &mut self,
        id: ServerId,
        not: lsp_server::Notification,
    ) -> Result<()> {
        let server = self.server.get_mut(&id).context("Server not found.")?;

        if not.method == "$/progress" {
            let params: lsp_types::ProgressParams =
                serde_json::from_value(not.params).context("Could not parse server message")?;

            return Ok(());
        }

        if not.method == "experimental/serverStatus" {
            info!("Server is healthy!");
        }

        // Broadcast every notification to all of the clients.
        for client_id in &server.connections {
            let client = self.client.get_mut(client_id).unwrap();
            client.write_message(&not).await;
        }

        Ok(())
    }

    pub async fn drop_client(&mut self, id: ClientId) -> Result<()> {
        info!("Dropping client {id:?}");

        let mut client = self.client.remove(&id).context("Client not found.")?;

        // This should close the IO for the client.
        client
            .write_message(&lsp_server::Notification {
                method: "$/exit".to_string(),
                params: serde_json::Value::Null,
            })
            .await;

        // This can actually be None when we're dropping the server itself. Which can
        // happen when the server process exits.
        let server_id = client.server_id;
        let server = self
            .server
            .get_mut(&server_id)
            .context("Server not found.")?;
        let maybe_index = server.connections.iter().position(|v| *v == id);

        // This can be none when the client gets disconnected before it sends the 'initialized'
        // notification.
        let switch_primary = if let Some(index) = maybe_index {
            server.connections.remove(index);
            index == 0 && !server.connections.is_empty()
        } else {
            false
        };

        let docs_to_close = server
            .document
            .iter()
            .filter_map(|(uri, doc)| (doc.opened_by == id).then_some(uri))
            .cloned()
            .collect::<Vec<_>>();

        for uri in docs_to_close {
            info!("Sending 'didClose' on behalf of the client.");

            server.document.remove(&uri);

            server
                .write_message(lsp_server::Message::Notification(
                    lsp_server::Notification {
                        method: lsp_types::notification::DidCloseTextDocument::METHOD.to_string(),
                        params: serde_json::to_value(lsp_types::DidCloseTextDocumentParams {
                            text_document: lsp_types::TextDocumentIdentifier { uri },
                        })
                        .unwrap(),
                    },
                ))
                .await;
        }

        if switch_primary {
            // The index is zero which means this is the oldest client that was connected and
            // now that we have removed it a new client is taking the role of the primary so
            // we have to make the switch.
            self.new_primary(server_id).await;
        }

        Ok(())
    }

    pub async fn handle_server_drop(&mut self, id: ServerId) -> Result<()> {
        info!("Server dropping");
        if self.server.contains_key(&id) {
            self.drop_server(id).await;
        }
        Ok(())
    }

    async fn drop_server(&mut self, id: ServerId) {
        let mut server = self.server.remove(&id).expect("The server to exists.");
        let clients = self
            .client
            .iter()
            .filter_map(|(cid, client)| (client.server_id == id).then_some(*cid))
            .collect::<Vec<_>>();

        for cid in clients {
            self.drop_client(cid).await;
        }

        server.kill_sender.send(server.stdin);
    }

    async fn new_primary(&mut self, id: ServerId) {
        // We have a new primary connection on the provided server. We have a few obligations:
        // 1. Send the workspace configuration of this client to the server.
        // 2. Send every pending server request to the new primary.

        let server = self.server.get_mut(&id).expect("The server to exists.");
        let client_id = server.connections[0];
        let client = self
            .client
            .get_mut(&client_id)
            .expect("The client to exists");

        if let Some(not) = client.last_change_configuration_notification.take() {
            server
                .write_message(lsp_server::Message::Notification(not))
                .await;
        }

        for (_, req) in &server.pending_server_requests {
            client.send_request(req.clone()).await;
        }
    }
}

impl ServerState {
    pub async fn send_request(&mut self, client: ClientId, mut request: lsp_server::Request) {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.request_id_map.insert(id.into(), (client, request.id));
        request.id = id.into();
        self.write_message(lsp_server::Message::Request(request))
            .await;
    }

    pub async fn write_message(&mut self, message: lsp_server::Message) {
        let bytes = encode_lsp_message(message);
        if let Err(e) = self.stdin.write_all(&bytes).await {
            error!("Failed to write to server: {e}");
        }
    }

    pub fn get_server_request_id_from_client_request_id(
        &self,
        client: ClientId,
        id: &lsp_server::RequestId,
    ) -> Option<&lsp_server::RequestId> {
        self.request_id_map
            .iter()
            .find(|(_, (e_client, e_id))| e_client == &client && e_id == id)
            .map(|(rid, _)| rid)
    }
}

impl ClientState {
    async fn send_request(&mut self, mut request: lsp_server::Request) {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.request_id_map.insert(id.into(), request.id.clone());
        request.id = id.into();
        self.write_message(&request).await;
    }

    pub async fn write_message<T: Serialize>(&mut self, message: &T) {
        let bytes = serde_json::to_vec(&message).unwrap();
        if let Err(e) = self.writer.send(bytes.into()).await {
            error!("Failed to write to client: {e}");
        }
    }
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

impl From<&lsp_types::InitializeParams> for ClientLspInfo {
    fn from(value: &lsp_types::InitializeParams) -> Self {
        let work_done_progress = value
            .capabilities
            .window
            .as_ref()
            .map(|window| window.work_done_progress)
            .flatten()
            .unwrap_or(false);

        Self { work_done_progress }
    }
}

// Spawn a server process and return the assigned id to it and the stdin. The kill signal
// can be used to kill the server. We pass the [`ChildStdin`], even though we don't need
// it just as a safety merit.
fn spawn_server(
    id: ServerId,
    mut command: Command,
    mut kill: oneshot::Receiver<ChildStdin>,
    tx: mpsc::Sender<Task>,
) -> Result<ChildStdin> {
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

    Ok(stdin)
}

// Map the information provided by the client during bootstrap and initialization to the
// `ServerDescriptor`.
fn extract_server_info(
    default_bin: &str,
    bootstrap: BootstrapMessage,
    params: &lsp_types::InitializeParams,
) -> Result<(ServerDescriptor, Command)> {
    #[allow(deprecated)] // `root_path` is deprecated.
    // RAD_ROOT > root_uri > root_path > find_project_root(cwd)
    // Maybe we can also use the `cwd` to make this infallible? But really, how fault
    // tolerant should this thing be? Seems like such an overkill.
    let workspace = bootstrap
        .envs
        .get("RAD_ROOT")
        .cloned()
        .or(params.root_uri.clone().map(|uri| uri.to_string()))
        .or(params.root_path.clone())
        .map(|v| Ok::<_, anyhow::Error>(v))
        .unwrap_or_else(|| {
            find_project_root(&bootstrap.cwd).map(|v| v.to_string_lossy().to_string())
        })
        .context("Could not get the workspace from the bootstrap information")?;

    // Get the rust analyzer binary from the env variables that are set in the child
    // or default it to the one we have.
    let bin = bootstrap
        .envs
        .get("RAD_RA")
        .map(|s| s.as_str())
        .unwrap_or(default_bin)
        .to_string();

    // Create the command by setting the binary name and the environment variables and
    // the cwd.
    let mut cmd = Command::new(&bin);
    cmd.args(bootstrap.args.iter());
    cmd.env_clear();
    cmd.envs(bootstrap.envs);
    cmd.current_dir(bootstrap.cwd); // TODO: Maybe `project_root`?

    let sd = ServerDescriptor {
        bin,
        args: bootstrap.args,
        workspace,
    };

    Ok((sd, cmd))
}

/// This function allows us to get the initialize parameters sent by the client and modify it
/// before sending it to the language server. As an example of why we need to do so is the window
/// notification functionality, which is something we always want to have enabled for our self.
fn hijack_initialize_params(params: lsp_types::InitializeParams) -> lsp_types::InitializeParams {
    // TODO: enable work_done_progress.
    params
}

#[inline(always)]
fn parse_notification<N: lsp_types::notification::Notification>(
    notification: &lsp_server::Notification,
) -> Result<Option<N::Params>> {
    if notification.method != N::METHOD {
        return Ok(None);
    }

    let params = serde_json::from_value(notification.params.clone())
        .with_context(|| format!("Failed to parse parameters for '{}'", N::METHOD))?;

    Ok(Some(params))
}
