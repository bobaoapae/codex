//! CLI clients for the experimental app-server recovery methods.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::Subcommand;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadRecoveryCreateParams;
use codex_app_server_protocol::ThreadRecoveryPreviewParams;
use codex_utils_cli::CliConfigOverrides;
use serde_json::Value;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;

/// Manage an immutable recovery preview and its replacement lineage.
#[derive(Debug, Args)]
pub(crate) struct RecoveryCommand {
    #[command(subcommand)]
    pub(crate) subcommand: RecoverySubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RecoverySubcommand {
    /// Analyze a thread without changing its rollout.
    Preview(RecoveryPreviewCommand),

    /// Consume a preview token and create a replacement lineage.
    Create(RecoveryCreateCommand),
}

#[derive(Debug, Args)]
pub(crate) struct RecoveryPreviewCommand {
    /// Source thread UUID.
    #[arg(value_name = "THREAD_ID")]
    pub(crate) thread_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct RecoveryCreateCommand {
    /// Opaque token returned by `codex recovery preview`.
    #[arg(value_name = "TOKEN")]
    pub(crate) token: String,
}

pub(crate) fn run(command: RecoveryCommand, config_overrides: CliConfigOverrides) -> Result<()> {
    let (method, params) = match command.subcommand {
        RecoverySubcommand::Preview(RecoveryPreviewCommand { thread_id }) => (
            "thread/recovery/preview",
            serde_json::to_value(ThreadRecoveryPreviewParams { thread_id })?,
        ),
        RecoverySubcommand::Create(RecoveryCreateCommand { token }) => (
            "thread/recovery/create",
            serde_json::to_value(ThreadRecoveryCreateParams { token })?,
        ),
    };

    let mut client = RecoveryClient::spawn(&config_overrides.raw_overrides)?;
    let result = (|| {
        client.initialize()?;
        client.request(method, params)
    })();
    let shutdown_result = client.shutdown();
    let result = result?;
    shutdown_result?;

    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

struct RecoveryClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_request_id: i64,
}

impl RecoveryClient {
    fn spawn(config_overrides: &[String]) -> Result<Self> {
        let codex_bin = std::env::current_exe().context("failed to resolve codex executable")?;
        let codex_bin_display = codex_bin.display().to_string();
        let mut command = Command::new(codex_bin);
        command.arg("app-server").arg("--listen").arg("stdio://");
        for override_value in config_overrides {
            command.arg("--config").arg(override_value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start `{codex_bin_display} app-server`"))?;
        let stdin = child
            .stdin
            .take()
            .context("recovery app-server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("recovery app-server stdout unavailable")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_request_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        let params = serde_json::to_value(InitializeParams {
            client_info: ClientInfo {
                name: "codex-cli-recovery".to_string(),
                title: Some("Codex CLI recovery".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        })?;
        let _: Value = self.request("initialize", params)?;
        self.write_message(JSONRPCMessage::Notification(JSONRPCNotification {
            method: "initialized".to_string(),
            params: None,
        }))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let request_id = RequestId::Integer(self.next_request_id);
        self.next_request_id += 1;
        self.write_message(JSONRPCMessage::Request(JSONRPCRequest {
            id: request_id.clone(),
            method: method.to_string(),
            params: Some(params),
            trace: None,
        }))?;

        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("recovery app-server closed stdout before `{method}` completed");
            }
            let message: JSONRPCMessage = serde_json::from_str(line.trim()).with_context(|| {
                format!("recovery app-server emitted invalid JSON for `{method}`")
            })?;
            match message {
                JSONRPCMessage::Response(response) if response.id == request_id => {
                    return Ok(response.result);
                }
                JSONRPCMessage::Error(error) if error.id == request_id => {
                    bail!(
                        "{method} failed ({}): {}",
                        error.error.code,
                        error.error.message
                    );
                }
                JSONRPCMessage::Request(_) => {
                    bail!("recovery app-server requested unsupported interactive input");
                }
                JSONRPCMessage::Notification(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {
                    // Initialization may emit connection-scoped notifications. Ignore them until
                    // the response for this request arrives.
                }
            }
        }
    }

    fn write_message(&mut self, message: JSONRPCMessage) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("recovery app-server stdin is closed")?;
        serde_json::to_writer(&mut *stdin, &message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn shutdown(mut self) -> Result<()> {
        self.stdin.take();
        self.child
            .wait()
            .context("failed waiting for recovery app-server")?;
        Ok(())
    }
}

impl Drop for RecoveryClient {
    fn drop(&mut self) {
        // Closing stdin asks a private stdio app-server to exit. Do not kill a
        // caller-owned daemon or attempt any recovery mutation from Drop.
        self.stdin.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: RecoverySubcommand,
    }

    #[test]
    fn parses_preview_and_create_shapes() {
        let TestCli { command } =
            TestCli::try_parse_from(["codex", "preview", "01a05464-12ca-75c3-b7a8-856c95a3aaee"])
                .expect("preview should parse");
        assert!(matches!(
            command,
            RecoverySubcommand::Preview(RecoveryPreviewCommand { thread_id })
                if thread_id == "01a05464-12ca-75c3-b7a8-856c95a3aaee"
        ));

        let TestCli { command } = TestCli::try_parse_from(["codex", "create", "opaque-token"])
            .expect("create should parse");
        assert!(matches!(
            command,
            RecoverySubcommand::Create(RecoveryCreateCommand { token })
                if token == "opaque-token"
        ));
    }
}
