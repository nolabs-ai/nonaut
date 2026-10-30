//! Workspace snapshots for `/undo`, built on nono's content-addressed store.
//!
//! A turn snapshot starts in the background while the model thinks. Its first
//! mutation waits for completion, preserving the boundary without delaying
//! inference. Snapshots live outside the workspace and `/tmp`, beyond tool
//! reach. `.git`, build output and ignored files are not tracked.

use std::{
    collections::HashMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use nono::undo::{
    Change, ExclusionConfig, ExclusionFilter, SnapshotManager, SnapshotManifest, WalkBudget,
};

const UNTRACKED: [&str; 3] = [".git", "target", "node_modules"];

struct TurnSnapshot {
    undo: Arc<Mutex<Undo>>,
    taken: tokio::sync::OnceCell<()>,
}

static ARMED: OnceLock<Mutex<HashMap<PathBuf, Arc<TurnSnapshot>>>> = OnceLock::new();

pub(crate) struct Undo {
    manager: SnapshotManager,
    session_dir: PathBuf,
    stack: Vec<SnapshotManifest>,
    /// Why the last checkpoint failed, reported when `/undo` finds nothing.
    last_error: Option<String>,
}

impl Undo {
    pub(crate) fn open(workspace: &Path) -> Result<Self> {
        remove_empty_legacy_stores(workspace);
        Self::open_in(workspace, &state_dir()?)
    }

    fn open_in(workspace: &Path, store: &Path) -> Result<Self> {
        let session_dir = store.join(format!("{}-{}", std::process::id(), unix_nanos()));
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("could not create {}", session_dir.display()))?;
        fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700))?;
        let filter = ExclusionFilter::new(
            ExclusionConfig {
                exclude_patterns: UNTRACKED.iter().map(|name| (*name).to_owned()).collect(),
                ..ExclusionConfig::default()
            },
            workspace,
        )
        .context("could not read the workspace ignore rules")?;
        let manager = SnapshotManager::new(
            session_dir.clone(),
            vec![workspace.to_path_buf()],
            filter,
            WalkBudget::default(),
        )
        .context("could not start the undo store")?;
        Ok(Self {
            manager,
            session_dir,
            stack: Vec::new(),
            last_error: None,
        })
    }

    /// Records the workspace as it is now. A failure only disables undo for
    /// the coming turn; it never blocks the turn itself.
    pub(crate) fn checkpoint(&mut self) {
        let result = match self.stack.last() {
            Some(previous) => self
                .manager
                .create_incremental(previous)
                .map(|(manifest, _)| manifest),
            None => self.manager.create_baseline(),
        };
        match result {
            Ok(manifest) => {
                self.stack.push(manifest);
                self.last_error = None;
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }

    /// Restores the newest snapshot. `Ok(None)` means there is nothing to undo.
    pub(crate) fn undo(&mut self) -> Result<Option<Vec<Change>>> {
        let Some(manifest) = self.stack.pop() else {
            return match &self.last_error {
                Some(error) => anyhow::bail!("the last snapshot failed: {error}"),
                None => Ok(None),
            };
        };
        self.manager
            .restore_to(&manifest)
            .map(Some)
            .context("could not restore the snapshot")
    }
}

fn remove_empty_legacy_stores(workspace: &Path) {
    let Ok(entries) = fs::read_dir(workspace) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("nonaut-undo-store-")
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        if fs::read_dir(&path).is_ok_and(|mut contents| contents.next().is_none()) {
            let _ = fs::remove_dir(path);
        }
    }
}

/// Start the turn snapshot off the inference path. Mutations share its barrier.
pub(crate) fn arm(workspace: &Path, undo: Option<Arc<Mutex<Undo>>>) {
    let armed = ARMED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut armed = armed.lock().unwrap_or_else(PoisonError::into_inner);
    match undo {
        Some(undo) => {
            let turn = Arc::new(TurnSnapshot {
                undo,
                taken: tokio::sync::OnceCell::new(),
            });
            armed.insert(workspace.to_path_buf(), Arc::clone(&turn));
            tokio::spawn(async move {
                take_snapshot(&turn).await;
            });
        }
        None => {
            armed.remove(workspace);
        }
    }
}

pub(crate) async fn checkpoint_before_mutation(workspace: &Path) {
    let turn = ARMED.get().and_then(|armed| {
        armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(workspace)
            .cloned()
    });
    if let Some(turn) = turn {
        take_snapshot(&turn).await;
    }
}

async fn take_snapshot(turn: &TurnSnapshot) {
    turn.taken
        .get_or_init(|| async {
            let undo = Arc::clone(&turn.undo);
            let _ = tokio::task::spawn_blocking(move || {
                undo.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .checkpoint();
            })
            .await;
        })
        .await;
}

impl Drop for Undo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.session_dir);
    }
}

fn state_dir() -> Result<PathBuf> {
    let state_home = match env::var_os("XDG_STATE_HOME") {
        Some(directory) => PathBuf::from(directory),
        None => env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/state"))
            .context("HOME is not set; cannot place undo snapshots")?,
    };
    Ok(state_home.join("nonaut/undo"))
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("nonaut-undo-{name}-{}", unix_nanos()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn undo_reverts_edits_creations_and_deletions_but_not_untracked_dirs() {
        let workspace = temp("ws");
        fs::write(workspace.join("kept.txt"), "original").unwrap();
        fs::write(workspace.join("deleted.txt"), "will be deleted").unwrap();
        fs::create_dir_all(workspace.join("target")).unwrap();

        let mut undo = Undo::open_in(&workspace, &temp("store")).unwrap();
        undo.checkpoint();

        fs::write(workspace.join("kept.txt"), "changed by the agent").unwrap();
        fs::remove_file(workspace.join("deleted.txt")).unwrap();
        fs::write(workspace.join("created.txt"), "new").unwrap();
        fs::write(workspace.join("target/build.bin"), "build output").unwrap();

        let changes = undo.undo().unwrap().unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(
            fs::read_to_string(workspace.join("kept.txt")).unwrap(),
            "original"
        );
        assert!(workspace.join("deleted.txt").exists());
        assert!(!workspace.join("created.txt").exists());
        assert!(workspace.join("target/build.bin").exists());
        assert!(undo.undo().unwrap().is_none());
        fs::remove_dir_all(&workspace).unwrap();
    }

    #[test]
    fn removes_only_empty_legacy_workspace_stores() {
        let workspace = temp("legacy");
        let empty = workspace.join("nonaut-undo-store-1");
        let populated = workspace.join("nonaut-undo-store-2");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(&populated).unwrap();
        fs::write(populated.join("snapshot"), "keep").unwrap();

        remove_empty_legacy_stores(&workspace);

        assert!(!empty.exists());
        assert!(populated.exists());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn armed_turn_snapshots_once_before_mutation() {
        let workspace = temp("lazy");
        let store = temp("lazy-store");
        let file = workspace.join("tracked.txt");
        fs::write(&file, "before").unwrap();
        let undo = Arc::new(Mutex::new(Undo::open_in(&workspace, &store).unwrap()));

        arm(&workspace, Some(Arc::clone(&undo)));
        checkpoint_before_mutation(&workspace).await;
        checkpoint_before_mutation(&workspace).await;
        assert_eq!(undo.lock().unwrap().stack.len(), 1);

        fs::write(&file, "after").unwrap();
        undo.lock().unwrap().undo().unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "before");
        arm(&workspace, None);
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(store).unwrap();
    }
}
