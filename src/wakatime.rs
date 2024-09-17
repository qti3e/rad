use std::borrow::Borrow;
use tokio::process::Command;

/// Implemented over any lsp message that can be reported to wakatime.
pub trait WakaTimeReport {
    fn maybe_report_wakatime(
        &self,
        maybe_cli: &Option<String>,
        user_agent: &str,
        project_root: &str,
    ) {
        if let Some(cli) = maybe_cli {
            self.report_wakatime(cli, user_agent, project_root);
        }
    }

    fn report_wakatime(&self, cli: &str, user_agent: &str, project_root: &str) {
        let mut cmd = Command::new(cli);
        cmd.args(["--plugin", user_agent]);
        cmd.args(["--alternate-project", project_root]);
        cmd.args(["--project-folder", project_root]);
        cmd.args(["--language", "rust"]);
        cmd.args(["--category", "coding"]);

        // expand the command based on the information we can extract from the lsp message.
        self.build_command(&mut cmd);

        tokio::spawn(async move {
            match cmd.status().await {
                Err(e) => tracing::error!(?e),
                Ok(exit) if !exit.success() => {
                    tracing::error!(
                        "wakatime-cli exited with error code: {}",
                        exit.code().map_or("<none>".into(), |c| c.to_string())
                    );
                }
                _ => {}
            };
        });
    }

    fn build_command(&self, cmd: &mut Command);
}

impl WakaTimeReport for lsp_types::DidOpenTextDocumentParams {
    fn build_command(&self, cmd: &mut Command) {
        let uri = self.text_document.uri.path();
        cmd.args(["--entity", uri]);
    }
}

impl WakaTimeReport for lsp_types::DidChangeTextDocumentParams {
    fn build_command(&self, cmd: &mut Command) {
        let uri = self.text_document.uri.path();
        cmd.args(["--entity", uri]);
        if let Some(line) = self
            .content_changes
            .iter()
            .find_map(|change| change.range.map(|x| x.start.line))
        {
            cmd.args(["--lineno", &line.to_string()]);
        }

        // TODO(qti3e): Once we have editor support.
        // "--line-additions", "123",
        // "--line-deletions", "456",

        cmd.arg("--write");
    }
}

impl WakaTimeReport for lsp_types::DidCloseTextDocumentParams {
    fn build_command(&self, cmd: &mut Command) {
        let uri = self.text_document.uri.path();
        cmd.args(["--entity", uri]);
    }
}
