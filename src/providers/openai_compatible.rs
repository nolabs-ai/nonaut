//! Shared agent loop for OpenAI-compatible chat-completions endpoints.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock as StdRwLock},
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
};

use crate::{config, tools};

use super::{
    COMPACTION_PROMPT, Model, PromptFrame, ProviderEvent, ProviderKind, ReasoningOption,
    ToolLoopGuard, compacted_user_message, history_needs_compaction,
};

const DEFAULT_EFFORT: &str = "default";
const MODEL_CACHE_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

type Events = mpsc::UnboundedSender<(u64, ProviderEvent)>;

#[derive(Clone)]
pub(crate) struct OpenAiCompatible {
    client: Client,
    base_url: String,
    provider_name: &'static str,
    cwd: PathBuf,
    models: Vec<Model>,
    selected_model: Arc<StdRwLock<Model>>,
    history: Arc<Mutex<Vec<Value>>>,
    active: Arc<Mutex<Option<JoinHandle<()>>>>,
    frame: Arc<PromptFrame>,
}

impl OpenAiCompatible {
    pub async fn connect(kind: ProviderKind, cwd: &Path) -> Result<Self> {
        let saved = config::load_provider_config(kind)?;
        let endpoint = Endpoint::for_provider(kind, saved.base_url, saved.api_key_env)?;
        let cache_key = super::cache::scoped_key(kind.id(), &endpoint.cache_scope());
        let client = endpoint.client()?;
        let models = match super::cache::load(&cache_key, MODEL_CACHE_AGE) {
            Some(models) => models,
            None => {
                let models = list_models(&client, &endpoint.base_url, endpoint.name).await?;
                if !models.is_empty() {
                    let _ = super::cache::store(&cache_key, &models);
                }
                models
            }
        };
        let default = saved
            .model
            .as_deref()
            .and_then(|id| models.iter().find(|model| model.id == id))
            .or_else(|| models.first())
            .cloned()
            .context("the provider returned no tool-capable models")?;

        let frame = Arc::new(PromptFrame::new(cwd));
        Ok(Self {
            client,
            base_url: endpoint.base_url,
            provider_name: endpoint.name,
            cwd: cwd.to_path_buf(),
            models,
            selected_model: Arc::new(StdRwLock::new(default)),
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
            .with_context(|| format!("{} did not list model {model_id}", self.provider_name))?;
        *self
            .selected_model
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = model.clone();
        Ok(model)
    }

    pub fn select_reasoning_effort(&self, effort: &str) -> Result<String> {
        if effort != DEFAULT_EFFORT {
            bail!(
                "{} does not advertise reasoning controls",
                self.provider_name
            );
        }
        Ok(effort.to_owned())
    }

    pub async fn new_session(&self) -> Result<()> {
        self.interrupt().await?;
        self.history.lock().await.clear();
        Ok(())
    }

    pub async fn stream(&self, input: String, request_id: u64, sender: Events) -> Result<()> {
        let (result_tx, result_rx) = oneshot::channel();
        let provider = self.clone();
        let handle = tokio::spawn(async move {
            let _ = result_tx.send(provider.run_turn(input, request_id, &sender).await);
        });
        let abort = handle.abort_handle();
        *self.active.lock().await = Some(handle);
        let result = result_rx.await;
        abort.abort();
        self.active.lock().await.take();
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
        self.history
            .lock()
            .await
            .push(json!({ "role": "user", "content": input }));
        let mut recent_calls = ToolLoopGuard::default();
        loop {
            self.compact_history().await?;
            let mut messages = vec![json!({ "role": "system", "content": self.frame.system })];
            messages.extend(self.history.lock().await.clone());
            let body = json!({
                "model": self.model().id,
                "messages": messages,
                "tools": self.frame.openai_tools,
                "tool_choice": "auto",
                "stream": true,
                "stream_options": { "include_usage": true }
            });
            let response = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
                .with_context(|| format!("could not reach {}", self.provider_name))?;
            if !response.status().is_success() {
                return api_error(response, self.provider_name).await;
            }
            let turn = read_response(response, request_id, sender).await?;
            if turn.tokens > 0 {
                let _ = sender.send((request_id, ProviderEvent::Usage(turn.tokens)));
            }
            self.history.lock().await.push(turn.assistant_message());
            if turn.calls.is_empty() {
                let _ = sender.send((request_id, ProviderEvent::Completed));
                return Ok(());
            }
            for call in turn.calls {
                let result = self
                    .run_tool(&call, request_id, sender, &mut recent_calls)
                    .await;
                self.history.lock().await.push(result);
            }
        }
    }

    async fn compact_history(&self) -> Result<()> {
        let history = self.history.lock().await.clone();
        if !history_needs_compaction(&history) {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({
                "model": self.model().id,
                "messages": [
                    {"role": "system", "content": COMPACTION_PROMPT},
                    {"role": "user", "content": serde_json::to_string(&history).unwrap_or_default()}
                ],
                "max_tokens": 4096,
                "stream": false
            }))
            .send()
            .await
            .with_context(|| format!("could not compact {} history", self.provider_name))?;
        if !response.status().is_success() {
            return api_error(response, self.provider_name).await;
        }
        let value: Value = response
            .json()
            .await
            .context("invalid compaction response")?;
        let summary = value["choices"][0]["message"]["content"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .context("the provider returned an empty compaction summary")?;
        *self.history.lock().await = vec![json!({
            "role": "user",
            "content": compacted_user_message(summary)
        })];
        Ok(())
    }

    async fn run_tool(
        &self,
        call: &ToolCall,
        request_id: u64,
        sender: &Events,
        recent_calls: &mut ToolLoopGuard,
    ) -> Value {
        let context = tools::audit_context(&call.id, &self.cwd, &call.name, &call.arguments);
        let _ = sender.send((request_id, ProviderEvent::ToolStarted(context)));
        let progress = super::tool_progress_reporter(request_id, call.id.clone(), sender.clone());
        let (success, output, failure) = match recent_calls
            .execute(&self.cwd, &call.name, &call.arguments, Some(progress))
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
                call_id: call.id.clone(),
                tool: call.name.clone(),
                success,
                output: output.clone(),
                failure,
            },
        ));
        json!({
            "role": "tool",
            "tool_call_id": call.id,
            "content": if output.trim().is_empty() { "(no output)" } else { &output }
        })
    }
}

struct Endpoint {
    name: &'static str,
    base_url: String,
    api_key_env: Option<String>,
    require_key: bool,
}

impl Endpoint {
    fn for_provider(
        kind: ProviderKind,
        configured_url: Option<String>,
        configured_key_env: Option<String>,
    ) -> Result<Self> {
        let has_configured_key = configured_key_env.is_some();
        let (name, default_url, default_key, require_key) = match kind {
            #[cfg(feature = "provider-gemini")]
            ProviderKind::Gemini => (
                "Gemini API",
                "https://generativelanguage.googleapis.com/v1beta/openai",
                Some("GEMINI_API_KEY"),
                true,
            ),
            #[cfg(feature = "provider-openrouter")]
            ProviderKind::OpenRouter => (
                "OpenRouter",
                "https://openrouter.ai/api/v1",
                Some("OPENROUTER_API_KEY"),
                true,
            ),
            #[cfg(feature = "provider-ollama")]
            ProviderKind::Ollama => (
                "Ollama",
                "http://127.0.0.1:11434/v1",
                Some("OLLAMA_API_KEY"),
                false,
            ),
            #[cfg(feature = "provider-openai-compatible")]
            ProviderKind::OpenAiCompatible => ("OpenAI-compatible API", "", None, false),
            _ => bail!("{} is not an OpenAI-compatible provider", kind.id()),
        };
        let base_url = configured_url.unwrap_or_else(|| default_url.to_owned());
        if base_url.trim().is_empty() {
            bail!("set agent.providers.{}.base_url in config.json", kind.id());
        }
        let base_url = validate_base_url(&base_url)?;
        Ok(Self {
            name,
            base_url,
            api_key_env: configured_key_env.or_else(|| default_key.map(str::to_owned)),
            require_key: require_key || has_configured_key,
        })
    }

    fn client(&self) -> Result<Client> {
        let mut headers = HeaderMap::new();
        if let Some(name) = &self.api_key_env {
            match std::env::var(name)
                .ok()
                .filter(|key| !key.trim().is_empty())
            {
                Some(key) => {
                    let mut value = HeaderValue::from_str(&format!("Bearer {}", key.trim()))
                        .with_context(|| format!("{name} is not a valid HTTP credential"))?;
                    value.set_sensitive(true);
                    headers.insert(AUTHORIZATION, value);
                }
                None if self.require_key => {
                    bail!("{name} is not set; export it and restart nonaut")
                }
                None => {}
            }
        }
        Client::builder()
            .default_headers(headers)
            .build()
            .context("could not build the provider HTTP client")
    }

    fn cache_scope(&self) -> String {
        let credential = self
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .unwrap_or_default();
        format!("{}\0{credential}", self.base_url)
    }
}

fn validate_base_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value.trim()).context("provider base_url is not a valid URL")?;
    let local_http = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if url.scheme() != "https" && !local_http {
        bail!("provider base_url must use HTTPS, except for loopback endpoints");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("provider base_url cannot contain credentials, a query, or a fragment");
    }
    Ok(value.trim().trim_end_matches('/').to_owned())
}

async fn list_models(client: &Client, base_url: &str, name: &str) -> Result<Vec<Model>> {
    let response = client
        .get(format!("{base_url}/models"))
        .send()
        .await
        .with_context(|| format!("could not reach {name}"))?;
    if !response.status().is_success() {
        return api_error(response, name).await;
    }
    let value: Value = response
        .json()
        .await
        .context("invalid model-list response")?;
    let mut models = value["data"]
        .as_array()
        .context("the provider returned an invalid model list")?
        .iter()
        .filter(|value| {
            value["supported_parameters"]
                .as_array()
                .is_none_or(|parameters| parameters.iter().any(|item| item == "tools"))
        })
        .filter_map(|value| {
            let id = value["id"].as_str()?;
            let display = value["name"].as_str().unwrap_or(id);
            Some(model_option(id, display))
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(models)
}

fn model_option(id: &str, display_name: &str) -> Model {
    Model {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        default_reasoning_effort: DEFAULT_EFFORT.to_owned(),
        reasoning_efforts: vec![ReasoningOption {
            id: DEFAULT_EFFORT.to_owned(),
            description: "Provider default".to_owned(),
        }],
    }
}

#[derive(Clone)]
struct ToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Turn {
    text: String,
    calls: Vec<ToolCall>,
    tokens: u64,
}

impl Turn {
    fn assistant_message(&self) -> Value {
        let calls = self
            .calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned())
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut message = json!({ "role": "assistant", "content": self.text });
        if !calls.is_empty() {
            message["tool_calls"] = Value::Array(calls);
        }
        message
    }
}

async fn read_response(
    response: reqwest::Response,
    request_id: u64,
    sender: &Events,
) -> Result<Turn> {
    let is_stream = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"));
    if !is_stream {
        let value: Value = response.json().await.context("invalid provider response")?;
        return complete_turn(&value, request_id, sender);
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut text = String::new();
    let mut calls = BTreeMap::<usize, PartialCall>::new();
    let mut tokens = 0;
    while let Some(chunk) = stream.next().await {
        buffer.extend(chunk.context("the provider stream was interrupted")?);
        while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=end).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                return finish_stream(text, calls, tokens);
            }
            let event: Value =
                serde_json::from_str(data).context("invalid provider stream event")?;
            let delta = &event["choices"][0]["delta"];
            if let Some(content) = delta["content"].as_str()
                && !content.is_empty()
            {
                text.push_str(content);
                let _ = sender.send((request_id, ProviderEvent::Delta(content.to_owned())));
            }
            for part in delta["tool_calls"].as_array().into_iter().flatten() {
                let index = part["index"].as_u64().unwrap_or_default() as usize;
                let call = calls.entry(index).or_default();
                if let Some(id) = part["id"].as_str() {
                    call.id.push_str(id);
                }
                if let Some(name) = part["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                if let Some(arguments) = part["function"]["arguments"].as_str() {
                    call.arguments.push_str(arguments);
                }
            }
            if let Some(total) = event["usage"]["total_tokens"].as_u64() {
                tokens = total;
            }
        }
    }
    if buffer.is_empty() {
        finish_stream(text, calls, tokens)
    } else {
        bail!("the provider stream ended with an incomplete event")
    }
}

fn finish_stream(text: String, calls: BTreeMap<usize, PartialCall>, tokens: u64) -> Result<Turn> {
    let calls = calls
        .into_values()
        .map(|call| {
            Ok(ToolCall {
                id: call.id,
                name: call.name,
                arguments: serde_json::from_str(&call.arguments)
                    .context("the provider returned invalid tool arguments")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Turn {
        text,
        calls,
        tokens,
    })
}

fn complete_turn(value: &Value, request_id: u64, sender: &Events) -> Result<Turn> {
    let message = &value["choices"][0]["message"];
    let text = message["content"].as_str().unwrap_or_default().to_owned();
    if !text.is_empty() {
        let _ = sender.send((request_id, ProviderEvent::Delta(text.clone())));
    }
    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| {
            Ok(ToolCall {
                id: call["id"]
                    .as_str()
                    .context("tool call had no id")?
                    .to_owned(),
                name: call["function"]["name"]
                    .as_str()
                    .context("tool call had no name")?
                    .to_owned(),
                arguments: serde_json::from_str(
                    call["function"]["arguments"].as_str().unwrap_or("{}"),
                )
                .context("the provider returned invalid tool arguments")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Turn {
        text,
        calls,
        tokens: value["usage"]["total_tokens"].as_u64().unwrap_or_default(),
    })
}

async fn api_error<T>(response: reqwest::Response, name: &str) -> Result<T> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or_default();
    let message = value["error"]["message"].as_str().unwrap_or(text.trim());
    bail!("{name} error {status}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_streamed_tool_calls() {
        let mut calls = BTreeMap::new();
        calls.insert(
            0,
            PartialCall {
                id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"README.md"}"#.to_owned(),
            },
        );
        let turn = finish_stream(String::new(), calls, 42).unwrap();
        assert_eq!(turn.calls[0].name, "read_file");
        assert_eq!(turn.calls[0].arguments["path"], "README.md");
        assert_eq!(turn.tokens, 42);
    }

    #[test]
    #[cfg(feature = "provider-openai-compatible")]
    fn custom_endpoint_requires_a_base_url() {
        let error = Endpoint::for_provider(ProviderKind::OpenAiCompatible, None, None)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("base_url"));
    }

    #[test]
    fn compatible_endpoints_require_https_or_loopback() {
        assert!(validate_base_url("https://api.example.com/v1").is_ok());
        assert!(validate_base_url("http://127.0.0.1:11434/v1").is_ok());
        assert!(validate_base_url("http://api.example.com/v1").is_err());
    }
}
