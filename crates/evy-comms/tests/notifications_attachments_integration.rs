//! HTTP integration tests for the v4-native notification tray + attachments
//! families (W6). Each test drives the real axum router on an ephemeral port.
//!
//! Both tests are deliberately self-contained: the notification ring is a
//! process-global (mirroring v3's module-global `_ring`), so all notification
//! assertions live in one sequential test rather than racing across parallel
//! tests. The attachments test points `SUBCTL_CONFIG_DIR` at a tempdir so it
//! never touches the real `~/.config/subctl` tree.

use std::sync::Arc;
use std::time::Duration;

use evy_comms::notifications_http::{emit, EmitNotificationInput, NotificationSeverity};
use evy_comms::{AppState, EventBroadcaster, HttpConfig, HttpServer, StubAppState};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

async fn spawn() -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
    let state: Arc<dyn AppState> = Arc::new(StubAppState);
    let server = HttpServer::new(HttpConfig::ephemeral(), broadcaster, state);
    let bound = server.bind().await.expect("bind");
    let addr = bound.local_addr();
    let shutdown = CancellationToken::new();
    let st = shutdown.clone();
    tokio::spawn(async move {
        let _ = bound.serve(st).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), shutdown)
}

#[tokio::test]
async fn notifications_tray_via_http() {
    let (base, shutdown) = spawn().await;
    let client = reqwest::Client::new();

    // Seed the process-global ring (the only emitter in this test binary).
    let n1 = emit(EmitNotificationInput {
        kind: "test-w6".into(),
        severity: NotificationSeverity::Alert,
        title: "hello operator".into(),
        body: "details".into(),
        team_id: Some("team-x".into()),
        metadata: None,
    });

    // GET list — v3 shape: { ok, notifications:[…] }.
    let res = client
        .get(format!("{base}/api/evy/notifications?limit=200"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    let arr = body["notifications"].as_array().expect("array");
    let found = arr
        .iter()
        .find(|n| n["id"] == json!(n1.id))
        .expect("emitted notification present");
    assert_eq!(found["severity"], json!("alert")); // enum → lowercase
    assert_eq!(found["read_at"], Value::Null); // present-and-null while unread
    assert_eq!(found["team_id"], json!("team-x"));
    assert_eq!(found["title"], json!("hello operator"));
    // `metadata` omitted when absent (v3 JSON.stringify parity).
    assert!(found.get("metadata").is_none());

    // POST {id}/read → { ok, found:true }.
    let res = client
        .post(format!("{base}/api/evy/notifications/{}/read", n1.id))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": true, "found": true }));

    // Re-list — the entry now carries a read_at timestamp.
    let res = client
        .get(format!("{base}/api/evy/notifications?limit=200"))
        .send()
        .await
        .expect("send");
    let body: Value = res.json().await.expect("json");
    let found = body["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == json!(n1.id))
        .unwrap();
    assert!(found["read_at"].is_string());

    // Unknown id → { ok, found:false }.
    let res = client
        .post(format!(
            "{base}/api/evy/notifications/deadbeef-unknown/read"
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": true, "found": false }));

    // read-all → { ok, marked:<number> }.
    let res = client
        .post(format!("{base}/api/evy/notifications/read-all"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert!(body["marked"].is_number());

    shutdown.cancel();
}

#[tokio::test]
async fn attachments_lifecycle_via_http() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Scope storage to the tempdir — handlers read SUBCTL_CONFIG_DIR per request.
    std::env::set_var("SUBCTL_CONFIG_DIR", dir.path());

    let (base, shutdown) = spawn().await;
    let client = reqwest::Client::new();

    let payload = b"# Spec\nhello".to_vec();
    let payload_len = payload.len();

    // POST upload — raw bytes + url-encoded X-Filename. 201 + v3 attachment shape.
    let res = client
        .post(format!("{base}/api/evy/attachments"))
        .header("X-Filename", "spec%20notes.md")
        .header("X-Source", "paste")
        .body(payload.clone())
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 201);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    let id = body["attachment"]["id"].as_str().expect("id").to_string();
    assert_eq!(id.len(), 16);
    assert_eq!(body["attachment"]["filename"], json!("spec notes.md")); // url-decoded
    assert_eq!(body["attachment"]["mime"], json!("text/markdown"));
    assert_eq!(body["attachment"]["size"], json!(payload_len));
    assert!(body["attachment"]["sha256"].is_string());

    // GET list — { ok, count, attachments:[…meta] }, metadata-only.
    let res = client
        .get(format!("{base}/api/evy/attachments"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["count"], json!(1));
    let item = &body["attachments"][0];
    assert_eq!(item["id"], json!(id));
    assert_eq!(item["source"], json!("paste"));
    assert_eq!(item["filename"], json!("spec notes.md"));
    // List omits sha256 / storage_path / deleted_at (v3 list shape).
    assert!(item.get("sha256").is_none());
    assert!(item.get("storage_path").is_none());
    assert!(item.get("deleted_at").is_none());

    // GET {id} — serve the bytes back with mime + inline disposition.
    let res = client
        .get(format!("{base}/api/evy/attachments/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers().get("content-type").unwrap().to_str().unwrap(),
        "text/markdown"
    );
    let cd = res
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cd.contains("inline"));
    assert!(cd.contains("spec notes.md"));
    let served = res.bytes().await.unwrap();
    assert_eq!(served.as_ref(), payload.as_slice());

    // Unknown id → 404 { ok:false, error:"not found" }.
    let res = client
        .get(format!("{base}/api/evy/attachments/deadbeefdeadbeef"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": false, "error": "not found" }));

    // Non-hex id (traversal probe) → client error, no file access. (Exact code
    // depends on axum's `%2f` path handling; the hex-id guard rejects either way.)
    let res = client
        .get(format!("{base}/api/evy/attachments/..%2f..%2fetc"))
        .send()
        .await
        .expect("send");
    assert!(res.status().is_client_error());

    // DELETE → { ok:true }, then DELETE again → 404.
    let res = client
        .delete(format!("{base}/api/evy/attachments/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body, json!({ "ok": true }));

    let res = client
        .delete(format!("{base}/api/evy/attachments/{id}"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 404);

    // List is empty after delete.
    let res = client
        .get(format!("{base}/api/evy/attachments"))
        .send()
        .await
        .expect("send");
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["count"], json!(0));

    shutdown.cancel();
}
