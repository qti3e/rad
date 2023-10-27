use anyhow::Result;
use std::{collections::HashMap, env, path::PathBuf};

use resolve_path::PathResolveExt;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use triomphe::Arc;

use crate::utils::find_project_root;

/// The configuration values.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Unix domain socket for the daemon.
    pub socket_path: PathBuf,
    /// The rust analyzer binary.
    pub rust_analyzer_bin: String,
}

impl Default for Config {
    fn default() -> Self {
        let socket_path = env::var("RAD_SOCKET")
            .unwrap_or_else(|_| "~/.rad/ipc.sock".to_string())
            .try_resolve()
            .expect("Could not resolve IPC socket path")
            .to_path_buf();

        let rust_analyzer_bin = env::var("RAD_RA").unwrap_or_else(|_| "rust-analyzer".to_string());

        Self {
            socket_path,
            rust_analyzer_bin,
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

impl BootstrapMessage {
    /// Given the bootstrap message from a client, build the server identifier and the command that
    /// must be used to spawn the server.
    pub fn parse(self, default_ra: &str) -> Result<(SessionKey, Command)> {
        let workspace = find_project_root(&self.cwd)?;

        // Get the rust analyzer binary from the env variables that are set in the child
        // or default it to the one we have.
        let bin = self
            .envs
            .get("RAD_RA")
            .map(|s| s.as_str())
            .unwrap_or(default_ra)
            .to_string()
            .to_string();

        // Create the command by setting the binary name and the environment variables and
        // the cwd.
        let mut cmd = Command::new(&bin);
        cmd.args(self.args.iter());
        cmd.env_clear();
        cmd.envs(self.envs);
        cmd.current_dir(self.cwd); // TODO: Maybe `project_root`?

        Ok((
            SessionKey {
                data: Arc::new(SessionKeyData {
                    bin,
                    args: self.args,
                    workspace,
                }),
            },
            cmd,
        ))
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
