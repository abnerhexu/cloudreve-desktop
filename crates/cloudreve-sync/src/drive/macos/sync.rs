//! Snapshot reconciliation for fully downloaded macOS folders.
use crate::{
    drive::{
        mounts::Mount,
        placeholder::{Fingerprint, baseline, fingerprint, metadata, validate_path},
        utils::local_path_to_cr_uri,
    },
    inventory::{FileMetadata, MetadataEntry},
    tasks::TaskPayload,
};
use anyhow::{Context, Result, ensure};
use cloudreve_api::{
    api::{ExplorerApi, explorer::ExplorerApiExt},
    models::explorer::{CreateFileService, DeleteFileService, FileResponse, file_type},
};
use notify_debouncer_full::{
    DebouncedEvent,
    notify::{Event, EventKind},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    PathOnly,
    PathAndFirstLayer,
    FullHierarchy,
}
pub type GroupedFsEvents = HashMap<EventKind, Vec<Event>>;
pub fn group_fs_events(events: Vec<DebouncedEvent>) -> GroupedFsEvents {
    let mut grouped = HashMap::new();
    for event in events {
        grouped
            .entry(event.event.kind)
            .or_insert_with(Vec::new)
            .push(event.event);
    }
    grouped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Upload,
    Download,
    DeleteLocal,
    DeleteRemote,
    Forget,
    Conflict,
}

/// All absences are authoritative only after both complete scans succeed.
fn decide(
    local: Option<&Fingerprint>,
    remote: bool,
    base: Option<&Fingerprint>,
    remote_changed: bool,
) -> Action {
    match (local, remote, base) {
        (None, false, _) => Action::Forget,
        (Some(_), false, None) => Action::Upload,
        (None, true, None) => Action::Download,
        (Some(_), true, None) => Action::Conflict,
        (None, true, Some(_)) => {
            if remote_changed {
                Action::Conflict
            } else {
                Action::DeleteRemote
            }
        }
        (Some(local), false, Some(base)) => {
            if local == base {
                Action::DeleteLocal
            } else {
                Action::Conflict
            }
        }
        (Some(local), true, Some(base)) => match (local != base, remote_changed) {
            (false, false) => Action::None,
            (true, false) => Action::Upload,
            (false, true) => Action::Download,
            (true, true) => Action::Conflict,
        },
    }
}

fn changed(remote: &FileResponse, base: &FileMetadata) -> bool {
    remote.primary_entity.as_deref().unwrap_or_default() != base.etag
        || remote.size != base.size
        || (remote.file_type == file_type::FOLDER) != base.is_folder
        || base
            .props
            .as_ref()
            .and_then(|p| p.get("remote_updated_at"))
            .and_then(|v| v.as_str())
            != Some(remote.updated_at.as_str())
        || base
            .props
            .as_ref()
            .and_then(|p| p.get("remote_id"))
            .and_then(|v| v.as_str())
            != Some(remote.id.as_str())
}

pub fn internal_name(name: &str) -> bool {
    name == ".DS_Store" || name.starts_with(".cloudreve-tmp-")
}

fn safe_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0'),
        "Unsafe remote filename"
    );
    Ok(())
}

impl Mount {
    pub async fn sync_paths(&self, paths: Vec<PathBuf>, mode: SyncMode) -> Result<()> {
        let result = self.reconcile(paths, mode).await;
        *self.last_sync_error.lock().await = result.as_ref().err().map(|e| format!("{e:#}"));
        result
    }
    async fn reconcile(&self, _paths: Vec<PathBuf>, _mode: SyncMode) -> Result<()> {
        // A single complete reconciliation also repairs missed/ambiguous watcher events.
        let _guard = self.sync_lock.lock().await;
        let config = self.get_config().await;
        if !config.enabled {
            return Ok(());
        }
        let root = &config.sync_path;
        validate_path(root, root)?;
        ensure!(
            std::fs::symlink_metadata(root)?.is_dir(),
            "Sync root is unavailable"
        );
        let drive_id = Uuid::parse_str(&self.id)?;
        tracing::info!(drive_id = %self.id, "Starting automatic folder reconciliation");
        let mut progressed = BTreeSet::new();
        let mut remote = BTreeMap::<PathBuf, FileResponse>::new();
        let mut dirs = VecDeque::from([root.clone()]);
        while let Some(dir) = dirs.pop_front() {
            let uri = local_path_to_cr_uri(dir.clone(), root.clone(), config.remote_path.clone())?
                .to_string();
            let mut previous = None;
            let mut children = BTreeMap::new();
            loop {
                // Never interpret listing/authentication/network failures as empty folders.
                let page = self
                    .cr_client
                    .list_files_all(previous.as_ref(), &uri, 1000)
                    .await?;
                for file in &page.res.files {
                    safe_name(&file.name)?;
                    let path = dir.join(&file.name);
                    if internal_name(&file.name) || self.is_ignored(&path).await {
                        continue;
                    }
                    validate_path(root, &path)?;
                    ensure!(
                        !remote.contains_key(&path),
                        "Duplicate remote listing entry"
                    );
                    if file.file_type == file_type::FOLDER {
                        dirs.push_back(path.clone());
                    }
                    children.insert(path.clone(), file.clone());
                    remote.insert(path, file.clone());
                }
                let more = page.more;
                previous = Some(page);
                if !more {
                    break;
                }
            }
            // Only a complete directory listing authorizes non-destructive
            // transfers here. A slow/failing descendant must not block siblings.
            let new_folders = self.reconcile_directory(root, &dir, &config.remote_path,
                &children, drive_id, &mut progressed).await?;
            for (path, file) in new_folders {
                dirs.push_back(path.clone());
                remote.insert(path, file);
            }
        }
        // Transfers were allowed to finish while scanning remote descendants.
        // Re-snapshot local state and inventory under their shared lock before
        // interpreting any absence as deletion. Never use the pre-transfer state.
        let _filesystem_guard = self.task_queue.filesystem_lock.lock().await;
        let mut local = BTreeMap::new();
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Non-UTF8 filename"))?;
                if internal_name(&name) || self.is_ignored(&path).await {
                    continue;
                }
                validate_path(root, &path)?;
                let scan_path = path.clone();
                let fp = tokio::task::spawn_blocking(move || fingerprint(&scan_path))
                    .await??
                    .context("File disappeared during scan; retrying later")?;
                if fp.directory {
                    dirs.push(path.clone());
                }
                local.insert(path, fp);
            }
        }
        // Conservatively reject aliases before writing anything on case-insensitive APFS.
        let mut names = HashMap::new();
        for path in local.keys().chain(remote.keys()) {
            use unicode_normalization::UnicodeNormalization;
            let key: String = path
                .to_str()
                .context("Invalid filename")?
                .nfd()
                .flat_map(char::to_lowercase)
                .collect();
            if let Some(other) = names.insert(key, path) {
                ensure!(
                    other == path,
                    "Filename collision: {} and {}",
                    other.display(),
                    path.display()
                );
            }
        }
        let inventory: BTreeMap<PathBuf, FileMetadata> = self
            .inventory
            .query_by_drive(&self.id)?
            .into_iter()
            .map(|m| (PathBuf::from(&m.local_path), m))
            .collect();
        let paths: BTreeSet<_> = local
            .keys()
            .chain(remote.keys())
            .chain(inventory.keys())
            .cloned()
            .collect();
        // A directory fingerprint intentionally ignores children. Check the subtree
        // separately before allowing removal of a directory on either side.
        for (path, old) in &inventory {
            if !old.is_folder || self.is_ignored(path).await {
                continue;
            }
            if local.contains_key(path) && !remote.contains_key(path) {
                let dirty = local
                    .iter()
                    .filter(|(p, _)| *p != path && p.starts_with(path))
                    .any(|(p, fp)| inventory.get(p).and_then(baseline).as_ref() != Some(fp));
                ensure!(
                    !dirty,
                    "Remote folder was deleted but local children changed: {}; move the local folder to a new name to preserve it",
                    path.display()
                );
            }
            if !local.contains_key(path) && remote.contains_key(path) {
                let dirty = remote
                    .iter()
                    .filter(|(p, _)| *p != path && p.starts_with(path))
                    .any(|(p, file)| inventory.get(p).is_none_or(|old| changed(file, old)));
                ensure!(
                    !dirty,
                    "Local folder was deleted but remote children changed: {}; restore the local folder or resolve the cloud folder manually",
                    path.display()
                );
            }
        }
        let active = self.task_queue.list_active_tasks()?;
        let mut deletes = Vec::new();
        for path in paths {
            // In particular, a just-uploaded file may not be in the earlier
            // remote snapshot. Reconcile it on the next pass, never delete it.
            if progressed.contains(&path) { continue; }
            validate_path(root, &path)?;
            if self.is_ignored(&path).await
                || path
                    .components()
                    .any(|c| internal_name(&c.as_os_str().to_string_lossy()))
            {
                continue;
            }
            if active.iter().any(|t| Path::new(&t.local_path) == path) {
                continue;
            }
            let l = local.get(&path);
            let r = remote.get(&path);
            let inv = inventory.get(&path);
            let base = inv.and_then(baseline);
            let remote_changed = r.zip(inv).is_some_and(|(r, b)| changed(r, b));
            // Existing directories are containers, not competing versions of file content.
            if l.is_some_and(|f| f.directory) && r.is_some_and(|r| r.file_type == file_type::FOLDER)
            {
                self.inventory.upsert(&MetadataEntry::from(&metadata(
                    r.unwrap(),
                    &path,
                    drive_id,
                    l,
                )))?;
                continue;
            }
            let action = decide(l, r.is_some(), base.as_ref(), remote_changed);
            match action {
                Action::None => {}
                Action::Forget => {
                    self.inventory
                        .batch_delete_by_path(vec![path.to_str().context("Invalid path")?])?;
                }
                Action::Upload => {
                    if l.is_some_and(|f| f.directory) {
                        let uri = local_path_to_cr_uri(
                            path.clone(),
                            root.clone(),
                            config.remote_path.clone(),
                        )?
                        .to_string();
                        let file = self
                            .cr_client
                            .create_file(&CreateFileService {
                                uri,
                                file_type: "folder".into(),
                                err_on_conflict: Some(true),
                                metadata: None,
                            })
                            .await?;
                        self.inventory
                            .upsert(&MetadataEntry::from(&metadata(&file, &path, drive_id, l)))?;
                    } else {
                        self.queue_transfer(&path, true, l).await?;
                    }
                }
                Action::Download => {
                    let r = r.unwrap();
                    if r.file_type == file_type::FOLDER {
                        ensure!(l.is_none(), "File/directory conflict: {}", path.display());
                        std::fs::create_dir(&path)?;
                        let fp = fingerprint(&path)?;
                        self.inventory.upsert(&MetadataEntry::from(&metadata(
                            r,
                            &path,
                            drive_id,
                            fp.as_ref(),
                        )))?;
                    } else {
                        self.queue_transfer(&path, false, l).await?;
                    }
                }
                Action::DeleteLocal | Action::DeleteRemote => deletes.push((path, action)),
                Action::Conflict => {
                    // Preserve both versions automatically. Directory conflicts require intervention.
                    ensure!(
                        !l.is_some_and(|f| f.directory)
                            && !r.is_some_and(|r| r.file_type == file_type::FOLDER),
                        "Directory conflict at {}; no content was removed",
                        path.display()
                    );
                    if l.is_some() {
                        ensure!(
                            fingerprint(&path)?.as_ref() == l,
                            "File changed during reconciliation"
                        );
                        let name = path
                            .file_name()
                            .context("Missing filename")?
                            .to_string_lossy();
                        let copy =
                            path.with_file_name(format!("{} (conflict {})", name, Uuid::new_v4()));
                        std::fs::rename(&path, &copy)?;
                        crate::utils::toast::send_warning_toast(
                            "Sync conflict",
                            &format!("Local version preserved as {}", copy.display()),
                        );
                    }
                    self.inventory
                        .batch_delete_by_path(vec![path.to_str().context("Invalid path")?])?;
                    if r.is_some() {
                        self.queue_transfer(&path, false, None).await?;
                    }
                }
            }
        }
        // Children first. Never recursively delete a local folder or a remote folder with new children.
        deletes.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        for (path, action) in deletes {
            validate_path(root, &path)?;
            match action {
                Action::DeleteLocal => {
                    ensure!(
                        fingerprint(&path)?.as_ref() == local.get(&path),
                        "Local file changed before deletion"
                    );
                    if local[&path].directory {
                        if std::fs::read_dir(&path)?.next().is_some() {
                            continue;
                        }
                        std::fs::remove_dir(&path)?;
                    } else {
                        std::fs::remove_file(&path)?;
                    }
                }
                Action::DeleteRemote => {
                    ensure!(
                        fingerprint(&path)?.is_none(),
                        "Local file reappeared before deletion"
                    );
                    let uri = local_path_to_cr_uri(
                        path.clone(),
                        root.clone(),
                        config.remote_path.clone(),
                    )?
                    .to_string();
                    let latest = self
                        .cr_client
                        .get_file_info(&cloudreve_api::models::explorer::GetFileInfoService {
                            uri: Some(uri.clone()),
                            id: None,
                            extended: None,
                            folder_summary: None,
                        })
                        .await?;
                    ensure!(
                        !changed(&latest, &inventory[&path]),
                        "Remote file changed before deletion"
                    );
                    if latest.file_type == file_type::FOLDER {
                        let page = self.cr_client.list_files_all(None, &uri, 1).await?;
                        if !page.res.files.is_empty() || page.more {
                            continue;
                        }
                    }
                    self.cr_client
                        .delete_files(&DeleteFileService {
                            uris: vec![uri],
                            unlink: None,
                            skip_soft_delete: Some(false),
                        })
                        .await?;
                }
                _ => unreachable!(),
            }
            self.inventory
                .batch_delete_by_path(vec![path.to_str().context("Invalid path")?])?;
        }
        Ok(())
    }

    /// Reconcile safe transfers once one directory's complete listing is known.
    /// Deletion, forgetting baselines and conflict moves remain in the full scan.
    async fn reconcile_directory(
        &self,
        root: &Path,
        dir: &Path,
        remote_root: &str,
        remote: &BTreeMap<PathBuf, FileResponse>,
        drive_id: Uuid,
        progressed: &mut BTreeSet<PathBuf>,
    ) -> Result<Vec<(PathBuf, FileResponse)>> {
        let _filesystem_guard = self.task_queue.filesystem_lock.lock().await;
        validate_path(root, dir)?;
        // A locally deleted directory must not be recreated by its children.
        if !dir.is_dir() { return Ok(Vec::new()); }
        let mut local = BTreeMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path.file_name().context("Missing filename")?.to_str().context("Non-UTF8 filename")?;
            if internal_name(name) || self.is_ignored(&path).await { continue; }
            validate_path(root, &path)?;
            let scan_path = path.clone();
            let fp = tokio::task::spawn_blocking(move || fingerprint(&scan_path)).await??
                .context("File disappeared during scan; retrying later")?;
            local.insert(path, fp);
        }
        // Validate all siblings, across all pages, before writing any of them.
        let mut names = HashMap::new();
        for path in local.keys().chain(remote.keys()) {
            use unicode_normalization::UnicodeNormalization;
            let key: String = path.to_str().context("Invalid filename")?.nfd().flat_map(char::to_lowercase).collect();
            if let Some(other) = names.insert(key, path) {
                ensure!(other == path, "Filename collision: {} and {}", other.display(), path.display());
            }
        }
        let active = self.task_queue.list_active_tasks()?;
        let paths: BTreeSet<_> = local.keys().chain(remote.keys()).cloned().collect();
        let mut created = Vec::new();
        for path in paths {
            if active.iter().any(|task| Path::new(&task.local_path) == path) { continue; }
            let l = local.get(&path);
            let r = remote.get(&path);
            let old = self.inventory.query_by_path(path.to_str().context("Invalid path")?)?;
            let base = old.as_ref().and_then(baseline);
            if l.is_some_and(|f| f.directory) && r.is_some_and(|f| f.file_type == file_type::FOLDER) { continue; }
            let remote_changed = r.zip(old.as_ref()).is_some_and(|(r, b)| changed(r, b));
            match decide(l, r.is_some(), base.as_ref(), remote_changed) {
                Action::Download => {
                    let file = r.unwrap();
                    if file.file_type == file_type::FOLDER {
                        ensure!(l.is_none(), "File/directory conflict: {}", path.display());
                        std::fs::create_dir(&path)?;
                        let fp = fingerprint(&path)?;
                        self.inventory.upsert(&MetadataEntry::from(&metadata(file, &path, drive_id, fp.as_ref())))?;
                    } else {
                        self.queue_transfer(&path, false, l).await?;
                    }
                    progressed.insert(path);
                }
                Action::Upload => {
                    if l.is_some_and(|f| f.directory) {
                        let uri = local_path_to_cr_uri(path.clone(), root.to_path_buf(), remote_root.to_string())?.to_string();
                        let file = self.cr_client.create_file(&CreateFileService {
                            uri, file_type: "folder".into(), err_on_conflict: Some(true), metadata: None,
                        }).await?;
                        self.inventory.upsert(&MetadataEntry::from(&metadata(&file, &path, drive_id, l)))?;
                        created.push((path.clone(), file));
                    } else {
                        self.queue_transfer(&path, true, l).await?;
                    }
                    progressed.insert(path);
                }
                _ => {},
            }
        }
        Ok(created)
    }

    async fn queue_transfer(
        &self,
        path: &Path,
        upload: bool,
        expected: Option<&Fingerprint>,
    ) -> Result<()> {
        let payload = if upload {
            TaskPayload::upload(path)
        } else {
            TaskPayload::download(path)
        };
        self.task_queue
            .enqueue(payload.with_custom_state(serde_json::json!({"macos_expected":expected})))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(digest: &str) -> Fingerprint {
        Fingerprint {
            directory: false,
            size: 1,
            digest: digest.into(),
        }
    }
    #[test]
    fn three_way_changes_and_deletions() {
        let old = file("old");
        let new = file("new");
        assert_eq!(decide(Some(&old), true, Some(&old), false), Action::None);
        assert_eq!(decide(Some(&new), true, Some(&old), false), Action::Upload);
        assert_eq!(decide(Some(&old), true, Some(&old), true), Action::Download);
        assert_eq!(decide(Some(&new), true, Some(&old), true), Action::Conflict);
        assert_eq!(decide(None, true, Some(&old), false), Action::DeleteRemote);
        assert_eq!(decide(None, true, Some(&old), true), Action::Conflict);
        assert_eq!(
            decide(Some(&old), false, Some(&old), false),
            Action::DeleteLocal
        );
        assert_eq!(
            decide(Some(&new), false, Some(&old), false),
            Action::Conflict
        );
        assert_eq!(decide(Some(&old), true, None, false), Action::Conflict);
        assert_eq!(decide(None, true, None, false), Action::Download);
        assert_eq!(decide(Some(&old), false, None, false), Action::Upload);
        assert_eq!(decide(None, false, Some(&old), false), Action::Forget);
    }
    #[test]
    fn rejects_remote_traversal() {
        for name in ["", ".", "..", "../outside", "a/b", "a\0b"] {
            assert!(safe_name(name).is_err());
        }
        assert!(safe_name("a\\b").is_ok());
    }
}

#[cfg(test)]
#[path = "integration_tests.rs"]
mod integration_tests;
