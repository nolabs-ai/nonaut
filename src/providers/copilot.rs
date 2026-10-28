//! GitHub Copilot provider backed by the installed Copilot CLI.

use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use github_copilot_sdk::{
    Client, ClientOptions, ToolResult,
    session::Session,
    tool::ToolHandler,
    types::{
        MessageOptions, SessionConfig, SystemMessageConfig, Tool, ToolInvocation,
        ToolResultExpanded,
    },
};
use tokio::sync::{RwLock, mpsc};

use crate::{config, tools};

use super::{
    BASE_INSTRUCTIONS, Model, ProviderEvent, ProviderKind, ReasoningOption, developer_instructions,
};

type Events = mpsc::UnboundedSender<(u64, ProviderEvent)>;
type ActiveEvents = Arc<StdMutex<Option<(u64, Events)>>>;

#[derive(Clone)]
pub(crate) struct Copilot {
    client: Client,
    cwd: PathBuf,
    models: Vec<Model>,
    selected_model: Arc<StdRwLock<Model>>,
    selected_effort: Arc<StdRwLock<String>>,
    session: Arc<RwLock<Arc<Session>>>,
    active_events: ActiveEvents,
}

impl Copilot {
    pub async fn connect(cwd: &Path) -> Result<Self> {
        let executable = find_executable("copilot").context(
            "GitHub Copilot requires the `copilot` CLI on PATH; install it and sign in first",
        )?;
        let client = Client::start(ClientOptions::new().with_program(executable).with_cwd(cwd))
            .await
            .context("could not start the GitHub Copilot CLI")?;
        let models = client
            .list_models()
            .await
            .context("could not list GitHub Copilot models")?
            .into_iter()
            .map(model_option)
            .collect::<Vec<_>>();
        let saved = config::load_provider_config(ProviderKind::Copilot)?;
        let selected = saved
            .model
            .as_deref()
            .and_then(|id| models.iter().find(|model| model.id == id))
            .or_else(|| models.iter().find(|model| model.id == "auto"))
            .or_else(|| models.first())
            .cloned()
            .context("GitHub Copilot returned no models")?;
        let effort = saved
            .reasoning
            .filter(|effort| {
                selected
                    .reasoning_efforts
                    .iter()
                    .any(|item| item.id == *effort)
            })
            .unwrap_or_else(|| selected.default_reasoning_effort.clone());
        let active_events = ActiveEvents::default();
        let session = create_session(&client, cwd, &selected.id, &effort, &active_events).await?;

        Ok(Self {
            client,
            cwd: cwd.to_path_buf(),
            models,
            selected_model: Arc::new(StdRwLock::new(selected)),
            selected_effort: Arc::new(StdRwLock::new(effort)),
            session: Arc::new(RwLock::new(Arc::new(session))),
            active_events,
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
            .with_context(|| format!("GitHub Copilot did not list model {model_id}"))?;
        *self
            .selected_model
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = model.clone();
        *self
            .selected_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            model.default_reasoning_effort.clone();
        Ok(model)
    }

    pub fn select_reasoning_effort(&self, effort: &str) -> Result<String> {
        if !self
            .model()
            .reasoning_efforts
            .iter()
            .any(|option| option.id == effort)
        {
            bail!("the selected GitHub Copilot model does not support `{effort}` reasoning");
        }
        *self
            .selected_effort
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = effort.to_owned();
        Ok(effort.to_owned())
    }

    pub async fn new_session(&self) -> Result<()> {
        self.interrupt().await?;
        let old = self.session.read().await.clone();
        old.disconnect().await.ok();
        let model = self.model();
        let effort = self.effort();
        let new = create_session(
            &self.client,
            &self.cwd,
            &model.id,
            &effort,
            &self.active_events,
        )
        .await?;
        *self.session.write().await = Arc::new(new);
        Ok(())
    }

    pub async fn stream(&self, input: String, request_id: u64, sender: Events) -> Result<()> {
        *lock(&self.active_events) = Some((request_id, sender.clone()));
        let result = self.run_turn(input, request_id, &sender).await;
        lock(&self.active_events).take();
        result
    }

    pub async fn interrupt(&self) -> Result<()> {
        if lock(&self.active_events).is_none() {
            return Ok(());
        }
        self.session
            .read()
            .await
            .abort()
            .await
            .context("could not interrupt GitHub Copilot")?;
        lock(&self.active_events).take();
        Ok(())
    }

    async fn run_turn(&self, input: String, request_id: u64, sender: &Events) -> Result<()> {
        let session = self.session.read().await.clone();
        let model = self.model();
        let effort = self.effort();
        let options = (effort != "default").then(|| {
            github_copilot_sdk::types::SetModelOptions::default().with_reasoning_effort(effort)
        });
        session
            .set_model(&model.id, options)
            .await
            .context("could not select the GitHub Copilot model")?;
        let mut events = session.subscribe();
        session
            .send(MessageOptions::new(input))
            .await
            .context("GitHub Copilot rejected the message")?;

        while let Ok(event) = events.recv().await {
            match event.event_type.as_str() {
                "assistant.message_delta" => {
                    if let Some(delta) = event
                        .data
                        .get("deltaContent")
                        .or_else(|| event.data.get("delta"))
                        .and_then(|value| value.as_str())
                    {
                        let _ = sender.send((request_id, ProviderEvent::Delta(delta.to_owned())));
                    }
                }
                "assistant.usage" => {
                    let tokens = usage_tokens(&event.data);
                    if tokens > 0 {
                        let _ = sender.send((request_id, ProviderEvent::Usage(tokens)));
                    }
                }
                "session.idle" | "assistant.turn_end" => {
                    let _ = sender.send((request_id, ProviderEvent::Completed));
                    return Ok(());
                }
                "session.error" => {
                    let message = event.data["message"]
                        .as_str()
                        .unwrap_or("GitHub Copilot session failed");
                    bail!("{message}");
                }
                "abort" | "session.aborted" => {
                    let _ = sender.send((request_id, ProviderEvent::Interrupted));
                    return Ok(());
                }
                _ => {}
            }
        }
        bail!("GitHub Copilot closed the event stream")
    }

    fn effort(&self) -> String {
        self.selected_effort
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

struct NonautTool {
    workspace: PathBuf,
    name: String,
    active_events: ActiveEvents,
}

#[async_trait]
impl ToolHandler for NonautTool {
    async fn call(
        &self,
        invocation: ToolInvocation,
    ) -> std::result::Result<ToolResult, github_copilot_sdk::Error> {
        let active = lock(&self.active_events).clone();
        if let Some((request_id, sender)) = &active {
            let context = tools::audit_context(
                &invocation.tool_call_id,
                &self.workspace,
                &self.name,
                &invocation.arguments,
            );
            let _ = sender.send((*request_id, ProviderEvent::ToolStarted(context)));
        }
        let result = tools::execute(&self.workspace, &self.name, &invocation.arguments).await;
        let (success, output, failure) = match result {
            Ok(output) => (true, output, None),
            Err(error) => {
                let failure = tools::audit_failure(&error);
                (false, format!("{error:#}"), Some(failure))
            }
        };
        if let Some((request_id, sender)) = active {
            let _ = sender.send((
                request_id,
                ProviderEvent::ToolFinished {
                    call_id: invocation.tool_call_id,
                    tool: self.name.clone(),
                    success,
                    output: output.clone(),
                    failure,
                },
            ));
        }
        Ok(if success {
            ToolResult::Text(nonempty(output))
        } else {
            ToolResult::Expanded(
                ToolResultExpanded::new(output.clone(), "failure").with_error(output),
            )
        })
    }
}

async fn create_session(
    client: &Client,
    cwd: &Path,
    model: &str,
    effort: &str,
    active_events: &ActiveEvents,
) -> Result<Session> {
    let instructions = format!(
        "{BASE_INSTRUCTIONS}\n\n{}\n\nWorkspace: {}",
        developer_instructions(cwd),
        cwd.display()
    );
    let specs = tools::specs();
    let declared_tools = specs.as_array().into_iter().flatten().map(|spec| {
        let name = spec["name"].as_str().unwrap_or_default().to_owned();
        Tool::new(&name)
            .with_description(spec["description"].as_str().unwrap_or_default())
            .with_parameters(spec["inputSchema"].clone())
            .with_skip_permission(true)
            .with_handler(Arc::new(NonautTool {
                workspace: cwd.to_path_buf(),
                name,
                active_events: active_events.clone(),
            }))
    });
    let mut config = SessionConfig::default()
        .with_model(model)
        .with_streaming(true)
        .with_system_message(
            SystemMessageConfig::new()
                .with_mode("replace")
                .with_content(instructions),
        )
        .with_tools(declared_tools)
        .with_available_tools(std::iter::empty::<String>());
    if effort != "default" {
        config = config.with_reasoning_effort(effort);
    }
    client
        .create_session(config)
        .await
        .context("could not create a GitHub Copilot session")
}

fn model_option(model: github_copilot_sdk::types::Model) -> Model {
    let efforts = model
        .supported_reasoning_efforts
        .unwrap_or_else(|| vec!["default".to_owned()]);
    let default = model
        .default_reasoning_effort
        .filter(|effort| efforts.contains(effort))
        .unwrap_or_else(|| {
            efforts
                .first()
                .cloned()
                .unwrap_or_else(|| "default".to_owned())
        });
    Model {
        id: model.id,
        display_name: model.name,
        default_reasoning_effort: default,
        reasoning_efforts: efforts
            .into_iter()
            .map(|id| ReasoningOption {
                description: reasoning_description(&id),
                id,
            })
            .collect(),
    }
}

fn reasoning_description(effort: &str) -> String {
    match effort {
        "low" => "Faster responses".to_owned(),
        "medium" => "Balanced reasoning".to_owned(),
        "high" => "Deeper reasoning".to_owned(),
        _ => "Provider default".to_owned(),
    }
}

fn usage_tokens(data: &serde_json::Value) -> u64 {
    ["inputTokens", "outputTokens"]
        .into_iter()
        .filter_map(|key| data.get(key).and_then(|value| value.as_u64()))
        .sum()
}

fn nonempty(output: String) -> String {
    if output.trim().is_empty() {
        "(no output)".to_owned()
    } else {
        output
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_combines_input_and_output_tokens() {
        assert_eq!(
            usage_tokens(&serde_json::json!({ "inputTokens": 12, "outputTokens": 7 })),
            19
        );
    }
}
