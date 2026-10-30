use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

const CACHE_VERSION: u8 = 1;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Entry<T> {
    version: u8,
    saved_at: u64,
    value: T,
}

pub(super) fn load<T: DeserializeOwned>(key: &str, max_age: Duration) -> Option<T> {
    let path = path(key).ok()?;
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > MAX_CACHE_BYTES {
        return None;
    }
    let entry: Entry<T> = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    (entry.version == CACHE_VERSION && now.saturating_sub(entry.saved_at) <= max_age.as_secs())
        .then_some(entry.value)
}

pub(super) fn store<T: Serialize>(key: &str, value: &T) -> Result<()> {
    let path = path(key)?;
    let directory = path.parent().context("provider cache path has no parent")?;
    crate::runtime_paths::prepare_private_directory(directory)?;
    let saved_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let bytes = serde_json::to_vec(&Entry {
        version: CACHE_VERSION,
        saved_at,
        value,
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("could not replace {}", path.display()))
}

pub(super) fn scoped_key(provider: &str, scope: &str) -> String {
    let digest = Sha256::digest(scope.as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{provider}-{suffix}")
}

fn path(key: &str) -> Result<PathBuf> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid provider cache key");
    }
    let base = BaseDirs::new().context("could not determine the home directory")?;
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.home_dir().join(".cache"));
    if !root.is_absolute() {
        bail!("XDG_CACHE_HOME must be absolute");
    }
    Ok(root.join("nonaut/providers").join(format!("{key}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_keys_are_stable_and_separate_endpoints() {
        assert_eq!(
            scoped_key("provider", "https://one.example"),
            scoped_key("provider", "https://one.example")
        );
        assert_ne!(
            scoped_key("provider", "https://one.example"),
            scoped_key("provider", "https://two.example")
        );
    }

    #[test]
    fn cache_keys_cannot_escape_the_cache_directory() {
        assert!(path("../outside").is_err());
        assert!(path("provider-name_1").is_ok());
    }
}
