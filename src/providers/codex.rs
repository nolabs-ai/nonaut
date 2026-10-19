use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, RwLock as StdRwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
    task::AbortHandle,
};

use crate::tools;

use super::{BASE_INSTRUCTIONS, Model, ProviderEvent, ReasoningOption, developer_instructions};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

type RpcReply = std::result::Result<Value, String>;
type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcReply>>>>;

#[derive(Clone)]
pub(crate) struct Codex {
    rpc: Rpc,
    cwd: PathBuf,
    thread_id: Arc<RwLock<String>>,
    active_turn_id: Arc<RwLock<Option<String>>>,
    models: Vec<Model>,
    selected_model: Arc<StdRwLock<Model>>,
    selected_reasoning_effort: Arc<StdRwLock<String>>,
}

#[derive(Clone)]
struct Rpc {
    outbound: mpsc::UnboundedSender<String>,
    pending: PendingRequests,
    notifications: broadcast::Sender<Value>,
    next_id: Arc<AtomicU64>,
    active_tools: ActiveTools,
}

type ActiveTools = Arc<Mutex<HashMap<String, (String, AbortHandle)>>>;

impl Codex {
    pub async fn connect(cwd: &Path) -> Result<Self> {
        let rpc = Rpc::spawn(cwd).await?;
        rpc.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "nonaut",
                    "title": "Nonaut",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": false
                }
            }),
        )
        .await
        .context("Codex app-server initialization failed")?;
        rpc.notify("initialized", json!({}))?;

        let mut authenticated = is_authenticated(&rpc).await?;
        if !authenticated {
            login(&rpc).await?;
            authenticated = is_authenticated(&rpc).await?;
        }
        if !authenticated {
            bail!("Codex login completed without an account");
        }

        let (models, default_model) = list_models(&rpc).await?;
        let model = default_model
            .or_else(|| models.first().cloned())
            .context("Codex app-server returned no visible models")?;

        let thread_id = start_thread(&rpc, cwd, &model.id).await?;
        let reasoning_effort = model.default_reasoning_effort.clone();

        Ok(Self {
            rpc,
            cwd: cwd.to_path_buf(),
            thread_id: Arc::new(RwLock::new(thread_id)),
            active_turn_id: Arc::new(RwLock::new(None)),
            models,
            selected_model: Arc::new(StdRwLock::new(model)),
            selected_reasoning_effort: Arc::new(StdRwLock::new(reasoning_effort)),
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
            .with_context(|| format!("Codex did not advertise model {model_id}"))?;
        *self
            .selected_model
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = model.clone();
        *self
            .selected_reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            model.default_reasoning_effort.clone();
        Ok(model)
    }

    pub fn reasoning_effort(&self) -> String {
        self.selected_reasoning_effort
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn select_reasoning_effort(&self, effort: &str) -> Result<String> {
        let model = self.model();
        let selected = model
            .reasoning_efforts
            .iter()
            .find(|option| option.id == effort)
            .with_context(|| {
                format!(
                    "{} does not advertise reasoning effort {effort}",
                    model.display_name
                )
            })?
            .id
            .clone();
        *self
            .selected_reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = selected.clone();
        Ok(selected)
    }

    pub async fn new_session(&self) -> Result<()> {
        self.interrupt().await?;
        let thread_id = start_thread(&self.rpc, &self.cwd, &self.model().id).await?;
        *self.thread_id.write().await = thread_id;
        Ok(())
    }

    pub async fn stream(
        &self,
        input: String,
        request_id: u64,
        sender: mpsc::UnboundedSender<(u64, ProviderEvent)>,
    ) -> Result<()> {
        let mut notifications = self.rpc.subscribe();
        let model = self.model();
        let effort = self.reasoning_effort();
        let thread_id = self.thread_id.read().await.clone();
        let response = self
            .rpc
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "model": model.id,
                    "effort": effort,
                    "input": [{ "type": "text", "text": input }]
                }),
            )
            .await
            .context("could not start the Codex turn")?;
        let turn_id = string_field(&response["turn"], "id")?;
        *self.active_turn_id.write().await = Some(turn_id.clone());

        loop {
            let notification = notifications
                .recv()
                .await
                .context("Codex app-server event stream closed")?;
            if notification["method"] == "item/agentMessage/delta"
                && notification["params"]["threadId"] == thread_id
                && notification["params"]["turnId"] == turn_id
            {
                if let Some(delta) = notification["params"]["delta"].as_str() {
                    let _ = sender.send((request_id, ProviderEvent::Delta(delta.to_owned())));
                }
            } else if notification["method"] == "thread/tokenUsage/updated"
                && notification["params"]["threadId"] == thread_id
            {
                if let Some(tokens) =
                    notification["params"]["tokenUsage"]["total"]["totalTokens"].as_u64()
                {
                    let _ = sender.send((request_id, ProviderEvent::Usage(tokens)));
                }
            } else if notification["method"] == "nonaut/toolStarted"
                && notification["params"]["threadId"] == thread_id
                && notification["params"]["turnId"] == turn_id
            {
                if let Some(tool) = notification["params"]["tool"].as_str() {
                    let _ = sender.send((request_id, ProviderEvent::ToolStarted(tool.to_owned())));
                }
            } else if notification["method"] == "nonaut/toolFinished"
                && notification["params"]["threadId"] == thread_id
                && notification["params"]["turnId"] == turn_id
            {
                if let Some(tool) = notification["params"]["tool"].as_str() {
                    let _ = sender.send((
                        request_id,
                        ProviderEvent::ToolFinished {
                            tool: tool.to_owned(),
                            success: notification["params"]["success"] == true,
                            output: notification["params"]["output"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                        },
                    ));
                }
            } else if notification["method"] == "turn/completed"
                && notification["params"]["threadId"] == thread_id
                && notification["params"]["turn"]["id"] == turn_id
            {
                *self.active_turn_id.write().await = None;
                let status = notification["params"]["turn"]["status"]
                    .as_str()
                    .unwrap_or("failed");
                if status == "completed" {
                    let _ = sender.send((request_id, ProviderEvent::Completed));
                    return Ok(());
                }
                if status == "interrupted" {
                    let _ = sender.send((request_id, ProviderEvent::Interrupted));
                    return Ok(());
                }
                let message = notification["params"]["turn"]["error"]["message"]
                    .as_str()
                    .unwrap_or(status);
                bail!("Codex turn {status}: {message}");
            }
        }
    }

    pub async fn interrupt(&self) -> Result<()> {
        let Some(turn_id) = self.active_turn_id.read().await.clone() else {
            return Ok(());
        };
        let thread_id = self.thread_id.read().await.clone();
        self.rpc.cancel_tools(&turn_id).await;
        self.rpc
            .request(
                "turn/interrupt",
                json!({ "threadId": thread_id, "turnId": turn_id }),
            )
            .await?;
        *self.active_turn_id.write().await = None;
        Ok(())
    }
}

impl Rpc {
    async fn spawn(cwd: &Path) -> Result<Self> {
        let mut child = Command::new("codex")
            .args([
                "app-server",
                "--disable",
                "shell_tool",
                "--disable",
                "unified_exec",
                "--disable",
                "view_image",
                "--disable",
                "multi_agent",
                "--disable",
                "apps",
                "--disable",
                "plugins",
                "--disable",
                "browser_use",
                "--disable",
                "computer_use",
                "--disable",
                "image_generation",
                "--listen",
                "stdio://",
            ])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("could not launch `codex app-server`")?;
        let stdin = child
            .stdin
            .take()
            .context("app-server stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("app-server stdout was unavailable")?;

        let (outbound, mut outbound_rx) = mpsc::unbounded_channel::<String>();
        let writer = outbound.downgrade();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = outbound_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let (notifications, _) = broadcast::channel(1024);
        let reader_notifications = notifications.clone();
        let active_tools: ActiveTools = Arc::new(Mutex::new(HashMap::new()));
        let reader_active_tools = active_tools.clone();
        let tool_workspace = cwd.to_path_buf();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = message["id"].as_u64()
                    && message.get("method").is_none()
                {
                    if let Some(sender) = reader_pending.lock().await.remove(&id) {
                        let result = match message.get("result") {
                            Some(result) => Ok(result.clone()),
                            None => Err(message["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown app-server error")
                                .to_owned()),
                        };
                        let _ = sender.send(result);
                    }
                } else if message.get("id").is_some() && message["method"] == "item/tool/call" {
                    start_tool_call(
                        message,
                        tool_workspace.clone(),
                        writer.clone(),
                        reader_notifications.clone(),
                        reader_active_tools.clone(),
                    )
                    .await;
                } else if let Some(id) = message.get("id") {
                    let denial = json!({
                        "id": id.clone(),
                        "error": {
                            "code": -32601,
                            "message": "Nonaut only accepts its declared dynamic tools"
                        }
                    });
                    if let Some(writer) = writer.upgrade() {
                        let _ = writer.send(denial.to_string());
                    }
                } else {
                    let _ = reader_notifications.send(message);
                }
            }
            let _ = reader_notifications.send(json!({ "method": "nonaut/processExited" }));
        });
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        Ok(Self {
            outbound,
            pending,
            notifications,
            next_id: Arc::new(AtomicU64::new(1)),
            active_tools,
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let message = json!({ "id": id, "method": method, "params": params });
        if self.outbound.send(message.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            bail!("Codex app-server is not running");
        }
        tokio::time::timeout(RPC_TIMEOUT, receiver)
            .await
            .with_context(|| format!("Codex app-server timed out handling {method}"))?
            .context("Codex app-server dropped the request")?
            .map_err(|message| anyhow!(message))
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({ "method": method, "params": params });
        self.outbound
            .send(message.to_string())
            .map_err(|_| anyhow!("Codex app-server is not running"))
    }

    fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    async fn cancel_tools(&self, turn_id: &str) {
        let mut active = self.active_tools.lock().await;
        for (_, (_, handle)) in active
            .iter()
            .filter(|(_, (active_turn, _))| active_turn == turn_id)
        {
            handle.abort();
        }
        active.retain(|_, (active_turn, _)| active_turn != turn_id);
    }
}

async fn start_tool_call(
    message: Value,
    workspace: PathBuf,
    writer: mpsc::WeakUnboundedSender<String>,
    notifications: broadcast::Sender<Value>,
    active_tools: ActiveTools,
) {
    let id = message["id"].clone();
    let params = message["params"].clone();
    let Some(tool) = params["tool"].as_str().map(str::to_owned) else {
        send_tool_error(writer, id, "tool request had no tool name");
        return;
    };
    let Some(call_id) = params["callId"].as_str().map(str::to_owned) else {
        send_tool_error(writer, id, "tool request had no call id");
        return;
    };
    let Some(turn_id) = params["turnId"].as_str().map(str::to_owned) else {
        send_tool_error(writer, id, "tool request had no turn id");
        return;
    };
    let thread_id = params["threadId"].as_str().unwrap_or_default().to_owned();
    let arguments = params["arguments"].clone();
    let _ = notifications.send(json!({
        "method": "nonaut/toolStarted",
        "params": { "threadId": thread_id, "turnId": turn_id, "tool": tool }
    }));

    let (start_tx, start_rx) = oneshot::channel();
    let task_call_id = call_id.clone();
    let task_turn_id = turn_id.clone();
    let task_tool = tool.clone();
    let task_active = active_tools.clone();
    let handle = tokio::spawn(async move {
        let _ = start_rx.await;
        let result = tools::execute(&workspace, &task_tool, &arguments).await;
        let (success, output) = match result {
            Ok(output) => (true, output),
            Err(error) => (false, format!("{error:#}")),
        };
        let event_output = output.clone();
        if let Some(writer) = writer.upgrade() {
            let response = json!({
                "id": id,
                "result": {
                    "contentItems": [{ "type": "inputText", "text": output }],
                    "success": success
                }
            });
            let _ = writer.send(response.to_string());
        }
        let _ = notifications.send(json!({
            "method": "nonaut/toolFinished",
            "params": {
                "threadId": thread_id,
                "turnId": task_turn_id,
                "tool": task_tool,
                "success": success,
                "output": event_output
            }
        }));
        task_active.lock().await.remove(&task_call_id);
    });
    active_tools
        .lock()
        .await
        .insert(call_id, (turn_id, handle.abort_handle()));
    let _ = start_tx.send(());
}

fn send_tool_error(writer: mpsc::WeakUnboundedSender<String>, id: Value, message: &str) {
    if let Some(writer) = writer.upgrade() {
        let response = json!({
            "id": id,
            "result": {
                "contentItems": [{ "type": "inputText", "text": message }],
                "success": false
            }
        });
        let _ = writer.send(response.to_string());
    }
}

async fn is_authenticated(rpc: &Rpc) -> Result<bool> {
    let response = rpc
        .request("account/read", json!({ "refreshToken": false }))
        .await?;
    Ok(!response["account"].is_null())
}

async fn list_models(rpc: &Rpc) -> Result<(Vec<Model>, Option<Model>)> {
    let mut cursor: Option<String> = None;
    let mut models = Vec::new();
    let mut default_model = None;
    loop {
        let response = rpc
            .request(
                "model/list",
                json!({ "cursor": cursor, "limit": 100, "includeHidden": false }),
            )
            .await?;
        let data = response["data"]
            .as_array()
            .context("Codex app-server returned an invalid model catalog")?;
        for value in data {
            if value["hidden"] == true {
                continue;
            }
            let model = Model {
                id: string_field(value, "model")?,
                display_name: string_field(value, "displayName")?,
                default_reasoning_effort: string_field(value, "defaultReasoningEffort")?,
                reasoning_efforts: value["supportedReasoningEfforts"]
                    .as_array()
                    .context("Codex app-server returned invalid reasoning options")?
                    .iter()
                    .map(|option| {
                        Ok(ReasoningOption {
                            id: string_field(option, "reasoningEffort")?,
                            description: string_field(option, "description")?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            if value["isDefault"] == true {
                default_model = Some(model.clone());
            }
            models.push(model);
        }
        cursor = response["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok((models, default_model))
}

async fn start_thread(rpc: &Rpc, cwd: &Path, model: &str) -> Result<String> {
    let thread = rpc
        .request(
            "thread/start",
            json!({
                "model": model,
                "cwd": cwd,
                "ephemeral": true,
                // Codex native execution is disabled above. Dynamic tool calls are
                // intentionally unrestricted here so nono is the sole policy boundary.
                "sandbox": "danger-full-access",
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "dynamicTools": tools::specs(),
                "baseInstructions": BASE_INSTRUCTIONS,
                "developerInstructions": developer_instructions(cwd)
            }),
        )
        .await
        .context("could not start a Codex thread")?;
    string_field(&thread["thread"], "id")
}

async fn login(rpc: &Rpc) -> Result<()> {
    let mut notifications = rpc.subscribe();
    let response = rpc
        .request(
            "account/login/start",
            json!({
                "type": "chatgpt",
                "appBrand": "codex",
                "codexStreamlinedLogin": true,
                "useHostedLoginSuccessPage": true
            }),
        )
        .await?;
    let login_id = string_field(&response, "loginId")?;
    let auth_url = string_field(&response, "authUrl")?;
    webbrowser::open(&auth_url).context("could not open the ChatGPT login page")?;

    tokio::time::timeout(LOGIN_TIMEOUT, async {
        loop {
            let notification = notifications.recv().await?;
            if notification["method"] == "account/login/completed"
                && notification["params"]["loginId"] == login_id
            {
                if notification["params"]["success"] == true {
                    return Ok(());
                }
                let error = notification["params"]["error"]
                    .as_str()
                    .unwrap_or("ChatGPT login failed");
                bail!(error.to_owned());
            }
        }
    })
    .await
    .context("ChatGPT login timed out")?
}

fn string_field(value: &Value, name: &str) -> Result<String> {
    value[name]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("Codex app-server response has no {name}"))
}
