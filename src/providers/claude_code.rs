//! Claude subscription transport through the official Claude Code CLI.
//!
//! Claude Code owns OAuth and inference. Its built-in tools are disabled; the
//! only tools it receives are Nonaut's MCP tools, which execute through nono.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};

use super::{BASE_INSTRUCTIONS, Model, ProviderEvent, ReasoningOption, developer_instructions};
use crate::tools;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_EFFORT: &str = "default";
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const MCP_PREFIX: &str = "mcp__nonaut__";
const TOOL_NAMES: [&str; 7] = [
    "read_file",
    "list_directory",
    "write_file",
    "search_files",
    "edit_file",
    "delete_file",
    "run_command",
];

type Events = mpsc::UnboundedSender<(u64, ProviderEvent)>;

#[derive(Clone)]
pub(crate) struct ClaudeCode {
    executable: PathBuf,
    cwd: PathBuf,
    models: Vec<Model>,
    selected_model: Arc<RwLock<Model>>,
    selected_reasoning_effort: Arc<RwLock<String>>,
    session_id: Arc<Mutex<Option<String>>>,
    active: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ClaudeCode {
    pub async fn is_logged_in() -> bool {
        let executable = claude_executable();
        auth_status(&executable).await.unwrap_or(false)
    }

    pub async fn connect(cwd: &std::path::Path) -> Result<Self> {
        let executable = claude_executable();
        ensure_authenticated(&executable).await?;
        let models = claude_models();
        let selected_model = models[0].clone();
        Ok(Self {
            executable,
            cwd: cwd.to_path_buf(),
            models,
            selected_model: Arc::new(RwLock::new(selected_model)),
            selected_reasoning_effort: Arc::new(RwLock::new(DEFAULT_EFFORT.to_owned())),
            session_id: Arc::default(),
            active: Arc::default(),
        })
    }

    pub fn model(&self) -> Model {
        self.selected_model
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn select_model(&self, model_id: &str) -> Result<Model> {
        let model = self
            .models
            .iter()
            .find(|model| model.id == model_id)
            .cloned()
            .with_context(|| format!("Claude Code does not list model {model_id}"))?;
        *self
            .selected_model
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = model.clone();
        *self
            .selected_reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = DEFAULT_EFFORT.to_owned();
        Ok(model)
    }

    pub fn select_reasoning_effort(&self, effort: &str) -> Result<String> {
        if !self
            .model()
            .reasoning_efforts
            .iter()
            .any(|option| option.id == effort)
        {
            bail!("Claude Code does not support effort {effort}");
        }
        *self
            .selected_reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = effort.to_owned();
        Ok(effort.to_owned())
    }

    pub async fn new_session(&self) -> Result<()> {
        self.interrupt().await?;
        *self.session_id.lock().await = None;
        Ok(())
    }

    pub async fn stream(&self, input: String, request_id: u64, sender: Events) -> Result<()> {
        let (result_tx, result_rx) = oneshot::channel();
        let provider = self.clone();
        let handle = tokio::spawn(async move {
            let _ = result_tx.send(provider.run_turn(input, request_id, &sender).await);
        });
        let _guard = AbortOnDrop(handle.abort_handle());
        *self.active.lock().await = Some(handle);
        result_rx.await.unwrap_or(Ok(()))
    }

    pub async fn interrupt(&self) -> Result<()> {
        if let Some(handle) = self.active.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        Ok(())
    }

    async fn run_turn(&self, input: String, request_id: u64, sender: &Events) -> Result<()> {
        let executable = std::env::current_exe().context("could not locate Nonaut")?;
        let model = self
            .selected_model
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .id
            .clone();
        let effort = self
            .selected_reasoning_effort
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let existing_session = self.session_id.lock().await.clone();
        let mcp_config = json!({
            "mcpServers": {
                "nonaut": {
                    "type": "stdio",
                    "command": executable,
                    "args": ["__nonaut_mcp", self.cwd]
                }
            }
        });
        let system = format!(
            "{BASE_INSTRUCTIONS}\n\n{}\n\nWorkspace: {}",
            developer_instructions(&self.cwd),
            self.cwd.display()
        );
        let allowed_tools = TOOL_NAMES
            .iter()
            .map(|name| format!("{MCP_PREFIX}{name}"))
            .collect::<Vec<_>>()
            .join(",");

        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.cwd)
            // A shell API key otherwise takes precedence over a Claude
            // subscription login and can silently switch billing modes.
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL")
            .arg("--print")
            .arg("--verbose")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--model")
            .arg(model)
            .arg("--system-prompt")
            .arg(system)
            .arg("--mcp-config")
            .arg(mcp_config.to_string())
            .arg("--strict-mcp-config")
            .arg("--tools")
            .arg("")
            .arg("--allowedTools")
            .arg(allowed_tools)
            .arg("--permission-mode")
            .arg("dontAsk")
            .arg("--permission-prompts")
            .arg("none")
            .arg("--setting-sources")
            .arg("")
            .arg("--disable-slash-commands")
            .arg("--no-chrome");
        if effort != DEFAULT_EFFORT {
            command.arg("--effort").arg(effort);
        }
        if let Some(session) = existing_session {
            command.arg("--resume").arg(session);
        }
        command
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .context("could not launch Claude Code; install the `claude` CLI")?;
        let stdout = child
            .stdout
            .take()
            .context("Claude Code stdout unavailable")?;
        let mut stderr = child
            .stderr
            .take()
            .context("Claude Code stderr unavailable")?;
        let stderr_task = tokio::spawn(async move {
            let mut output = String::new();
            let _ = stderr.read_to_string(&mut output).await;
            output
        });
        let mut lines = BufReader::new(stdout).lines();
        let mut state = CliStream::new(self.cwd.clone());
        while let Some(line) = lines
            .next_line()
            .await
            .context("could not read Claude Code output")?
        {
            let event: Value = serde_json::from_str(&line)
                .with_context(|| format!("invalid Claude Code event: {line}"))?;
            state.apply(&event, request_id, sender);
        }
        let status = child
            .wait()
            .await
            .context("could not wait for Claude Code")?;
        let stderr = stderr_task.await.unwrap_or_default();
        if let Some(session) = state.session_id {
            *self.session_id.lock().await = Some(session);
        }
        if !status.success() {
            bail!(
                "Claude Code exited with {status}: {}",
                stderr.trim().trim_start_matches("Error: ")
            );
        }
        if let Some(error) = state.error {
            bail!("{error}");
        }
        if !state.completed {
            bail!("Claude Code ended without a result");
        }
        let _ = sender.send((request_id, ProviderEvent::Completed));
        Ok(())
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct CliStream {
    workspace: PathBuf,
    session_id: Option<String>,
    tool_names: HashMap<String, String>,
    completed: bool,
    error: Option<String>,
}

impl CliStream {
    fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            session_id: None,
            tool_names: HashMap::new(),
            completed: false,
            error: None,
        }
    }

    fn apply(&mut self, event: &Value, request_id: u64, sender: &Events) {
        if let Some(session) = event["session_id"].as_str() {
            self.session_id = Some(session.to_owned());
        }
        match event["type"].as_str().unwrap_or_default() {
            "stream_event" => {
                let delta = &event["event"]["delta"];
                if delta["type"] == "text_delta"
                    && let Some(text) = delta["text"].as_str()
                    && !text.is_empty()
                {
                    let _ = sender.send((request_id, ProviderEvent::Delta(text.to_owned())));
                }
            }
            "assistant" => {
                for block in message_content(event) {
                    if block["type"] == "tool_use"
                        && let (Some(id), Some(name)) =
                            (block["id"].as_str(), block["name"].as_str())
                    {
                        let name = display_tool_name(name);
                        self.tool_names.insert(id.to_owned(), name.clone());
                        let context =
                            tools::audit_context(id, &self.workspace, &name, &block["input"]);
                        let _ = sender.send((request_id, ProviderEvent::ToolStarted(context)));
                    }
                }
            }
            "user" => {
                for block in message_content(event) {
                    if block["type"] == "tool_result"
                        && let Some(id) = block["tool_use_id"].as_str()
                    {
                        let tool = self
                            .tool_names
                            .remove(id)
                            .unwrap_or_else(|| "tool".to_owned());
                        let success = block["is_error"] != true;
                        let output = content_text(&block["content"]);
                        let failure = (!success).then(|| tools::audit_failure_text(&output));
                        let _ = sender.send((
                            request_id,
                            ProviderEvent::ToolFinished {
                                call_id: id.to_owned(),
                                tool,
                                success,
                                output,
                                failure,
                            },
                        ));
                    }
                }
            }
            "result" => {
                let usage = &event["usage"];
                let tokens = [
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|field| usage[*field].as_u64())
                .sum();
                if tokens > 0 {
                    let _ = sender.send((request_id, ProviderEvent::Usage(tokens)));
                }
                if event["is_error"] == true {
                    self.error = Some(
                        event["result"]
                            .as_str()
                            .unwrap_or("Claude Code request failed")
                            .to_owned(),
                    );
                } else {
                    self.completed = true;
                }
            }
            _ => {}
        }
    }
}

fn message_content(event: &Value) -> &[Value] {
    event["message"]["content"]
        .as_array()
        .map_or(&[], Vec::as_slice)
}

fn display_tool_name(name: &str) -> String {
    name.strip_prefix(MCP_PREFIX).unwrap_or(name).to_owned()
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn claude_models() -> Vec<Model> {
    [
        ("opus", "Claude Opus"),
        ("sonnet", "Claude Sonnet"),
        ("haiku", "Claude Haiku"),
    ]
    .into_iter()
    .map(|(id, display_name)| Model {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        default_reasoning_effort: DEFAULT_EFFORT.to_owned(),
        reasoning_efforts: std::iter::once(DEFAULT_EFFORT)
            .chain(EFFORTS)
            .map(|effort| ReasoningOption {
                id: effort.to_owned(),
                description: if effort == DEFAULT_EFFORT {
                    "Model default".to_owned()
                } else {
                    format!("{effort} effort")
                },
            })
            .collect(),
    })
    .collect()
}

fn claude_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("CLAUDE_BIN").filter(|path| !path.is_empty()) {
        return path.into();
    }
    if let Some(base) = BaseDirs::new() {
        let installed = base.home_dir().join(".local/bin/claude");
        if installed.is_file() {
            return installed;
        }
    }
    PathBuf::from("claude")
}

async fn auth_status(executable: &std::path::Path) -> Result<bool> {
    let output = Command::new(executable)
        .args(["auth", "status", "--json"])
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("could not run `claude auth status`; install the Claude Code CLI")?;
    if !output.status.success() {
        return Ok(false);
    }
    let status: Value = serde_json::from_slice(&output.stdout)
        .context("Claude Code returned an invalid authentication status")?;
    Ok(is_subscription_status(&status))
}

fn is_subscription_status(status: &Value) -> bool {
    let method = status["authMethod"].as_str().unwrap_or_default();
    status["loggedIn"] == true
        && (status["subscriptionType"].is_string() || matches!(method, "claude.ai" | "oauth_token"))
}

async fn ensure_authenticated(executable: &std::path::Path) -> Result<()> {
    if auth_status(executable).await? {
        return Ok(());
    }
    let child = Command::new(executable)
        .args(["auth", "login", "--claudeai"])
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not start Claude subscription sign-in")?;
    let output = tokio::time::timeout(LOGIN_TIMEOUT, child.wait_with_output())
        .await
        .context("Claude sign-in timed out after 5 minutes")?
        .context("could not wait for Claude sign-in")?;
    if !output.status.success() || !auth_status(executable).await? {
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Claude sign-in did not complete. Run `claude auth login --claudeai` and try again. {}",
            detail.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_cli_stream_events_to_shared_events() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut stream = CliStream::new(PathBuf::from("/workspace"));
        for event in [
            json!({ "type": "system", "session_id": "s1" }),
            json!({ "type": "stream_event", "event": { "delta": { "type": "text_delta", "text": "hello" } } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "id": "t1", "name": "mcp__nonaut__read_file" }] } }),
            json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "text", "text": "ok" }] }] } }),
            json!({ "type": "result", "is_error": false, "usage": { "input_tokens": 10, "output_tokens": 5 } }),
        ] {
            stream.apply(&event, 7, &sender);
        }
        assert_eq!(stream.session_id.as_deref(), Some("s1"));
        assert!(stream.completed);
        assert!(matches!(
            receiver.try_recv().unwrap().1,
            ProviderEvent::Delta(_)
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().1,
            ProviderEvent::ToolStarted(_)
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().1,
            ProviderEvent::ToolFinished { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().1,
            ProviderEvent::Usage(15)
        ));
    }

    #[test]
    fn exposes_subscription_model_aliases() {
        let models = claude_models();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["opus", "sonnet", "haiku"]
        );
        assert!(
            models[0]
                .reasoning_efforts
                .iter()
                .any(|effort| effort.id == "max")
        );
    }

    #[test]
    fn distinguishes_subscription_login_from_api_credentials() {
        assert!(is_subscription_status(&json!({
            "loggedIn": true,
            "authMethod": "claude.ai",
            "subscriptionType": "max"
        })));
        assert!(!is_subscription_status(&json!({
            "loggedIn": true,
            "authMethod": "api_key"
        })));
        assert!(!is_subscription_status(&json!({
            "loggedIn": false,
            "authMethod": "none"
        })));
    }
}
