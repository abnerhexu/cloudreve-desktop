//! Durable retry records. Ambiguous remote changes fail closed; never adopt an
//! unrelated file on a name collision or overwrite a newer remote version.
use super::*;
use cloudreve_sync::{
    inventory::InventoryDb,
    uploader::{ProgressCallback, ProgressUpdate, UploadParams, Uploader, UploaderConfig},
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[derive(Debug)]
pub struct Failure(pub &'static str);
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Failure {}
fn fail<T>(kind: &'static str) -> Result<T> {
    Err(Failure(kind).into())
}

#[derive(serde::Serialize, Deserialize, Default)]
struct Journal {
    last: Option<FileResponse>,
    done: bool,
    intent: String,
}
fn metadata(f: &FileResponse) -> String {
    format!("{}\n{}\n{}", f.path, f.updated_at, f.size)
}
fn same_version(f: &FileResponse, content: &str, meta: &str) -> bool {
    f.primary_entity.as_deref().unwrap_or_default() == content && metadata(f) == meta
}
fn filename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0'),
        "invalid filename"
    );
    Ok(())
}
fn save(path: &std::path::Path, journal: &Journal) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(journal)?)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn digest(path: &std::path::Path) -> Result<String> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_file(),
        "not a regular file"
    );
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut block = [0u8; 65536];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
struct UploadProgress(Arc<AtomicU64>);
impl ProgressCallback for UploadProgress {
    fn on_progress(&self, p: ProgressUpdate) {
        self.0
            .store((p.progress * 1000.0) as u64, Ordering::Relaxed);
    }
}
async fn upload(
    client: Arc<Client>,
    r: &Request,
    f: &FileResponse,
    cancel: CancellationToken,
    progress: Arc<AtomicU64>,
) -> Result<FileResponse> {
    let size = std::fs::metadata(&r.local_path)?.len();
    // The upload session's previous_version is checked by the server, rather
    // than relying solely on the preflight metadata read.
    let db = Arc::new(InventoryDb::with_path(
        r.state_directory.join("uploads.sqlite"),
    )?);
    Uploader::new(client.clone(), db, UploaderConfig::default())
        .with_cancel_token(cancel)
        .upload(
            UploadParams {
                local_path: r.local_path.clone(),
                remote_uri: f.path.clone(),
                file_size: size,
                mime_type: None,
                last_modified: None,
                overwrite: true,
                previous_version: f.primary_entity.clone().unwrap_or_default(),
                task_id: uuid::Uuid::new_v4().to_string(),
                drive_id: r.domain.clone(),
            },
            UploadProgress(progress),
        )
        .await?;
    item(&client, r, &f.id).await
}

pub(super) async fn perform(
    client: Arc<Client>,
    r: &Request,
    cancel: CancellationToken,
    progress: Arc<AtomicU64>,
) -> Result<Value> {
    ensure!(
        r.state_directory.is_absolute(),
        "missing private state directory"
    );
    std::fs::create_dir_all(&r.state_directory)?;
    let content_hash = if r.local_path.as_os_str().is_empty() {
        String::new()
    } else {
        digest(&r.local_path)?
    };
    // Excludes tokens and temporary filenames, so token rotation or a new staging
    // URL does not change the replay identity.
    let intent = serde_json::to_string(&json!([
        r.operation,
        r.id,
        r.parent,
        r.name,
        r.version,
        r.metadata_version,
        r.directory,
        content_hash
    ]))?;
    let key_input = if r.operation == "create" {
        ensure!(
            !r.request_id.is_empty(),
            "create requires stable template identifier"
        );
        format!("{}:{}:{}", r.domain, r.operation, r.request_id)
    } else {
        format!("{}:{}", r.domain, intent)
    };
    let key = format!("{:x}", Sha256::digest(key_input.as_bytes()));
    let path = r.state_directory.join(format!("{key}.json"));
    let mut journal: Journal = match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal {
            intent: intent.clone(),
            ..Default::default()
        },
        Err(e) => return Err(e.into()),
    };
    if journal.intent != intent {
        return fail("version_conflict");
    }
    if journal.done {
        return Ok(serde_json::to_value(journal.last)?);
    }
    save(&path, &journal)?;
    let mut f = if r.operation == "create" {
        filename(&r.name)?;
        if let Some(last) = &journal.last {
            item(&client, r, &last.id).await?
        } else {
            let parent = item(&client, r, &r.parent).await?;
            ensure!(parent.file_type == file_type::FOLDER, "not a folder");
            let uri = CrUri::new(&parent.path)?.join(&[&r.name]).to_string();
            let created = client
                .create_file(&CreateFileService {
                    uri: uri.clone(),
                    file_type: if r.directory { "folder" } else { "file" }.into(),
                    err_on_conflict: Some(true),
                    metadata: None,
                })
                .await;
            let f = match created {
                Ok(f) => f,
                // Without a committed server ID, a same-name file cannot be
                // proven to belong to this request. Preserve it and the local
                // pending file rather than guessing after a lost response.
                Err(ApiError::ApiError { code: 40004, .. }) => return fail("collision"),
                Err(e) => return Err(e.into()),
            };
            journal.last = Some(f.clone());
            save(&path, &journal)?;
            f
        }
    } else {
        ensure!(r.id != "root", "cannot mutate root");
        match item(&client, r, &r.id).await {
            Ok(f) => f,
            Err(e) if r.operation == "delete" && error_kind(&e) == "not_found" => {
                journal.done = true;
                save(&path, &journal)?;
                return Ok(Value::Null);
            }
            Err(e) => return Err(e),
        }
    };
    let (expected_content, expected_meta) = match &journal.last {
        Some(last) => (
            last.primary_entity.clone().unwrap_or_default(),
            metadata(last),
        ),
        None => (r.version.clone(), r.metadata_version.clone()),
    };
    if r.operation != "create" || journal.last.is_some() {
        if !same_version(&f, &expected_content, &expected_meta) {
            return fail("version_conflict");
        }
    }
    if r.operation == "delete" {
        // Never recursively remove a directory that may contain unseen edits.
        // File Provider will retry after deleting its children.
        if f.file_type == file_type::FOLDER {
            let listing = client.list_files_all(None, &f.path, 1).await?;
            if !listing.res.files.is_empty() || listing.more {
                return fail("directory_not_empty");
            }
        }
        client
            .delete_files(&DeleteFileService {
                uris: vec![f.path],
                unlink: None,
                skip_soft_delete: Some(false),
            })
            .await?;
        journal.last = None;
        journal.done = true;
        save(&path, &journal)?;
        return Ok(Value::Null);
    }
    if r.operation == "modify" {
        if !r.parent.is_empty() {
            let parent = item(&client, r, &r.parent).await?;
            let old_parent = CrUri::new(&f.path)?.parent()?.to_string();
            if old_parent.trim_end_matches('/') != parent.path.trim_end_matches('/') {
                client
                    .move_files(&MoveFileService {
                        uris: vec![f.path.clone()],
                        dst: parent.path,
                        copy: Some(false),
                    })
                    .await?;
                f = item(&client, r, &r.id).await?;
                journal.last = Some(f.clone());
                save(&path, &journal)?;
            }
        }
        if !r.name.is_empty() && r.name != f.name {
            filename(&r.name)?;
            f = client
                .rename_file(&RenameFileService {
                    uri: f.path,
                    new_name: r.name.clone(),
                })
                .await?;
            journal.last = Some(f.clone());
            save(&path, &journal)?;
        }
    }
    if !r.local_path.as_os_str().is_empty() {
        ensure!(
            f.file_type != file_type::FOLDER,
            "cannot write folder content"
        );
        f = upload(client, r, &f, cancel, progress).await?;
        ensure!(
            digest(&r.local_path)? == content_hash,
            "local content changed during upload"
        );
    }
    journal.last = Some(f.clone());
    journal.done = true;
    save(&path, &journal)?;
    Ok(serde_json::to_value(f)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journal_is_durable_and_rejects_invalid_names() {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path().join("journal.json");
        save(
            &p,
            &Journal {
                done: true,
                intent: "retry-key".into(),
                last: None,
            },
        )
        .unwrap();
        let loaded: Journal = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        assert!(loaded.done);
        assert_eq!(loaded.intent, "retry-key");
        for name in ["", ".", "..", "a/b", "a\0b"] {
            assert!(filename(name).is_err());
        }
    }
}
