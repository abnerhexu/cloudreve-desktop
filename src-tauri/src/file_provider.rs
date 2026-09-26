//! Desktop -> bundled File Provider host. Credentials are sent only on stdin.
use crate::commands::AddDriveArgs;
use serde_json::{json, Value};

fn helper_error(output: &[u8]) -> String {
    let value: Value = serde_json::from_slice(output).unwrap_or(Value::Null);
    let domain = value["error_domain"].as_str().unwrap_or("unknown");
    let code = value["error_code"].as_i64().unwrap_or(0);
    // Never expose arbitrary helper output (which could contain server data).
    let advice = match domain {
        "CloudreveAccountScopeMismatch" => "Use the same account and remote folder. Legacy domains without stored identity must be removed with data preservation and added again.",
        "NSOSStatusErrorDomain" => "Unlock the macOS login Keychain and allow Cloudreve to access its account record, then retry.",
        "NSFileProviderErrorDomain" => "Check the Finder drive, account authorization, network, and macOS extension settings.",
        "NSCocoaErrorDomain" => "Check macOS extension settings, file permissions and whether the selected cloud folder is accessible.",
        _ => "Refresh the Finder drive status and retry.",
    };
    let safe_domain = match domain {
        "CloudreveAccountScopeMismatch" | "NSOSStatusErrorDomain" | "NSFileProviderErrorDomain" | "NSCocoaErrorDomain" => domain,
        _ => "unknown",
    };
    format!("File Provider: {safe_domain} ({code}). {advice}")
}

#[cfg(target_os = "macos")]
fn helper() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|_| "Cannot locate application")?;
    let contents = exe
        .parent()
        .and_then(|p| p.parent())
        .ok_or("Invalid application bundle")?;
    let path =
        contents.join("Helpers/CloudreveFinderPreview.app/Contents/MacOS/cloudreve-provider-host");
    if path.is_file() {
        return Ok(path);
    }
    #[cfg(debug_assertions)]
    {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/file-provider/CloudreveFinderPreview.app/Contents/MacOS/cloudreve-provider-host");
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("File Provider component is not installed".into())
}

async fn call(action: &str, arg: Option<&str>, input: Option<Value>) -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        use std::process::Stdio;
        use tokio::{io::AsyncWriteExt, process::Command};
        let helper_path = helper()?;
        // LaunchServices does not reliably discover an extension nested inside
        // Contents/Helpers. Register this bundled extension once per app launch.
        static REGISTERED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        REGISTERED.get_or_try_init(|| async {
            let contents = helper_path.parent().and_then(|p| p.parent())
                .ok_or("Invalid File Provider helper bundle")?;
            let extension = contents.join("PlugIns/CloudreveFileProvider.appex");
            if !extension.is_dir() { return Err("File Provider extension is missing"); }
            let status = tokio::time::timeout(std::time::Duration::from_secs(10),
                Command::new("/usr/bin/pluginkit")
                    .arg("-a").arg(extension)
                    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
                    .kill_on_drop(true).status())
                .await.map_err(|_| "File Provider registration timed out")?
                .map_err(|_| "Cannot register File Provider extension")?;
            if !status.success() { return Err("File Provider registration failed"); }
            Ok(())
        }).await?;
        let mut command = Command::new(helper_path);
        command
            .arg(action)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(arg) = arg {
            command.arg(arg);
        }
        let mut process = command
            .spawn()
            .map_err(|_| "Cannot start File Provider host")?;
        if let Some(input) = input {
            if let Some(mut stdin) = process.stdin.take() {
                stdin
                    .write_all(&serde_json::to_vec(&input).map_err(|_| "Invalid setup data")?)
                    .await
                    .map_err(|_| "File Provider setup failed")?;
            }
        } else {
            drop(process.stdin.take());
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            process.wait_with_output(),
        )
        .await
        .map_err(|_| "File Provider timed out; check registered drives before retrying")?
        .map_err(|_| "File Provider host failed")?;
        if !output.status.success() {
            return Err(helper_error(&output.stdout));
        }
        serde_json::from_slice(&output.stdout).map_err(|_| "Invalid File Provider response".into())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (action, arg, input);
        Err("File Provider requires macOS".into())
    }
}

#[tauri::command]
pub async fn file_provider_status() -> Value {
    if !cfg!(target_os = "macos") {
        return json!({"available": false, "reason": "unsupported"});
    }
    call("status", None, None)
        .await
        .unwrap_or_else(|error| json!({"available": false, "reason": "host_error", "message": error}))
}

#[tauri::command]
pub async fn add_finder_drive(config: AddDriveArgs, domain_id: String) -> Result<String, String> {
    configure_finder_drive(config, domain_id, "register").await
}

#[tauri::command]
pub async fn reauthorize_finder_drive(config: AddDriveArgs, domain_id: String) -> Result<String, String> {
    configure_finder_drive(config, domain_id, "reauthorize").await
}

async fn configure_finder_drive(config: AddDriveArgs, domain_id: String, action: &str) -> Result<String, String> {
    if file_provider_status().await["available"] != true {
        return Err("Finder sync is unavailable in this build".into());
    }
    uuid::Uuid::parse_str(&domain_id).map_err(|_| "Invalid domain ID")?;
    let now = chrono::Utc::now();
    let access = chrono::Duration::try_seconds(
        i64::try_from(config.access_token_expires).map_err(|_| "Invalid expiry")?,
    )
    .ok_or("Invalid expiry")?;
    let refresh = chrono::Duration::try_seconds(
        i64::try_from(config.refresh_token_expires).map_err(|_| "Invalid expiry")?,
    )
    .ok_or("Invalid expiry")?;
    let request = json!({
        "domain": domain_id, "server": config.site_url, "root": config.remote_path, "writable": true, "user_id": config.user_id,
        "tokens": {"access_token": config.access_token, "refresh_token": config.refresh_token,
            "access_expires": now.checked_add_signed(access).ok_or("Invalid expiry")?.to_rfc3339(),
            "refresh_expires": now.checked_add_signed(refresh).ok_or("Invalid expiry")?.to_rfc3339()}
    });
    call(action, Some(&config.drive_name), Some(request)).await?;
    Ok(domain_id)
}

#[tauri::command]
pub async fn list_finder_drives() -> Result<Value, String> {
    if !cfg!(target_os = "macos") {
        return Ok(json!([]));
    }
    call("list", None, None).await
}
#[tauri::command]
pub async fn resume_finder_drive(domain_id: String) -> Result<Value, String> {
    uuid::Uuid::parse_str(&domain_id).map_err(|_| "Invalid domain ID")?;
    call("resume", Some(&domain_id), None).await
}
#[tauri::command]
pub async fn finder_drive_location(domain_id: String) -> Result<Value, String> {
    uuid::Uuid::parse_str(&domain_id).map_err(|_| "Invalid domain ID")?;
    call("location", Some(&domain_id), None).await
}
#[tauri::command]
pub async fn remove_finder_drive(domain_id: String) -> Result<Value, String> {
    uuid::Uuid::parse_str(&domain_id).map_err(|_| "Invalid domain ID")?;
    call("remove", Some(&domain_id), None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_errors_keep_codes_but_never_arbitrary_output() {
        assert!(helper_error(br#"{"error_domain":"NSOSStatusErrorDomain","error_code":-25308}"#).contains("Keychain"));
        assert!(helper_error(br#"{"error_domain":"NSCocoaErrorDomain","error_code":4099}"#).contains("4099"));
        assert!(!helper_error(b"secret server response").contains("secret"));
        assert!(!helper_error(br#"{"error_domain":"secret","error_code":1}"#).contains("secret"));
    }
}
