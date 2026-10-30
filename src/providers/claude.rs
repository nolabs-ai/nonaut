//! Claude via the Anthropic Messages API. Nonaut owns the agent loop: stream a
//! response, run requested tools through `tools::execute` (nono), send the
//! results back, repeat until Claude ends the turn.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock as StdRwLock},
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};

use crate::tools;

use super::{
    COMPACTION_PROMPT, Model, PromptFrame, ProviderEvent, ReasoningOption, ToolLoopGuard,
    compacted_user_message, history_needs_compaction,
};

const MAX_OUTPUT_TOKENS: u64 = 64_000;
/// Omits `output_config.effort` so the model's own default applies.
const DEFAULT_EFFORT: &str = "default";
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const MODEL_CACHE_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

type Events = mpsc::UnboundedSender<(u64, ProviderEvent)>;

#[derive(Clone, Deserialize, Serialize)]
struct ModelInfo {
    model: Model,
    max_tokens: u64,
    adaptive_thinking: bool,
}

#[derive(Clone)]
pub(crate) struct ClaudeApi {
    client: Client,
    base_url: String,
    cwd: PathBuf,
    catalog: Vec<ModelInfo>,
    models: Vec<Model>,
    selected_model: Arc<StdRwLock<Model>>,
    selected_reasoning_effort: Arc<StdRwLock<String>>,
    history: Arc<Mutex<Vec<Value>>>,
    active: Arc<Mutex<Option<JoinHandle<()>>>>,
    frame: Arc<PromptFrame>,
}

impl ClaudeApi {
    pub async fn connect(cwd: &Path) -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .context("ANTHROPIC_API_KEY is not set; export it and restart nonaut")?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned())
            .trim_end_matches('/')
            .to_owned();
        let cache_key = super::cache::scoped_key("claude", &format!("{base_url}\0{}", key.trim()));
        let mut key = HeaderValue::from_str(key.trim()).context("invalid ANTHROPIC_API_KEY")?;
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", key);
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .context("could not build the HTTP client")?;

        let catalog = match super::cache::load(&cache_key, MODEL_CACHE_AGE) {
            Some(catalog) => catalog,
            None => {
                let catalog = fetch_catalog(&client, &base_url).await?;
                if !catalog.is_empty() {
                    let _ = super::cache::store(&cache_key, &catalog);
                }
                catalog
            }
        };
        let default = catalog
            .iter()
            .find(|info| info.model.id.contains("opus"))
            .or_else(|| catalog.first())
            .context("the Claude API returned no active models")?
            .model
            .clone();

        let frame = Arc::new(PromptFrame::new(cwd));
        Ok(Self {
            client,
            base_url,
            cwd: cwd.to_path_buf(),
            models: catalog.iter().map(|info| info.model.clone()).collect(),
            catalog,
            selected_model: Arc::new(StdRwLock::new(default)),
            selected_reasoning_effort: Arc::new(StdRwLock::new(DEFAULT_EFFORT.to_owned())),
            history: Arc::default(),
            active: Arc::default(),
            frame,
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
            .with_context(|| format!("the Claude API did not list model {model_id}"))?;
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
        let model = self.model();
        if !model
            .reasoning_efforts
            .iter()
            .any(|option| option.id == effort)
        {
            bail!("{} does not support effort {effort}", model.display_name);
        }
        *self
            .selected_reasoning_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = effort.to_owned();
        Ok(effort.to_owned())
    }

    pub async fn new_session(&self) -> Result<()> {
        self.interrupt().await?;
        self.history.lock().await.clear();
        Ok(())
    }

    pub async fn stream(&self, input: String, request_id: u64, sender: Events) -> Result<()> {
        // Run the turn in its own task so `interrupt` can stop it and wait.
        let (result_tx, result_rx) = oneshot::channel();
        let provider = self.clone();
        let handle = tokio::spawn(async move {
            let _ = result_tx.send(provider.run_turn(input, request_id, &sender).await);
        });
        let _guard = AbortOnDrop(handle.abort_handle());
        *self.active.lock().await = Some(handle);
        let result = result_rx.await;
        // A dropped sender means `interrupt` aborted the turn and reports it.
        result.unwrap_or(Ok(()))
    }

    pub async fn interrupt(&self) -> Result<()> {
        if let Some(handle) = self.active.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        Ok(())
    }

    async fn run_turn(&self, input: String, request_id: u64, sender: &Events) -> Result<()> {
        push_user_text(&mut *self.history.lock().await, input);
        let mut recent_calls = ToolLoopGuard::default();
        let id = self.model().id;
        let info = self
            .catalog
            .iter()
            .find(|info| info.model.id == id)
            .context("selected model is not in the catalog")?;
        let effort = self
            .selected_reasoning_effort
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        loop {
            self.compact_history(info).await?;
            let mut messages = self.history.lock().await.clone();
            // Cache the prefix; marked on a copy so history never accumulates
            // more breakpoints than the API allows.
            if let Some(block) = messages
                .last_mut()
                .and_then(|message| message["content"].as_array_mut())
                .and_then(|content| content.last_mut())
            {
                block["cache_control"] = json!({ "type": "ephemeral" });
            }
            let mut body = json!({
                "model": info.model.id,
                "max_tokens": info.max_tokens,
                "stream": true,
                "system": [{ "type": "text", "text": self.frame.system, "cache_control": { "type": "ephemeral" } }],
                "tools": self.frame.anthropic_tools,
                "messages": messages,
            });
            if info.adaptive_thinking {
                body["thinking"] = json!({ "type": "adaptive" });
            }
            if effort != DEFAULT_EFFORT {
                body["output_config"] = json!({ "effort": effort });
            }

            let response = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .context("could not reach the Claude API")?;
            if !response.status().is_success() {
                // api_json always fails for a non-success status.
                return api_json(response).await.map(drop);
            }
            let turn = read_stream(response, request_id, sender).await?;
            let _ = sender.send((
                request_id,
                ProviderEvent::Usage(turn.input_tokens + turn.output_tokens),
            ));

            // The API rejects empty text blocks on replay.
            let content = turn
                .blocks
                .into_iter()
                .filter(|block| {
                    block.is_object() && !(block["type"] == "text" && block["text"] == "")
                })
                .collect::<Vec<_>>();
            let calls = content
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .cloned()
                .collect::<Vec<_>>();
            if !content.is_empty() {
                self.history
                    .lock()
                    .await
                    .push(json!({ "role": "assistant", "content": content }));
            }
            match turn.stop_reason.as_str() {
                "tool_use" if !calls.is_empty() => {
                    let mut results = Vec::new();
                    for call in &calls {
                        results.push(
                            self.run_tool(call, request_id, sender, &mut recent_calls)
                                .await,
                        );
                    }
                    self.history
                        .lock()
                        .await
                        .push(json!({ "role": "user", "content": results }));
                }
                "pause_turn" => {}
                "end_turn" | "stop_sequence" => {
                    let _ = sender.send((request_id, ProviderEvent::Completed));
                    return Ok(());
                }
                reason => bail!("Claude stopped: {reason}"),
            }
        }
    }

    async fn compact_history(&self, info: &ModelInfo) -> Result<()> {
        let history = self.history.lock().await.clone();
        if !history_needs_compaction(&history) {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({
                "model": info.model.id,
                "max_tokens": 4096,
                "system": COMPACTION_PROMPT,
                "messages": [{
                    "role": "user",
                    "content": serde_json::to_string(&history).unwrap_or_default()
                }],
            }))
            .send()
            .await
            .context("could not compact Claude history")?;
        let value = api_json(response).await?;
        let summary = value["content"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str())
            .filter(|text| !text.trim().is_empty())
            .context("Claude returned an empty compaction summary")?;
        *self.history.lock().await = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": compacted_user_message(summary)}]
        })];
        Ok(())
    }

    async fn run_tool(
        &self,
        call: &Value,
        request_id: u64,
        sender: &Events,
        recent_calls: &mut ToolLoopGuard,
    ) -> Value {
        let name = call["name"].as_str().unwrap_or_default().to_owned();
        let call_id = call["id"].as_str().unwrap_or(&name).to_owned();
        let context = tools::audit_context(&call_id, &self.cwd, &name, &call["input"]);
        let _ = sender.send((request_id, ProviderEvent::ToolStarted(context)));
        let progress = super::tool_progress_reporter(request_id, call_id.clone(), sender.clone());
        let (success, output, failure) = match recent_calls
            .execute(&self.cwd, &name, &call["input"], Some(progress))
            .await
        {
            Ok(output) => (true, output, None),
            Err(error) => {
                let failure = tools::audit_failure(&error);
                (false, format!("{error:#}"), Some(failure))
            }
        };
        let _ = sender.send((
            request_id,
            ProviderEvent::ToolFinished {
                call_id,
                tool: name,
                success,
                output: output.clone(),
                failure,
            },
        ));
        tool_result(&call["id"], &output, !success)
    }
}

async fn fetch_catalog(client: &Client, base_url: &str) -> Result<Vec<ModelInfo>> {
    let response = client
        .get(format!("{base_url}/v1/models?limit=1000"))
        .send()
        .await
        .context("could not reach the Claude API")?;
    Ok(api_json(response).await?["data"]
        .as_array()
        .context("the Claude API returned an invalid model list")?
        .iter()
        .filter_map(parse_model)
        .collect())
}

/// Stops the turn if the caller drops `stream` without calling `interrupt`.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn api_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    let text = response
        .text()
        .await
        .context("could not read the Claude API response")?;
    let value: Value = serde_json::from_str(&text).unwrap_or_default();
    if !status.is_success() {
        let message = value["error"]["message"].as_str().unwrap_or(text.trim());
        bail!("Claude API error {status}: {message}");
    }
    Ok(value)
}

fn parse_model(value: &Value) -> Option<ModelInfo> {
    if value["lifecycle"]
        .as_str()
        .is_some_and(|state| state != "active")
    {
        return None;
    }
    let id = value["id"].as_str()?.to_owned();
    let capabilities = &value["capabilities"];
    let reasoning_efforts = std::iter::once(DEFAULT_EFFORT)
        .chain(
            EFFORTS
                .into_iter()
                .filter(|level| capabilities["effort"][*level]["supported"] == true),
        )
        .map(|level| ReasoningOption {
            id: level.to_owned(),
            description: if level == DEFAULT_EFFORT {
                "Model default".to_owned()
            } else {
                format!("{level} effort")
            },
        })
        .collect();
    Some(ModelInfo {
        max_tokens: value["max_tokens"]
            .as_u64()
            .filter(|tokens| *tokens > 0)
            .map_or(32_000, |tokens| tokens.min(MAX_OUTPUT_TOKENS)),
        adaptive_thinking: capabilities["thinking"]["types"]["adaptive"]["supported"] == true,
        model: Model {
            display_name: value["display_name"].as_str().unwrap_or(&id).to_owned(),
            id,
            default_reasoning_effort: DEFAULT_EFFORT.to_owned(),
            reasoning_efforts,
        },
    })
}

/// Appends user text, first answering any tool calls an interrupted turn left
/// open: the API requires every `tool_use` to get a `tool_result`.
fn push_user_text(history: &mut Vec<Value>, text: String) {
    let open_calls = history
        .last()
        .filter(|last| last["role"] == "assistant")
        .and_then(|last| last["content"].as_array())
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| tool_result(&block["id"], "Interrupted by the user.", true))
        .collect::<Vec<_>>();
    if !open_calls.is_empty() {
        history.push(json!({ "role": "user", "content": open_calls }));
    }
    let block = json!({ "type": "text", "text": text });
    match history.last_mut() {
        Some(last) if last["role"] == "user" => {
            if let Some(content) = last["content"].as_array_mut() {
                content.push(block);
            }
        }
        _ => history.push(json!({ "role": "user", "content": [block] })),
    }
}

fn tool_result(id: &Value, output: &str, is_error: bool) -> Value {
    let text = if output.trim().is_empty() {
        "(no output)"
    } else {
        output
    };
    json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": [{ "type": "text", "text": text }],
        "is_error": is_error,
    })
}

#[derive(Default)]
struct Turn {
    blocks: Vec<Value>,
    partial_json: Vec<String>,
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
    done: bool,
}

async fn read_stream(
    response: reqwest::Response,
    request_id: u64,
    sender: &Events,
) -> Result<Turn> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut turn = Turn::default();
    while let Some(chunk) = stream.next().await {
        buffer.extend(
            chunk
                .context("the Claude stream was interrupted")?
                .iter()
                .filter(|b| **b != b'\r'),
        );
        while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
            let frame = buffer.drain(..end + 2).collect::<Vec<_>>();
            for data in String::from_utf8_lossy(&frame)
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
            {
                let event: Value =
                    serde_json::from_str(data.trim()).context("invalid Claude stream event")?;
                if let Some(text) = turn.apply(&event)? {
                    let _ = sender.send((request_id, ProviderEvent::Delta(text)));
                }
                if turn.done {
                    return Ok(turn);
                }
            }
        }
    }
    bail!("the Claude stream ended before the message finished")
}

impl Turn {
    /// Rebuilds the assistant message from stream events so it can be replayed
    /// verbatim (thinking blocks must keep their signatures).
    fn apply(&mut self, event: &Value) -> Result<Option<String>> {
        let index = event["index"].as_u64().unwrap_or_default() as usize;
        match event["type"].as_str().unwrap_or_default() {
            "message_start" | "message_delta" => {
                let usage = if event["type"] == "message_start" {
                    &event["message"]["usage"]
                } else {
                    &event["usage"]
                };
                let input = [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .filter_map(|field| usage[*field].as_u64())
                .sum::<u64>();
                if input > 0 {
                    self.input_tokens = input;
                }
                if let Some(output) = usage["output_tokens"].as_u64() {
                    self.output_tokens = output;
                }
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = reason.to_owned();
                }
            }
            "content_block_start" => {
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                    self.partial_json.resize(index + 1, String::new());
                }
                self.blocks[index] = event["content_block"].clone();
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                let block = self
                    .blocks
                    .get_mut(index)
                    .context("delta for unknown block")?;
                let (field, text) = match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => ("text", &delta["text"]),
                    "thinking_delta" => ("thinking", &delta["thinking"]),
                    "signature_delta" => {
                        block["signature"] = delta["signature"].clone();
                        return Ok(None);
                    }
                    "input_json_delta" => {
                        self.partial_json[index]
                            .push_str(delta["partial_json"].as_str().unwrap_or_default());
                        return Ok(None);
                    }
                    _ => return Ok(None),
                };
                let text = text.as_str().unwrap_or_default();
                block[field] = json!(format!(
                    "{}{text}",
                    block[field].as_str().unwrap_or_default()
                ));
                if field == "text" && !text.is_empty() {
                    return Ok(Some(text.to_owned()));
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.blocks.get_mut(index)
                    && block["type"] == "tool_use"
                {
                    // Truncated JSON (max_tokens) falls back to {} to stay replayable.
                    block["input"] = serde_json::from_str(&self.partial_json[index])
                        .unwrap_or_else(|_| json!({}));
                }
            }
            "message_stop" => self.done = true,
            "error" => bail!(
                "Claude stream error: {}",
                event["error"]["message"].as_str().unwrap_or("unknown")
            ),
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_capabilities() {
        let info = parse_model(&json!({
            "id": "claude-sonnet-5-5",
            "lifecycle": "active",
            "max_tokens": 128000,
            "capabilities": {
                "effort": { "low": { "supported": true }, "max": { "supported": true } },
                "thinking": { "types": { "adaptive": { "supported": true } } }
            }
        }))
        .unwrap();
        let efforts = info
            .model
            .reasoning_efforts
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(efforts, ["default", "low", "max"]);
        assert_eq!(info.max_tokens, MAX_OUTPUT_TOKENS);
        assert!(info.adaptive_thinking);
        assert!(parse_model(&json!({ "id": "old", "lifecycle": "deprecated" })).is_none());
    }

    #[test]
    fn rebuilds_thinking_text_and_tool_use_from_events() {
        let mut turn = Turn::default();
        let mut text = String::new();
        for event in [
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 10, "cache_read_input_tokens": 90 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "hmm" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig" } }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "Reading." } }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "t1", "name": "read_file", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "\"a.rs\"}" } }),
            json!({ "type": "content_block_stop", "index": 2 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 40 } }),
            json!({ "type": "message_stop" }),
        ] {
            if let Some(delta) = turn.apply(&event).unwrap() {
                text.push_str(&delta);
            }
        }
        assert!(turn.done);
        assert_eq!(text, "Reading.");
        assert_eq!(turn.stop_reason, "tool_use");
        assert_eq!(turn.input_tokens + turn.output_tokens, 140);
        assert_eq!(turn.blocks[0]["signature"], "sig");
        assert_eq!(turn.blocks[2]["input"]["path"], "a.rs");
    }

    #[test]
    fn answers_tool_calls_left_open_by_an_interrupt() {
        let mut history = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "go" }] }),
            json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "a", "name": "read_file", "input": {} }] }),
        ];
        push_user_text(&mut history, "next".to_owned());
        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["content"][0]["tool_use_id"], "a");
        assert_eq!(history[2]["content"][1]["text"], "next");
    }
}
