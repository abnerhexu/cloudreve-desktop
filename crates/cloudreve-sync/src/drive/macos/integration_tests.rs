use super::*;
use crate::{
    drive::mounts::{Credentials, DriveConfig},
    inventory::InventoryDb,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Server {
    files: Arc<Mutex<BTreeMap<String, FileResponse>>>,
    fail: Arc<AtomicBool>,
    origin: String,
}
fn response(data: impl serde::Serialize) -> Json<Value> {
    Json(json!({"code":0,"msg":"","data":data}))
}
fn remote(name: &str, data: &str) -> FileResponse {
    serde_json::from_value(json!({"id":name,"name":name,"type":0,"size":data.len(),"path":format!("cloudreve://my/{name}"),"primary_entity":data,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"})).unwrap()
}
async fn list(State(s): State<Server>) -> Json<Value> {
    if s.fail.load(Ordering::SeqCst) {
        return Json(json!({"code":500,"msg":"injected listing failure"}));
    }
    let files: Vec<_> = s.files.lock().await.values().cloned().collect();
    response(cloudreve_api::models::explorer::ListResponse {
        files,
        ..Default::default()
    })
}
async fn info(State(s): State<Server>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let name = q["uri"].rsplit('/').next().unwrap();
    response(s.files.lock().await.get(name).cloned())
}
async fn url(State(s): State<Server>, Json(v): Json<Value>) -> Json<Value> {
    let name = v["uris"][0].as_str().unwrap().rsplit('/').next().unwrap();
    response(json!({"urls":[{"url":format!("{}/content?name={}",s.origin,name)}],"expires":""}))
}
async fn content(State(s): State<Server>, Query(q): Query<HashMap<String, String>>) -> String {
    s.files.lock().await[&q["name"]]
        .primary_entity
        .clone()
        .unwrap()
}
async fn delete(State(s): State<Server>, Json(v): Json<Value>) -> Json<Value> {
    for uri in v["uris"].as_array().unwrap() {
        s.files
            .lock()
            .await
            .remove(uri.as_str().unwrap().rsplit('/').next().unwrap());
    }
    response(())
}
async fn create(State(s): State<Server>, Json(v): Json<Value>) -> Json<Value> {
    let name = v["uri"].as_str().unwrap().rsplit('/').next().unwrap();
    let file = remote(name, "");
    s.files.lock().await.insert(name.to_string(), file.clone());
    response(file)
}
async fn settle(mount: &Mount) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !mount.task_queue.list_active_tasks().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("transfer timed out");
    let tasks = mount.inventory.query_recent_tasks(Some(&mount.id)).unwrap();
    for task in tasks.finished {
        assert_ne!(task.status.as_str(), "failed", "transfer failed: {task:?}");
    }
}

#[tokio::test]
async fn download_update_conflict_delete_and_listing_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server {
        files: Arc::new(Mutex::new(BTreeMap::new())),
        fail: Arc::new(AtomicBool::new(false)),
        origin: format!("http://{}", listener.local_addr().unwrap()),
    };
    server
        .files
        .lock()
        .await
        .insert("file.txt".into(), remote("file.txt", "old"));
    let app = Router::new()
        .route("/api/v4/file", get(list).delete(delete))
        .route("/api/v4/file/info", get(info))
        .route("/api/v4/file/url", post(url))
        .route("/api/v4/file/create", post(create))
        .route("/content", get(content))
        .with_state(server.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let root = base.join("sync");
    std::fs::create_dir(&root).unwrap();
    let db = Arc::new(InventoryDb::with_path(base.join("db.sqlite")).unwrap());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mount = Mount::new(
        DriveConfig {
            id: Uuid::new_v4().to_string(),
            instance_url: server.origin.clone(),
            remote_path: "cloudreve://my".into(),
            sync_path: root.clone(),
            enabled: true,
            credentials: Credentials {
                access_token: Some("test".into()),
                refresh_token: "test".into(),
                access_expires: Some("2099-01-01T00:00:00Z".into()),
                refresh_expires: "2099-01-01T00:00:00Z".into(),
            },
            ..Default::default()
        },
        db,
        tx,
    )
    .await;
    mount
        .cr_client
        .set_tokens("test".into(), "test".into())
        .await;
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    settle(&mount).await;
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"old");
    server
        .files
        .lock()
        .await
        .insert("file.txt".into(), remote("file.txt", "new"));
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    settle(&mount).await;
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"new");
    std::fs::write(root.join("file.txt"), b"local").unwrap();
    server
        .files
        .lock()
        .await
        .insert("file.txt".into(), remote("file.txt", "remote"));
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    settle(&mount).await;
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"remote");
    let conflict = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("conflict")
        })
        .unwrap();
    assert_eq!(std::fs::read(&conflict).unwrap(), b"local");
    std::fs::remove_file(conflict).unwrap();
    server.fail.store(true, Ordering::SeqCst);
    assert!(
        mount
            .sync_paths(vec![], SyncMode::FullHierarchy)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(root.join("file.txt")).unwrap(), b"remote");
    server.fail.store(false, Ordering::SeqCst);
    std::fs::remove_file(root.join("file.txt")).unwrap();
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    assert!(server.files.lock().await.is_empty());
    // New local empty file exercises the shared upload path without provider-specific mocks.
    std::fs::write(root.join("empty"), b"").unwrap();
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    settle(&mount).await;
    assert!(server.files.lock().await.contains_key("empty"));
    server.files.lock().await.clear();
    mount
        .sync_paths(vec![], SyncMode::FullHierarchy)
        .await
        .unwrap();
    assert!(!root.join("empty").exists());
    // A missing root is not an empty tree and cannot authorize cloud deletion.
    server
        .files
        .lock()
        .await
        .insert("protected".into(), remote("protected", "data"));
    std::fs::rename(&root, base.join("moved-sync")).unwrap();
    assert!(
        mount
            .sync_paths(vec![], SyncMode::FullHierarchy)
            .await
            .is_err()
    );
    assert!(server.files.lock().await.contains_key("protected"));
    std::fs::rename(base.join("moved-sync"), &root).unwrap();
    // A queued download must not overwrite edits made after it was planned.
    std::fs::write(root.join("protected"), b"editing").unwrap();
    mount
        .task_queue
        .enqueue(
            TaskPayload::download(root.join("protected"))
                .with_custom_state(json!({"macos_expected":null})),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !mount.task_queue.list_active_tasks().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read(root.join("protected")).unwrap(), b"editing");
    assert!(
        mount
            .inventory
            .query_recent_tasks(Some(&mount.id))
            .unwrap()
            .finished
            .iter()
            .any(|t| t.status.as_str() == "failed")
    );
    mount.shutdown().await;
    handle.abort();
}
