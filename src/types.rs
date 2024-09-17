use std::{collections::HashMap, env, path::PathBuf};

use resolve_path::PathResolveExt;
use serde::{Deserialize, Serialize};
use triomphe::Arc;

/// The configuration values.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Unix domain socket for the daemon.
    pub socket_path: PathBuf,
    /// The rust analyzer binary.
    pub rust_analyzer_bin: String,
    /// The name of the wakatime-cli binary to use.
    pub wakatime_cli: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let socket_path = env::var("RAD_SOCKET")
            .unwrap_or_else(|_| "~/.rad/ipc.sock".to_string())
            .try_resolve()
            .expect("Could not resolve IPC socket path")
            .to_path_buf();

        let rust_analyzer_bin = env::var("RAD_RA").unwrap_or_else(|_| "rust-analyzer".to_string());
        let wakatime_cli = env::var("RAD_WAKATIME").ok().map(|s| {
            if s.is_empty() {
                "wakatime-cli".into()
            } else {
                s
            }
        });

        Self {
            socket_path,
            rust_analyzer_bin,
            wakatime_cli,
        }
    }
}

/// The bootstrap message is the first message sent from the client process to the main process
/// which contains the information related to the environment at which the client is being
/// executed.
#[derive(Debug, Serialize, Deserialize)]
pub struct BootstrapMessage {
    /// The current working directory for the client process.
    pub cwd: PathBuf,
    /// The arguments passed to the binary.
    pub args: Vec<String>,
    /// All of the environment variables available to the client.
    pub envs: HashMap<String, String>,
}

impl Default for BootstrapMessage {
    /// Create the bootstrap message from the current env.
    fn default() -> Self {
        let cwd = env::current_dir().expect("Failed to get cwd.");
        let args = env::args().skip(1).collect();
        let envs = env::vars().collect();
        Self { cwd, args, envs }
    }
}

/// The unique identifier for a session all the clients with the same session key
/// get connected to the same LSP server.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    pub data: Arc<SessionKeyData>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKeyData {
    pub bin: String,
    pub args: Vec<String>,
    pub workspace: PathBuf,
}
