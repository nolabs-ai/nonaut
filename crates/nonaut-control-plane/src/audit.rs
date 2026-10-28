use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{paths::Paths, storage};

const EVENT_DOMAIN: &[u8] = b"nono.audit.event.alpha\n";
const CHAIN_DOMAIN: &[u8] = b"nono.audit.chain.alpha\n";
const MERKLE_DOMAIN: &[u8] = b"nono.audit.merkle.alpha\n";
const SESSION_DOMAIN: &[u8] = b"nono.audit.session-digest.alpha\n";

pub struct AuditSession {
    id: String,
    directory: PathBuf,
    events: File,
    started: String,
    sequence: u64,
    chain: Option<[u8; 32]>,
    leaves: Vec<[u8; 32]>,
    started_instant: Instant,
    active_tools: HashMap<String, Instant>,
    tool_calls: u64,
    tools_succeeded: u64,
    tools_failed: u64,
    tools_denied: u64,
    write_capable_tools: u64,
    tokens: u64,
    files_created: u64,
    files_modified: u64,
    files_deleted: u64,
    filesystem_changes_complete: bool,
    profile_ref: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionContext {
    pub nonaut_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    pub sandbox: String,
    pub os: String,
    pub architecture: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolContext {
    pub call_id: String,
    pub tool: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argv_display: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hash: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
}

pub struct ToolOutcome<'a> {
    pub call_id: &'a str,
    pub tool: &'a str,
    pub success: bool,
    pub decision: &'a str,
    pub reason: Option<&'a str>,
    pub exit_code: Option<i32>,
    pub output_bytes: u64,
    pub change: Option<&'a str>,
}

impl ToolContext {
    pub fn minimal(call_id: impl Into<String>, tool: impl Into<String>) -> Self {
        let tool = tool.into();
        Self {
            call_id: call_id.into(),
            operation: tool.clone(),
            tool,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct FinalizedSession {
    pub(crate) directory: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FinalizedRecord {
    pub session_id: String,
    pub started_at: String,
    pub ended_at: String,
    pub merkle_root: String,
    pub hash_chain_head: String,
    pub session_metadata_json: String,
    pub session_digest: String,
    pub profile_ref: Option<String>,
}

#[derive(Serialize)]
struct EventRecord<'a> {
    sequence: u64,
    prev_chain: Option<String>,
    leaf_hash: String,
    chain_hash: String,
    event_json: &'a str,
    event: &'a Value,
}

#[derive(Serialize)]
struct SessionMetadata<'a> {
    session_id: &'a str,
    started: &'a str,
    ended: Option<&'a str>,
    command: [&'static str; 2],
    executable_identity: Option<()>,
    tracked_paths: Vec<Vec<u8>>,
    snapshot_count: u32,
    exit_code: Option<i32>,
    merkle_roots: Vec<String>,
    network_events: Vec<Value>,
    audit_event_count: u64,
    audit_integrity: Integrity,
    audit_attestation: Option<()>,
}

#[derive(Serialize)]
struct Integrity {
    hash_algorithm: &'static str,
    event_count: u64,
    chain_head: String,
    merkle_root: String,
}

impl AuditSession {
    pub fn start(workspace: &Path, context: SessionContext) -> Result<Self> {
        let paths = Paths::from_environment()?;
        Self::start_with_paths(&paths, workspace, context)
    }

    fn start_with_paths(paths: &Paths, workspace: &Path, context: SessionContext) -> Result<Self> {
        let id = Uuid::now_v7().to_string();
        let directory = paths.sessions.join(&id);
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let events = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(directory.join("audit-events.ndjson"))?;
        let started = timestamp();
        let workspace_hash = format!(
            "sha256:{}",
            domain_hash(
                b"nonaut.audit.workspace.v1\n",
                workspace.as_os_str().as_bytes(),
            )
        );
        let profile_ref = (!context.profiles.is_empty()).then(|| context.profiles.join("+"));
        let mut session = Self {
            id,
            directory,
            events,
            started: started.clone(),
            sequence: 0,
            chain: None,
            leaves: Vec::new(),
            started_instant: Instant::now(),
            active_tools: HashMap::new(),
            tool_calls: 0,
            tools_succeeded: 0,
            tools_failed: 0,
            tools_denied: 0,
            write_capable_tools: 0,
            tokens: 0,
            files_created: 0,
            files_modified: 0,
            files_deleted: 0,
            filesystem_changes_complete: true,
            profile_ref,
        };
        session.append(json!({
            "type": "session_started",
            "started": started,
            "command": ["nonaut", "agent-turn"],
            "workspace_hash": workspace_hash
        }))?;
        let mut context = serde_json::to_value(context)?;
        let context = context
            .as_object_mut()
            .context("session context was not an object")?;
        context.insert(
            "type".to_owned(),
            Value::String("nonaut_context".to_owned()),
        );
        context.insert("timestamp".to_owned(), Value::String(timestamp()));
        context.insert("workspace_hash".to_owned(), Value::String(workspace_hash));
        session.append(Value::Object(context.clone()))?;
        Ok(session)
    }

    pub fn tool_started(&mut self, context: &ToolContext) -> Result<()> {
        self.active_tools
            .insert(context.call_id.clone(), Instant::now());
        self.tool_calls += 1;
        if matches!(
            context.tool.as_str(),
            "write_file" | "edit_file" | "delete_file" | "exec" | "run_command"
        ) {
            self.write_capable_tools += 1;
        }
        let mut event = serde_json::to_value(context)?;
        let event = event
            .as_object_mut()
            .context("tool context was not an object")?;
        event.insert(
            "type".to_owned(),
            Value::String("agent_tool_started".to_owned()),
        );
        event.insert("timestamp".to_owned(), Value::String(timestamp()));
        self.append(Value::Object(event.clone()))
    }

    pub fn tool_finished(&mut self, outcome: ToolOutcome<'_>) -> Result<()> {
        let duration_ms = self
            .active_tools
            .remove(outcome.call_id)
            .map(|started| started.elapsed().as_millis() as u64);
        if outcome.success {
            self.tools_succeeded += 1;
        } else {
            self.tools_failed += 1;
        }
        if outcome.decision == "denied" {
            self.tools_denied += 1;
        }
        if matches!(outcome.tool, "exec" | "run_command") {
            self.filesystem_changes_complete = false;
        }
        match outcome.change {
            Some("created") => self.files_created += 1,
            Some("modified") => self.files_modified += 1,
            Some("deleted") => self.files_deleted += 1,
            _ => {}
        }
        let mut event = json!({
            "type": "agent_tool_finished",
            "timestamp": timestamp(),
            "call_id": outcome.call_id,
            "tool": outcome.tool,
            "success": outcome.success,
            "decision": outcome.decision,
            "duration_ms": duration_ms,
            "output_bytes": outcome.output_bytes
        });
        let event = event
            .as_object_mut()
            .context("tool result was not an object")?;
        if let Some(reason) = outcome.reason {
            event.insert("reason".to_owned(), Value::String(reason.to_owned()));
        }
        if let Some(exit_code) = outcome.exit_code {
            event.insert("exit_code".to_owned(), Value::from(exit_code));
        }
        if let Some(change) = outcome.change {
            event.insert("change".to_owned(), Value::String(change.to_owned()));
        }
        self.append(Value::Object(event.clone()))
    }

    pub fn record_usage(&mut self, tokens: u64) {
        self.tokens = tokens;
    }

    pub fn finish(mut self, success: bool) -> Result<FinalizedSession> {
        let ended = timestamp();
        let exit_code = if success { 0 } else { 1 };
        let mut summary = json!({
            "type": "nonaut_turn_summary",
            "timestamp": ended,
            "duration_ms": self.started_instant.elapsed().as_millis() as u64,
            "tool_calls": self.tool_calls,
            "filesystem_changes_complete": self.filesystem_changes_complete
        });
        let summary_fields = summary
            .as_object_mut()
            .context("turn summary was not an object")?;
        for (name, value) in [
            ("tools_succeeded", self.tools_succeeded),
            ("tools_failed", self.tools_failed),
            ("tools_denied", self.tools_denied),
            ("tools_incomplete", self.active_tools.len() as u64),
            ("write_capable_tools", self.write_capable_tools),
            ("files_created", self.files_created),
            ("files_modified", self.files_modified),
            ("files_deleted", self.files_deleted),
            ("reported_tokens", self.tokens),
        ] {
            if value > 0 {
                summary_fields.insert(name.to_owned(), Value::from(value));
            }
        }
        self.append(summary)?;
        self.append(json!({
            "type": "session_ended",
            "ended": ended,
            "exit_code": exit_code
        }))?;
        self.events.sync_all()?;
        let chain = self.chain.context("audit session has no events")?;
        let root = merkle_root(&self.leaves);
        let chain_hex = hex(&chain);
        let root_hex = hex(&root);
        let metadata = SessionMetadata {
            session_id: &self.id,
            started: &self.started,
            ended: Some(&ended),
            command: ["nonaut", "agent-turn"],
            executable_identity: None,
            tracked_paths: Vec::new(),
            snapshot_count: 0,
            exit_code: Some(exit_code),
            merkle_roots: Vec::new(),
            network_events: Vec::new(),
            audit_event_count: self.sequence,
            audit_integrity: Integrity {
                hash_algorithm: "sha256",
                event_count: self.sequence,
                chain_head: chain_hex.clone(),
                merkle_root: root_hex.clone(),
            },
            audit_attestation: None,
        };
        let session_metadata_json = serde_json::to_string(&metadata)?;
        let session_digest = domain_hash(SESSION_DOMAIN, session_metadata_json.as_bytes());
        storage::write_json(
            &self.directory.join("session.json"),
            &FinalizedRecord {
                session_id: self.id,
                started_at: self.started,
                ended_at: ended,
                merkle_root: root_hex,
                hash_chain_head: chain_hex,
                session_metadata_json,
                session_digest,
                profile_ref: self.profile_ref,
            },
        )?;
        Ok(FinalizedSession {
            directory: self.directory,
        })
    }

    fn append(&mut self, event: Value) -> Result<()> {
        let event_json = serde_json::to_string(&event)?;
        let leaf = hash_bytes(EVENT_DOMAIN, event_json.as_bytes());
        let mut hasher = Sha256::new();
        hasher.update(CHAIN_DOMAIN);
        hasher.update(self.chain.unwrap_or([0; 32]));
        hasher.update(leaf);
        let chain: [u8; 32] = hasher.finalize().into();
        let record = EventRecord {
            sequence: self.sequence,
            prev_chain: self.chain.as_ref().map(hex),
            leaf_hash: hex(&leaf),
            chain_hash: hex(&chain),
            event_json: &event_json,
            event: &event,
        };
        serde_json::to_writer(&mut self.events, &record)?;
        self.events.write_all(b"\n")?;
        self.events.flush()?;
        self.sequence += 1;
        self.chain = Some(chain);
        self.leaves.push(leaf);
        Ok(())
    }
}

pub(crate) fn load(directory: &Path) -> Result<FinalizedRecord> {
    let path = directory.join("session.json");
    let bytes = fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("could not parse {}", path.display()))
}

fn timestamp() -> String {
    jiff::Timestamp::now().to_string()
}

fn domain_hash(domain: &[u8], bytes: &[u8]) -> String {
    hex(&hash_bytes(domain, bytes))
}

fn hash_bytes(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hasher.finalize().into()
}

fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| {
                if pair.len() == 1 {
                    pair[0]
                } else {
                    let mut hasher = Sha256::new();
                    hasher.update(MERKLE_DOMAIN);
                    hasher.update(pair[0]);
                    hasher.update(pair[1]);
                    hasher.finalize().into()
                }
            })
            .collect();
    }
    level[0]
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalizes_a_verifiable_session_shape() {
        let root = std::env::temp_dir().join(format!("nonaut-audit-test-{}", Uuid::now_v7()));
        let paths = Paths {
            enrollment: root.join("config/control-plane.json"),
            key: root.join("state/control-plane/device.pk8"),
            sessions: root.join("state/audit"),
            outbox: root.join("state/outbox"),
        };
        let mut session = AuditSession::start_with_paths(
            &paths,
            Path::new("/workspace"),
            SessionContext {
                nonaut_version: "test".to_owned(),
                profiles: vec!["rust".to_owned(), "python".to_owned()],
                ..SessionContext::default()
            },
        )
        .unwrap();
        let tool = ToolContext::minimal("call-1", "read_file");
        session.tool_started(&tool).unwrap();
        session
            .tool_finished(ToolOutcome {
                call_id: "call-1",
                tool: "read_file",
                success: true,
                decision: "allowed",
                reason: None,
                exit_code: None,
                output_bytes: 12,
                change: None,
            })
            .unwrap();
        let finalized = session.finish(true).unwrap();

        let record = load(&finalized.directory).unwrap();
        assert_eq!(record.profile_ref.as_deref(), Some("rust+python"));
        let metadata: Value = serde_json::from_str(&record.session_metadata_json).unwrap();
        assert_eq!(metadata["audit_event_count"], 6);
        assert_eq!(
            metadata["audit_integrity"]["merkle_root"],
            record.merkle_root
        );
        assert_eq!(
            domain_hash(SESSION_DOMAIN, record.session_metadata_json.as_bytes()),
            record.session_digest
        );

        let events = fs::read_to_string(finalized.directory.join("audit-events.ndjson")).unwrap();
        let events = events
            .lines()
            .enumerate()
            .map(|(sequence, line)| {
                let value: Value = serde_json::from_str(line).unwrap();
                assert_eq!(value["sequence"], sequence);
                let event: Value =
                    serde_json::from_str(value["event_json"].as_str().unwrap()).unwrap();
                assert_eq!(event, value["event"]);
                event
            })
            .collect::<Vec<_>>();
        let result = &events[3];
        assert!(result.get("exit_code").is_none());
        assert!(result.get("reason").is_none());
        let summary = &events[4];
        assert_eq!(summary["tools_succeeded"], 1);
        assert!(summary.get("tools_failed").is_none());
        assert!(summary.get("reported_tokens").is_none());
        assert!(summary.get("files_modified").is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
