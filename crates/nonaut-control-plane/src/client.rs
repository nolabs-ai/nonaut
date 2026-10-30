use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::{
    FinalizedSession,
    audit::{self, FinalizedRecord},
    paths::Paths,
    storage,
};

const PROTOCOL_VERSION: &str = "1";
const KEY_ALGORITHM: &str = "ecdsa_p256_sha256_fixed";
const INGEST_PATH: &str = "/api/v1/audit/ingest";
const RESPONSE_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub struct Client {
    paths: Paths,
    http: reqwest::Client,
}

pub struct Enrollment<'a> {
    pub url: &'a str,
    pub token: &'a str,
    pub display_name: Option<&'a str>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollmentStatus {
    pub platform_url: String,
    pub tenant_id: String,
    pub subject_id: String,
    pub enrolled_at: String,
}

#[derive(Debug)]
pub struct SyncReport {
    pub delivered: usize,
    pub remaining: usize,
}

#[derive(Serialize)]
struct EnrollmentRequest<'a> {
    protocol_version: &'static str,
    token: &'a str,
    subject_kind: &'static str,
    display_name: Option<&'a str>,
    key_algorithm: &'static str,
    public_key: String,
}

#[derive(Deserialize)]
struct EnrollmentResponse {
    protocol_version: String,
    tenant_id: String,
    subject_id: String,
    subject_kind: String,
    management_mode: String,
    enrolled_at: String,
}

#[derive(Serialize)]
struct IngestEnvelope {
    ingest_id: Uuid,
    tenant_id: String,
    session_id: String,
    shipper_id: String,
    principal_id: Option<String>,
    profile_ref: Option<String>,
    merkle_root: String,
    hash_chain_head: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    session_metadata_json: String,
    session_digest: String,
    events_ndjson: String,
}

#[derive(Serialize, Deserialize)]
struct QueuedRequest {
    request_id: Uuid,
    body: String,
    session_directory: PathBuf,
}

impl Client {
    pub fn from_environment() -> Result<Self> {
        Ok(Self {
            paths: Paths::from_environment()?,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(20))
                .build()?,
        })
    }

    pub fn status(&self) -> Result<Option<EnrollmentStatus>> {
        match fs::read(&self.paths.enrollment) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove this device's local enrollment credentials.
    ///
    /// Durable audit sessions and queued deliveries are intentionally retained.
    /// This does not revoke the device in the remote control plane.
    pub fn unenroll(&self) -> Result<bool> {
        let key_removed = remove_file_if_exists(&self.paths.key)?;
        let enrollment_removed = remove_file_if_exists(&self.paths.enrollment)?;
        Ok(key_removed || enrollment_removed)
    }

    pub async fn enroll(&self, enrollment: Enrollment<'_>) -> Result<EnrollmentStatus> {
        if self.status()?.is_some() || self.paths.key.exists() {
            bail!("Nonaut is already enrolled; key rotation is not implemented");
        }
        let platform_url = validate_url(enrollment.url)?;
        let key =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("could not generate the enrollment key"))?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, key.as_ref())
            .map_err(|_| anyhow::anyhow!("could not load the enrollment key"))?;
        let request = EnrollmentRequest {
            protocol_version: PROTOCOL_VERSION,
            token: enrollment.token,
            subject_kind: "device",
            display_name: enrollment.display_name,
            key_algorithm: KEY_ALGORITHM,
            public_key: URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
        };
        let response = self
            .http
            .post(endpoint(&platform_url, "/api/v1/enrollment/exchange")?)
            .json(&request)
            .send()
            .await
            .context("enrollment request failed")?;
        let status = response.status();
        let body = response_text(response).await?;
        if !status.is_success() {
            bail!("platform enrollment returned HTTP {status}: {body}");
        }
        let response: EnrollmentResponse = serde_json::from_str(&body)
            .context("platform returned an invalid enrollment response")?;
        if response.protocol_version != PROTOCOL_VERSION
            || response.subject_kind != "device"
            || response.management_mode != "audit_only"
        {
            bail!("platform returned an unsupported enrollment contract");
        }
        validate_identifier("tenant id", &response.tenant_id)?;
        validate_identifier("subject id", &response.subject_id)?;
        let state = EnrollmentStatus {
            platform_url,
            tenant_id: response.tenant_id,
            subject_id: response.subject_id,
            enrolled_at: response.enrolled_at,
        };
        storage::write(&self.paths.key, key.as_ref())?;
        if let Err(error) = storage::write_json(&self.paths.enrollment, &state) {
            let _ = fs::remove_file(&self.paths.key);
            return Err(error);
        }
        Ok(state)
    }

    pub fn queue(&self, session: FinalizedSession) -> Result<()> {
        let state = self.status()?.context("Nonaut is not enrolled")?;
        let finalized = audit::load(&session.directory)?;
        let request_id = Uuid::now_v7();
        let body = serde_json::to_string(&build_envelope(
            request_id,
            &state,
            &session.directory,
            finalized,
        )?)?;
        let queued = QueuedRequest {
            request_id,
            body,
            session_directory: session.directory,
        };
        let path = self.paths.outbox.join(format!("{request_id}.json"));
        storage::write_json(&path, &queued)
    }

    pub fn queued_count(&self) -> Result<usize> {
        Ok(queue_paths(&self.paths.outbox)?.len())
    }

    pub async fn sync(&self) -> Result<SyncReport> {
        let state = self.status()?.context("Nonaut is not enrolled")?;
        self.queue_finalized_sessions(&state)?;
        let mut delivered = 0;
        for path in queue_paths(&self.paths.outbox)? {
            let queued: QueuedRequest = serde_json::from_slice(&fs::read(&path)?)?;
            if self.deliver(&state, &path, &queued).await.is_ok() {
                delivered += 1;
            }
        }
        Ok(SyncReport {
            delivered,
            remaining: self.queued_count()?,
        })
    }

    async fn deliver(
        &self,
        state: &EnrollmentStatus,
        queue_path: &Path,
        queued: &QueuedRequest,
    ) -> Result<()> {
        let signed = self.sign(state, queued.request_id, queued.body.as_bytes())?;
        let response = self
            .http
            .post(endpoint(&state.platform_url, INGEST_PATH)?)
            .header("Content-Type", "application/json")
            .header("X-Nono-Protocol-Version", PROTOCOL_VERSION)
            .header("X-Nono-Subject-Id", &state.subject_id)
            .header("X-Nono-Timestamp", signed.timestamp)
            .header("X-Nono-Request-Id", queued.request_id.to_string())
            .header("X-Nono-Content-SHA256", signed.body_digest)
            .header("X-Nono-Signature", signed.signature)
            .body(queued.body.clone())
            .send()
            .await
            .context("audit delivery failed")?;
        let status = response.status();
        let body = response_text(response).await?;
        if !status.is_success() {
            bail!("audit delivery returned HTTP {status}: {body}");
        }
        storage::write(
            &queued.session_directory.join("platform-receipt.json"),
            body.as_bytes(),
        )?;
        fs::remove_file(queue_path)?;
        Ok(())
    }

    fn sign(
        &self,
        state: &EnrollmentStatus,
        request_id: Uuid,
        body: &[u8],
    ) -> Result<SignedHeaders> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        let body_digest = format!("sha256:{}", sha256(body));
        let canonical = canonical_request(&state.subject_id, &timestamp, request_id, &body_digest);
        let key = fs::read(&self.paths.key).context("could not read the enrollment key")?;
        let mode = fs::metadata(&self.paths.key)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!("the enrollment key must not be accessible by group or other users");
        }
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key)
            .map_err(|_| anyhow::anyhow!("the enrollment key is invalid"))?;
        let signature = pair
            .sign(&SystemRandom::new(), canonical.as_bytes())
            .map_err(|_| anyhow::anyhow!("could not sign the audit request"))?;
        Ok(SignedHeaders {
            timestamp,
            body_digest,
            signature: format!("p256-sha256={}", URL_SAFE_NO_PAD.encode(signature.as_ref())),
        })
    }

    fn queue_finalized_sessions(&self, state: &EnrollmentStatus) -> Result<()> {
        let queued_sessions = queue_paths(&self.paths.outbox)?
            .into_iter()
            .filter_map(|path| fs::read(path).ok())
            .filter_map(|bytes| serde_json::from_slice::<QueuedRequest>(&bytes).ok())
            .map(|queued| queued.session_directory)
            .collect::<Vec<_>>();
        let Ok(entries) = fs::read_dir(&self.paths.sessions) else {
            return Ok(());
        };
        for entry in entries {
            let directory = entry?.path();
            if !directory.join("session.json").is_file()
                || directory.join("platform-receipt.json").is_file()
                || queued_sessions.contains(&directory)
            {
                continue;
            }
            let finalized = audit::load(&directory)?;
            let request_id = Uuid::now_v7();
            let body =
                serde_json::to_string(&build_envelope(request_id, state, &directory, finalized)?)?;
            storage::write_json(
                &self.paths.outbox.join(format!("{request_id}.json")),
                &QueuedRequest {
                    request_id,
                    body,
                    session_directory: directory,
                },
            )?;
        }
        Ok(())
    }
}

struct SignedHeaders {
    timestamp: String,
    body_digest: String,
    signature: String,
}

fn build_envelope(
    ingest_id: Uuid,
    state: &EnrollmentStatus,
    directory: &Path,
    session: FinalizedRecord,
) -> Result<IngestEnvelope> {
    Ok(IngestEnvelope {
        ingest_id,
        tenant_id: state.tenant_id.clone(),
        session_id: session.session_id,
        shipper_id: state.subject_id.clone(),
        principal_id: None,
        profile_ref: session.profile_ref,
        merkle_root: session.merkle_root,
        hash_chain_head: Some(session.hash_chain_head),
        started_at: session.started_at,
        ended_at: Some(session.ended_at),
        session_metadata_json: session.session_metadata_json,
        session_digest: session.session_digest,
        events_ndjson: fs::read_to_string(directory.join("audit-events.ndjson"))?,
    })
}

fn queue_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(Vec::new());
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn validate_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value)?;
    let loopback =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !loopback {
        bail!("platform URL must use HTTPS; HTTP is allowed only for loopback");
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn endpoint(base: &str, path: &str) -> Result<String> {
    let mut url = Url::parse(base)?;
    let joined = format!(
        "{}/{}",
        url.path().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    url.set_path(&joined);
    Ok(url.to_string())
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("platform {name} contains unsupported characters");
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_request(
    subject_id: &str,
    timestamp: &str,
    request_id: Uuid,
    body_digest: &str,
) -> String {
    format!(
        "nono-request-v1\nPOST\n{INGEST_PATH}\n{subject_id}\n{timestamp}\n{request_id}\n{body_digest}"
    )
}

async fn response_text(mut response: reqwest::Response) -> Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            bail!("platform response exceeded {RESPONSE_LIMIT} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).context("platform returned non-UTF-8 text")
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client(root: &Path) -> Client {
        Client {
            paths: Paths {
                enrollment: root.join("config/control-plane.json"),
                key: root.join("state/control-plane/device.pk8"),
                sessions: root.join("state/audit"),
                outbox: root.join("state/outbox"),
            },
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn accepts_https_and_loopback_http_only() {
        assert_eq!(
            validate_url("https://platform.example/base/?ignored=yes#fragment").unwrap(),
            "https://platform.example/base"
        );
        assert!(validate_url("http://localhost:8080").is_ok());
        assert!(validate_url("http://platform.example").is_err());
    }

    #[test]
    fn preserves_platform_base_path() {
        assert_eq!(
            endpoint("https://platform.example/control", INGEST_PATH).unwrap(),
            "https://platform.example/control/api/v1/audit/ingest"
        );
    }

    #[test]
    fn rejects_identifiers_that_break_signed_headers() {
        assert!(validate_identifier("subject id", "device-123").is_ok());
        assert!(validate_identifier("subject id", "device\nforged").is_err());
    }

    #[test]
    fn canonical_request_matches_protocol_v1() {
        let request_id = Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap();
        assert_eq!(
            canonical_request("device-1", "1234", request_id, "sha256:abcd"),
            "nono-request-v1\nPOST\n/api/v1/audit/ingest\ndevice-1\n1234\n01900000-0000-7000-8000-000000000001\nsha256:abcd"
        );
    }

    #[test]
    fn unenroll_removes_credentials_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("nonaut-unenroll-test-{}", Uuid::now_v7()));
        let client = test_client(&root);
        storage::write(&client.paths.key, b"private key").unwrap();
        storage::write(&client.paths.enrollment, b"enrollment").unwrap();
        storage::write(&client.paths.outbox.join("queued.json"), b"queued").unwrap();

        assert!(client.unenroll().unwrap());
        assert!(!client.paths.key.exists());
        assert!(!client.paths.enrollment.exists());
        assert!(client.paths.outbox.join("queued.json").exists());
        assert!(!client.unenroll().unwrap());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unenroll_removes_an_orphaned_key() {
        let root = std::env::temp_dir().join(format!("nonaut-unenroll-test-{}", Uuid::now_v7()));
        let client = test_client(&root);
        storage::write(&client.paths.key, b"private key").unwrap();

        assert!(client.unenroll().unwrap());
        assert!(!client.paths.key.exists());

        fs::remove_dir_all(root).unwrap();
    }
}
