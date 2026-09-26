//! Ordinary files on macOS. No cloud placeholders or hydration states are emulated.
use crate::inventory::{FileMetadata, InventoryDb, MetadataEntry};
use anyhow::{Context, Result, ensure};
use cloudreve_api::models::explorer::{FileResponse, file_type};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub directory: bool,
    pub size: u64,
    pub digest: String,
}

/// Refuse symlinks anywhere below the root, including ancestors of a missing leaf.
pub fn validate_path(root: &Path, path: &Path) -> Result<()> {
    ensure!(root.is_absolute(), "Sync root must be absolute");
    let relative = path.strip_prefix(root).context("Path outside sync root")?;
    let mut current = PathBuf::new();
    for component in root.components().chain(relative.components()) {
        ensure!(
            !matches!(component, Component::ParentDir | Component::CurDir),
            "Unsafe path component"
        );
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) => ensure!(
                !meta.file_type().is_symlink(),
                "Symlinks are not supported: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn fingerprint(path: &Path) -> Result<Option<Fingerprint>> {
    let before = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        before.is_file() || before.is_dir(),
        "Only regular files and directories can sync: {}",
        path.display()
    );
    if before.is_dir() {
        return Ok(Some(Fingerprint {
            directory: true,
            size: 0,
            digest: String::new(),
        }));
    }
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let after = fs::symlink_metadata(path)?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "File changed while scanning"
    );
    Ok(Some(Fingerprint {
        directory: false,
        size: after.len(),
        digest: format!("{:x}", hash.finalize()),
    }))
}

pub fn baseline(meta: &FileMetadata) -> Option<Fingerprint> {
    meta.props
        .as_ref()?
        .get("macos_sync_v1")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

pub fn metadata(
    file: &FileResponse,
    path: &Path,
    drive_id: Uuid,
    snapshot: Option<&Fingerprint>,
) -> FileMetadata {
    FileMetadata {
        id: 0,
        drive_id,
        local_path: path.to_string_lossy().into_owned(),
        is_folder: file.file_type == file_type::FOLDER,
        created_at: chrono::DateTime::parse_from_rfc3339(&file.created_at)
            .map(|v| v.timestamp())
            .unwrap_or_default(),
        updated_at: chrono::DateTime::parse_from_rfc3339(&file.updated_at)
            .map(|v| v.timestamp())
            .unwrap_or_default(),
        size: file.size,
        etag: file.primary_entity.clone().unwrap_or_default(),
        metadata: file.metadata.clone().unwrap_or_default(),
        props: Some(
            serde_json::json!({"macos_sync_v1": snapshot, "remote_id": file.id, "remote_updated_at": file.updated_at}),
        ),
        permissions: file.permission.clone().unwrap_or_default(),
        shared: file.shared.unwrap_or(false),
        conflict_state: None,
    }
}

#[derive(Clone)]
pub struct LocalFileInfo {
    pub exists: bool,
    pub is_directory: bool,
    pub file_size: Option<u64>,
    pub last_modified: Option<SystemTime>,
    pub snapshot: Option<Fingerprint>,
}
impl LocalFileInfo {
    pub fn from_path(path: &Path) -> Result<Self> {
        let snapshot = fingerprint(path)?;
        Ok(Self {
            exists: snapshot.is_some(),
            is_directory: snapshot.as_ref().is_some_and(|s| s.directory),
            file_size: snapshot.as_ref().map(|s| s.size),
            last_modified: fs::metadata(path).ok().and_then(|m| m.modified().ok()),
            snapshot,
        })
    }
}

/// Adapter for the shared transfer pipeline; commits only fully present content.
pub struct LocalFile {
    pub local_file_info: LocalFileInfo,
    local_path: PathBuf,
    sync_root: PathBuf,
    drive_id: Uuid,
    remote: Option<FileResponse>,
    read_error: Option<String>,
}
pub type CrPlaceholder = LocalFile;
impl LocalFile {
    pub fn new(path: impl Into<PathBuf>, root: PathBuf, drive_id: Uuid) -> Self {
        let path = path.into();
        let result = validate_path(&root, &path).and_then(|_| LocalFileInfo::from_path(&path));
        let (info, read_error) = match result {
            Ok(info) => (info, None),
            Err(error) => (
                LocalFileInfo {
                    exists: true,
                    is_directory: false,
                    file_size: None,
                    last_modified: None,
                    snapshot: None,
                },
                Some(error.to_string()),
            ),
        };
        Self {
            local_path: path,
            sync_root: root,
            drive_id,
            local_file_info: info,
            remote: None,
            read_error,
        }
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(error) = &self.read_error {
            anyhow::bail!("{error}");
        }
        validate_path(&self.sync_root, &self.local_path)
    }
    pub fn with_mark_no_children(self, _: bool) -> Self {
        self
    }
    pub fn with_remote_file(mut self, remote: &FileResponse) -> Self {
        self.remote = Some(remote.clone());
        self
    }
    pub fn update_sync_error_state(&self, _: bool) -> Result<()> {
        Ok(())
    }
    pub fn commit(&mut self, inventory: Arc<InventoryDb>) -> Result<()> {
        self.validate()?;
        let current = fingerprint(&self.local_path)?;
        ensure!(current.is_some(), "Cannot commit absent content");
        ensure!(
            current == self.local_file_info.snapshot,
            "Local content changed during transfer; preserving unsynced changes"
        );
        let remote = self.remote.as_ref().context("Remote metadata missing")?;
        ensure!(
            current.as_ref().unwrap().directory == (remote.file_type == file_type::FOLDER),
            "File type changed during transfer"
        );
        if remote.file_type != file_type::FOLDER {
            ensure!(
                current.as_ref().unwrap().size == remote.size as u64,
                "Transfer size mismatch"
            );
        }
        inventory.upsert(&MetadataEntry::from(&metadata(
            remote,
            &self.local_path,
            self.drive_id,
            current.as_ref(),
        )))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprints_detect_same_size_edits_and_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, b"one").unwrap();
        let first = fingerprint(&path).unwrap();
        fs::write(&path, b"two").unwrap();
        assert_ne!(first, fingerprint(&path).unwrap());
        let encoded = serde_json::to_string(&first).unwrap();
        assert_eq!(first, serde_json::from_str(&encoded).unwrap());
    }
    #[test]
    fn rejects_symlinks_traversal_and_missing_content_commit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("link")).unwrap();
        assert!(validate_path(&root, &root.join("link/child")).is_err());
        assert!(validate_path(&root, &root.join("../outside")).is_err());
        assert!(fingerprint(&root.join("link")).is_err());
        let db = Arc::new(InventoryDb::with_path(root.join("db.sqlite")).unwrap());
        let mut file = LocalFile::new(root.join("missing"), root.clone(), Uuid::new_v4());
        assert!(file.commit(db).is_err());
    }
    #[test]
    fn commit_refuses_content_changed_during_upload() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("file");
        fs::write(&path, b"old").unwrap();
        let mut file = LocalFile::new(&path, root.clone(), Uuid::new_v4());
        fs::write(&path, b"new").unwrap();
        let db = Arc::new(InventoryDb::with_path(root.join("db.sqlite")).unwrap());
        assert!(
            file.commit(db.clone())
                .unwrap_err()
                .to_string()
                .contains("changed during transfer")
        );
        assert!(db.query_by_path(path.to_str().unwrap()).unwrap().is_none());
    }
}
