//! JSON C ABI used by the sandboxed Swift File Provider extension.
//! No process-global drive state, logging subscriber, or desktop config access.
use anyhow::{Result, ensure};
use cloudreve_api::{
    Client, ClientConfig,
    api::{ExplorerApi, explorer::ExplorerApiExt},
    error::ApiError,
    models::{explorer::*, uri::CrUri, user::Token},
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_char},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
mod mutations;

#[derive(Deserialize, serde::Serialize)]
struct Request {
    server: String,
    root: String,
    domain: String,
    tokens: Token,
    operation: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    local_path: PathBuf,
    #[serde(default)]
    parent: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    metadata_version: String,
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    state_directory: PathBuf,
}

/// Opaque ownership: create -> run once -> free. cancel may overlap run, but not free.
pub struct Operation {
    input: String,
    cancel: CancellationToken,
    progress: Arc<AtomicU64>,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_create(input: *const c_char) -> *mut Operation {
    if input.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(input) = (unsafe { CStr::from_ptr(input) }).to_str() else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(Operation {
        input: input.into(),
        cancel: CancellationToken::new(),
        progress: Arc::new(AtomicU64::new(0)),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_progress(op: *mut Operation) -> u64 {
    (unsafe { op.as_ref() })
        .map(|o| o.progress.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_cancel(op: *mut Operation) {
    if let Some(op) = unsafe { op.as_ref() } {
        op.cancel.cancel();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_run(op: *mut Operation) -> *mut c_char {
    let result = std::panic::catch_unwind(|| {
        let Some(op) = (unsafe { op.as_ref() }) else {
            return json!({"error":"invalid_request"});
        };
        static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RUNTIME
            .get_or_init(|| tokio::runtime::Runtime::new().expect("runtime"))
            .block_on(execute(&op.input, op.cancel.clone(), op.progress.clone()))
    })
    .unwrap_or_else(|_| json!({"error":"internal"}));
    CString::new(result.to_string()).unwrap().into_raw()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_free(op: *mut Operation) {
    if !op.is_null() {
        drop(unsafe { Box::from_raw(op) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn crfp_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

fn in_scope(root: &str, uri: &str) -> Result<()> {
    let root = CrUri::new(root)?;
    let uri = CrUri::new(uri)?;
    ensure!(
        root.base(true) == uri.base(true) && uri.elements().starts_with(&root.elements()),
        "out of scope"
    );
    ensure!(
        uri.elements().iter().all(|p| !p.is_empty()
            && p != "."
            && p != ".."
            && !p.contains('/')
            && !p.contains('\0')),
        "invalid path"
    );
    Ok(())
}

async fn item(client: &Client, r: &Request, id: &str) -> Result<FileResponse> {
    let f = client
        .get_file_info(&GetFileInfoService {
            uri: (id == "root").then(|| r.root.clone()),
            id: (id != "root").then(|| id.to_string()),
            ..Default::default()
        })
        .await?;
    in_scope(&r.root, &f.path)?;
    Ok(f)
}

fn error_kind(error: &anyhow::Error) -> &'static str {
    if let Some(e) = error.downcast_ref::<mutations::Failure>() {
        return e.0;
    }
    match error.downcast_ref::<ApiError>() {
        Some(ApiError::ApiError { code: 404, .. }) => "not_found",
        Some(ApiError::ApiError { code: 40076, .. }) => "version_conflict",
        Some(ApiError::ApiError { code: 40004, .. }) => "collision",
        Some(ApiError::ApiError { code: 403, .. }) => "permission",
        Some(
            ApiError::LoginRequired(_)
            | ApiError::RefreshTokenExpired
            | ApiError::AccessTokenExpired
            | ApiError::NoTokensAvailable
            | ApiError::InvalidToken(_),
        ) => "not_authenticated",
        Some(ApiError::RequestError(_)) => "unreachable",
        _ => "internal", // Never infer deletion from a generic server failure.
    }
}

async fn execute(input: &str, cancel: CancellationToken, progress: Arc<AtomicU64>) -> Value {
    let Ok(r) = serde_json::from_str::<Request>(input) else {
        return json!({"error":"invalid_request"});
    };
    let refreshed = Arc::new(Mutex::new(r.tokens.clone()));
    let mut client = Client::new(
        ClientConfig::new(&r.server)
            .with_client_id(&r.domain)
            .with_timeout(60),
    );
    if client.set_tokens_with_expiry(&r.tokens).await.is_err() {
        return json!({"error":"not_authenticated"});
    }
    let tokens = refreshed.clone();
    client.set_on_credential_refreshed(Arc::new(move |token| {
        *tokens.lock().unwrap() = token;
        Box::pin(async {})
    }));
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => json!({"error":"cancelled"}),
        result = perform(Arc::new(client), &r, cancel.clone(), progress) => match result {
            Ok(value) => json!({"data":value}),
            Err(e) => json!({"error":error_kind(&e), "api_code": match e.downcast_ref::<ApiError>() {
                Some(ApiError::ApiError { code, .. }) => Some(*code), _ => None,
            }}),
        }
    };
    let mut result = result;
    result["tokens"] = serde_json::to_value(&*refreshed.lock().unwrap()).unwrap();
    result
}

async fn perform(
    client: Arc<Client>,
    r: &Request,
    cancel: CancellationToken,
    progress: Arc<AtomicU64>,
) -> Result<Value> {
    in_scope(&r.root, &r.root)?;
    match r.operation.as_str() {
        "item" => Ok(serde_json::to_value(item(&client, r, &r.id).await?)?),
        "list" => {
            let parent = item(&client, r, &r.id).await?;
            ensure!(parent.file_type == file_type::FOLDER, "not folder");
            let mut previous = None;
            let mut files = Vec::new();
            loop {
                let page = client
                    .list_files_all(previous.as_ref(), &parent.path, 200)
                    .await?;
                for f in &page.res.files {
                    in_scope(&r.root, &f.path)?;
                }
                files.extend(page.res.files.clone());
                if !page.more {
                    break;
                }
                previous = Some(page);
            }
            Ok(serde_json::to_value(files)?)
        }
        "fetch" => {
            let f = item(&client, r, &r.id).await?;
            if !r.version.is_empty() && f.primary_entity.as_deref() != Some(&r.version) {
                return Ok(json!({"version_unavailable":true}));
            }
            let urls = client
                .get_file_url(&FileURLService {
                    uris: vec![f.path.clone()],
                    entity: f.primary_entity.clone(),
                    ..Default::default()
                })
                .await?;
            let url = &urls
                .urls
                .first()
                .ok_or_else(|| anyhow::anyhow!("missing URL"))?
                .url;
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?
                .get(url)
                .send()
                .await?
                .error_for_status()?;
            // Only write to a new system-provided staging file; no in-place replacement.
            let mut out = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&r.local_path)
                .await?;
            let mut stream = response.bytes_stream();
            let mut size = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                size += chunk.len() as u64;
                ensure!(size <= u64::try_from(f.size)?, "oversized response");
                out.write_all(&chunk).await?;
                progress.store(
                    (size.saturating_mul(1000) / (f.size.max(1) as u64)).min(1000),
                    Ordering::Relaxed,
                );
            }
            ensure!(size == u64::try_from(f.size)?, "size mismatch");
            out.sync_all().await?;
            Ok(serde_json::to_value(f)?)
        }
        "create" | "modify" | "delete" => mutations::perform(client, r, cancel, progress).await,
        _ => anyhow::bail!("unsupported operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_checks_components_and_account() {
        assert!(in_scope("cloudreve://my/a", "cloudreve://my/a/b").is_ok());
        for p in [
            "cloudreve://my/ab",
            "cloudreve://my/a/../b",
            "cloudreve://trash/a",
            "cloudreve://other@my/a",
            "cloudreve://my/a/x%2Fy",
        ] {
            assert!(in_scope("cloudreve://my/a", p).is_err(), "{p}");
        }
    }
    #[test]
    fn invalid_input_does_not_unwind_across_ffi() {
        let input = CString::new("{}").unwrap();
        unsafe {
            let op = crfp_create(input.as_ptr());
            let output = crfp_run(op);
            assert!(
                CStr::from_ptr(output)
                    .to_str()
                    .unwrap()
                    .contains("invalid_request")
            );
            crfp_string_free(output);
            crfp_free(op);
        }
    }
}
