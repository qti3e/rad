use anyhow::{bail, Context};
use futures::{SinkExt, StreamExt};
use resolve_path::PathResolveExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::{collections::HashMap, env, path::PathBuf};
use tokio::fs;
use tokio::io::{self, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{error, info};

struct Config {
    /// The Unix domain socket for the daemon.
    pub socket_path: PathBuf,
    /// The rust analyzer binary.
    pub rust_analyzer_bin: String,
}

impl Config {
    pub fn load_from_env() -> anyhow::Result<Self> {
        let socket_path = env::var("RAD_SOCKET")
            .unwrap_or_else(|_| "~/.rad/ipc.sock".to_string())
            .try_resolve()
            .context("Could not resolve IPC socket path")?
            .to_path_buf();

        let rust_analyzer_bin = env::var("RAD_RA").unwrap_or_else(|_| "rust-analyzer".to_string());

        Ok(Self {
            socket_path,
            rust_analyzer_bin,
        })
    }
}

/// The IPC message is sent between the daemon and the LSP binary.
#[derive(Debug, Serialize, Deserialize)]
enum IpcMessage {
    /// Bootstrap a language server with the provided information.
    Bootstrap(BootstrapMessage),
    /// An LSP message. Can be anything from a `Request`, `Response` or `Notification`.
    LspMessage(lsp_server::Message),
}

#[derive(Debug, Serialize, Deserialize)]
struct BootstrapMessage {
    /// The current working directory for the spawned LSP process.
    cwd: PathBuf,
    /// The set of environment variables.
    env: HashMap<String, String>,
    /// The command line arguments.
    args: Vec<String>,
}

#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct ServerIdentifier {
    /// Absolute path to the project root.
    project_root: PathBuf,
    /// The rust analyzer binary used for this project.
    rust_analyzer_bin: String,
}

#[tokio::main]
async fn main() {
    // Only start the sub command if we are a daemon.
    let exit = if env::args().nth(1) == Some("daemon".to_string()) {
        run_daemon().await
    } else {
        run_daemon_client().await
    };

    match exit {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::exit(-1);
        }
    }
}

/// Entry point of the daemon binary.
async fn run_daemon() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    info!("Running the daemon");
    let config = Config::load_from_env().context("Failed to load config")?;

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

    while let Ok((stream, addr)) = listener.accept().await {
        info!("Accepted client {addr:?}");
        let bin = config.rust_analyzer_bin.clone();
        tokio::spawn(async {
            match handle_client(bin, stream).await {
                Ok(_) => info!("Client detached."),
                Err(e) => error!("Client detaced {e:?}"),
            }
        });
    }

    Ok(())
}

async fn handle_client(default_ra: String, stream: UnixStream) -> anyhow::Result<()> {
    let (mut writer, mut reader) = bind_transport(stream).split();
    let Some(Ok(bootstrap)) = reader.next().await else {
        bail!("Connection close before getting bootstrap message.")
    };

    let Ok(IpcMessage::Bootstrap(bootstrap)): Result<IpcMessage, _> =
        serde_json::from_slice(&bootstrap)
    else {
        bail!("Invalid bootstrap messaged.");
    };

    let (_id, mut command) =
        build_command(default_ra, bootstrap).context("Could not build the command")?;

    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut stdin = child.stdin.take().unwrap();

    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        while let Some(message) = read_lsp_message(&mut reader).await? {
            println!("> {message:?}");
            let bytes = serde_json::to_vec(&message).unwrap().into();
            writer.send(bytes).await.context("Failed to write.")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    while let Some(Ok(message)) = reader.next().await {
        let IpcMessage::LspMessage(message) =
            serde_json::from_slice(&message).expect("Invalid message")
        else {
            bail!("invalid");
        };

        println!("< {message:?}");

        let message = encode_lsp_message(message);
        stdin
            .write_all(&message)
            .await
            .context("Could not write to stdin")?;
    }

    task.await.context("Could not join spawned task")?
}

/// Entry point of the LSP server. Which is also the daemon's client.
async fn run_daemon_client() -> anyhow::Result<()> {
    let config = Config::load_from_env().context("Failed to load config")?;
    let socket = UnixStream::connect(config.socket_path)
        .await
        .context("Failed to connect to the daemon. Is it running?")?;

    let (mut socket_writer, mut socket_reader) = bind_transport(socket).split();

    // Reader: read from stdin and write to the Unix socket.
    tokio::spawn(async move {
        // Write the bootstrap message.
        let cwd = env::current_dir().context("Could not get the current working directory")?;
        let message = IpcMessage::Bootstrap(BootstrapMessage {
            cwd,
            env: env::vars().collect(),
            args: env::args().skip(1).collect(),
        });
        let bytes = serde_json::to_vec(&message).unwrap().into();
        socket_writer
            .send(bytes)
            .await
            .context("Failed to write.")?;

        // Handle the stdin loop.
        let mut reader = BufReader::new(io::stdin());

        while let Some(message) = read_lsp_message(&mut reader).await? {
            let is_exit =
                matches!(&message, lsp_server::Message::Notification(n) if n.method == "exit");

            let message = IpcMessage::LspMessage(message);
            let bytes = serde_json::to_vec(&message).unwrap().into();
            socket_writer
                .send(bytes)
                .await
                .context("Failed to write.")?;

            if is_exit {
                break;
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    // Write stuff out to stdout.
    let mut stdout = std::io::stdout().lock();
    while let Some(Ok(message)) = socket_reader.next().await {
        let message: lsp_server::Message =
            serde_json::from_slice(&message).expect("Invalid message");
        message.write(&mut stdout)?;
    }

    Ok(())
}

/// Given the bootstrap message from a client, build the server identifier and the command that
/// must be used to spawn the server.
fn build_command(
    default_ra: String,
    bootstrap: BootstrapMessage,
) -> anyhow::Result<(ServerIdentifier, Command)> {
    let project_root = find_project_root(&bootstrap.cwd)?;

    // Get the rust analyzer binary from the env variables that are set in the child
    // or default it to the one we have.
    let rust_analyzer_bin = bootstrap
        .env
        .get("RAD_RA")
        .map(|s| s.clone())
        .unwrap_or(default_ra);

    // Create the command by setting the binary name and the environment variables and
    // the cwd.
    let mut cmd = Command::new(&rust_analyzer_bin);
    cmd.args(bootstrap.args);
    cmd.env_clear();
    cmd.envs(bootstrap.env);
    cmd.current_dir(bootstrap.cwd); // TODO: Maybe `project_root`?

    Ok((
        ServerIdentifier {
            project_root,
            rust_analyzer_bin,
        },
        cmd,
    ))
}

/// Given any input that can be treated as a `PathBuf` walk the directories up to the root and
/// returns the path directory that can be treated as a session root.
fn find_project_root(path: impl Into<PathBuf>) -> anyhow::Result<PathBuf> {
    let mut result = None;
    let mut current: PathBuf = path.into();
    let param = current.clone(); // To report error.

    assert!(
        current.is_absolute(),
        "Expected project path to be an absolute path."
    );

    loop {
        current.push("Cargo.lock");
        let exists = current.is_file();
        current.pop();

        if exists {
            // Cargo.lock is a hard stop for us. It means we're at a crate.
            return Ok(current);
        }

        // With cargo.toml we look for the deepest one since the project can be a work space.
        // This will also lead us to find the `Cargo.lock` of the entire workspace.
        current.push("Cargo.toml");
        let exists = current.is_file();
        current.pop();
        if exists {
            result = Some(current.clone());
        }

        // The `.rad_root` file can be used as a hack to pin where the root of the file
        // system is.
        current.push(".rad_root");
        let exists = current.is_file();
        current.pop();
        if exists {
            break;
        }

        // we have reached the root directory and there is no more parent for us to visit.
        if !current.pop() {
            break;
        }
    }

    if result.is_none() {
        bail!(
            "Could not find the project root for {:?}.",
            param.to_string_lossy()
        );
    }

    Ok(result.unwrap())
}

/// Read an LSP message from a [`BufReader`].
async fn read_lsp_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> anyhow::Result<Option<lsp_server::Message>> {
    let mut read_buf = String::new();
    // read headers.
    let mut size = None;
    loop {
        read_buf.clear();
        if reader.read_line(&mut read_buf).await? == 0 {
            return Ok(None);
        }
        if !read_buf.ends_with("\r\n") {
            bail!("malformed header: {read_buf:?}");
        }
        let buf = &read_buf[..read_buf.len() - 2];
        if buf.is_empty() {
            break;
        }
        let mut parts = buf.splitn(2, ": ");
        let header_name = parts.next().unwrap();
        let header_value = parts
            .next()
            .with_context(|| format!("malformed header: {read_buf:?}"))?;
        if header_name.eq_ignore_ascii_case("Content-Length") {
            size = Some(
                header_value
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }

    let size = size.ok_or_else(|| anyhow::anyhow!("no Content-Length"))?;

    let mut buf = vec![0; size];
    reader.read_exact(&mut buf).await?;
    let buf = String::from_utf8(buf).context("Invalid UTF-8")?;
    let message: lsp_server::Message = serde_json::from_str(&buf).context("invalid message")?;
    Ok(Some(message))
}

fn encode_lsp_message(message: lsp_server::Message) -> Vec<u8> {
    let mut result = Vec::new();
    message.write(&mut result).unwrap();
    result
}

fn bind_transport<T: AsyncRead + AsyncWrite>(io: T) -> Framed<T, LengthDelimitedCodec> {
    Framed::new(io, LengthDelimitedCodec::new())
}
