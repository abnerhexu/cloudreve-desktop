use crate::drive::{
    mounts::Mount,
    sync::{GroupedFsEvents, SyncMode},
    utils::local_path_to_cr_uri,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use cloudreve_api::{
    api::ExplorerApi,
    models::{explorer::metadata, user::Token},
};
use std::path::PathBuf;
use tokio::sync::oneshot::Sender;
#[derive(Debug)]
pub enum MountCommand {
    RefreshCredentials {
        credentials: Token,
    },
    CredentialInvalid,
    ProcessFsEvents {
        events: GroupedFsEvents,
    },
    Sync {
        local_paths: Vec<PathBuf>,
        mode: SyncMode,
        user_initiated: bool,
    },
}
/// Commands for the DriveManager
/// These can be sent from external sources like context menus or other UI components
#[derive(Debug)]
pub enum ManagerCommand {
    /// View a file or folder online in the web interface
    ViewOnline {
        path: PathBuf,
    },
    PersistConfig,
    GenerateThumbnail {
        path: PathBuf,
        response: Sender<Result<Bytes>>,
    },
    SyncNow {
        paths: Vec<PathBuf>,
        mode: SyncMode,
    },
    ResolveConflict {
        drive_id: String,
        file_id: i64,
        path: String,
        action: ConflictAction,
    },
    /// Show conflict resolution toast for a file
    ShowConflictToast {
        path: PathBuf,
    },
    /// Get drive status UI by sync root ID
    GetDriveStatusUI {
        syncroot_id: String,
        response: Sender<Result<Option<crate::drive::manager::DriveStatusUI>>>,
    },
    /// Open user profile URL in browser
    OpenProfileUrl {
        syncroot_id: String,
    },
    /// Open storage/capacity details URL in browser
    OpenStorageDetailsUrl {
        syncroot_id: String,
    },
    /// Request to open the sync status window in the UI
    OpenSyncStatusWindow,
    /// Request to open the settings window in the UI
    OpenSettingsWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    KeepRemote,
    OverwriteRemote,
    SaveAsNew,
}

impl ConflictAction {
    pub fn from_str(action: &str) -> Option<Self> {
        match action {
            "keep_remote" => Some(Self::KeepRemote),
            "overwrite_remote" => Some(Self::OverwriteRemote),
            "save_as_new" => Some(Self::SaveAsNew),
            _ => None,
        }
    }
}

impl Mount {
    pub async fn generate_thumbnail(&self, path: PathBuf) -> Result<Bytes> {
        let file_meta = self
            .inventory
            .query_by_path(path.to_str().unwrap_or(""))
            .context("failed to query metadata by path")?
            .ok_or_else(|| anyhow::anyhow!("no metadata found for path: {:?}", path))?;

        if file_meta.is_folder
            || file_meta
                .metadata
                .get(metadata::THUMBNAIL_DISABLED)
                .is_some()
        {
            return Err(anyhow::anyhow!("thumbnail disabled for path: {:?}", path));
        }

        let (sync_path, remote_base) = {
            let config = self.config.read().await;
            (config.sync_path.clone(), config.remote_path.to_string())
        };
        let uri = local_path_to_cr_uri(path.clone(), sync_path, remote_base)
            .context("failed to convert local path to cloudreve uri")?
            .to_string();
        let thumb_res = self.cr_client.get_file_thumb(uri.as_str(), None).await?;

        // Download the thumbnail
        let thumb_url = thumb_res.url;
        tracing::trace!(target: "drive::commands", thumb_url = %thumb_url, "Thumbnail URL");
        let thumb_response = reqwest::get(thumb_url).await?;
        // Make sure the response is successful
        if !thumb_response.status().is_success() {
            return Err(anyhow::anyhow!(
                "failed to download thumbnail: {:?}",
                thumb_response.status()
            ));
        }
        Ok(thumb_response.bytes().await?)
    }

    pub async fn process_fs_events(&self, events: GroupedFsEvents) -> Result<()> {
        if events
            .keys()
            .all(|kind| matches!(kind, notify_debouncer_full::notify::EventKind::Access(_)))
        {
            return Ok(());
        }
        self.sync_paths(vec![], SyncMode::FullHierarchy).await
    }
    pub async fn resolve_conflict(
        &self,
        _action: ConflictAction,
        _file_id: i64,
        _path: String,
    ) -> Result<()> {
        anyhow::bail!(
            "macOS keeps conflicting file content in a separate conflict copy. Resolve it in Finder, then sync again."
        )
    }
}
