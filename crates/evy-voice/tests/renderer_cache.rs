//! Integration test: VoiceRenderer hits the cache on the second call
//! for the same (text, voice_id, model) tuple, and refuses on a redacted
//! input.

use std::sync::Arc;

use evy_voice::{TtsClient, VoiceConfig, VoiceConfigStore, VoiceError, VoiceRenderer};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spin up a wiremock server pretending to be the TTS server, a config
/// store with `enabled: true` pointing at it, and a renderer ready to
/// fire. Returns the renderer plus the mock for assertion access.
async fn fixture() -> (VoiceRenderer, MockServer, TempDir) {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();

    // Seed a config that points at the mock, enabled.
    let cfg_path = dir.path().join("voice.json");
    let cfg = VoiceConfig {
        enabled: true,
        default_voice_id: "evy-rachel-weisz".into(),
        model: "voxcpm-0.5b".into(),
        tts_server: mock.uri(),
    };
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    let store = Arc::new(VoiceConfigStore::open(&cfg_path).await.unwrap());
    let client = Arc::new(TtsClient::new(store.current().tts_server.clone()));
    let cache_dir = dir.path().join("cache");
    let renderer = VoiceRenderer::new(store, client, cache_dir);

    (renderer, mock, dir)
}

#[tokio::test]
async fn render_caches_on_second_call() {
    let (renderer, mock, _dir) = fixture().await;

    // Expect exactly ONE POST /render across the two calls.
    Mock::given(method("POST"))
        .and(path("/render"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"FAKEWAV".to_vec()))
        .expect(1)
        .mount(&mock)
        .await;

    let first = renderer.render("hello jason", None).await.unwrap();
    assert!(!first.cached, "first call is a cache miss");
    assert_eq!(first.bytes_written, 7);
    assert!(first.audio_path.exists());
    assert!(first.audio_url.starts_with("/voice/audio/"));
    assert!(first.audio_url.ends_with(".wav"));

    let second = renderer.render("hello jason", None).await.unwrap();
    assert!(second.cached, "second call is a cache hit");
    assert_eq!(second.bytes_written, 0);
    assert_eq!(second.audio_path, first.audio_path);
    assert_eq!(second.audio_url, first.audio_url);

    // wiremock's expect(1) verifies the count when MockServer drops.
}

#[tokio::test]
async fn render_refuses_when_disabled() {
    let mock = MockServer::start().await;
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("voice.json");
    // Default: enabled=false.
    let store = Arc::new(VoiceConfigStore::open(&cfg_path).await.unwrap());
    let client = Arc::new(TtsClient::new(mock.uri()));
    let renderer = VoiceRenderer::new(store, client, dir.path().join("cache"));

    let err = renderer
        .render("hello", None)
        .await
        .expect_err("disabled config must refuse");
    assert!(matches!(err, VoiceError::Disabled), "got: {err:?}");
}

#[tokio::test]
async fn render_refuses_redacted_text() {
    let (renderer, mock, _dir) = fixture().await;
    // The mock has no expectations set; if the renderer reached out to
    // the TTS server it would fail the test (404 from wiremock by
    // default → TtsServerStatus, not Redacted).
    let _ = &mock;

    // 25-char secret-shaped suffix (above the spec's 20-char floor).
    let bad = "render this: sk-abc1234567890abcdef12345";
    let err = renderer
        .render(bad, None)
        .await
        .expect_err("redacted input must refuse");
    match err {
        VoiceError::Redacted { pattern } => {
            assert_eq!(pattern, "sk-key", "expected sk-key label, got {pattern}");
        }
        other => panic!("expected Redacted, got {other:?}"),
    }
}

#[tokio::test]
async fn render_propagates_tts_server_failure() {
    let (renderer, mock, _dir) = fixture().await;

    Mock::given(method("POST"))
        .and(path("/render"))
        .respond_with(ResponseTemplate::new(503).set_body_string("server overloaded"))
        .mount(&mock)
        .await;

    let err = renderer
        .render("unique text not in cache yet", None)
        .await
        .expect_err("5xx must surface");
    match err {
        VoiceError::TtsServerStatus { status, detail } => {
            assert_eq!(status, 503);
            assert!(detail.contains("overloaded"), "got: {detail}");
        }
        other => panic!("expected TtsServerStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn render_passes_request_shape_to_tts_server() {
    use wiremock::matchers::{body_json, header};
    let (renderer, mock, _dir) = fixture().await;

    Mock::given(method("POST"))
        .and(path("/render"))
        .and(header("content-type", "application/json"))
        .and(body_json(serde_json::json!({
            "text": "hello jason",
            "voice_id": "evy-rachel-weisz",
            "model": "voxcpm-0.5b",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"OK".to_vec()))
        .expect(1)
        .mount(&mock)
        .await;

    let out = renderer.render("hello jason", None).await.unwrap();
    assert!(!out.cached);
    assert_eq!(out.bytes_written, 2);
}

#[tokio::test]
async fn render_empty_text_is_rejected() {
    let (renderer, _mock, _dir) = fixture().await;
    let err = renderer
        .render("   \t\n  ", None)
        .await
        .expect_err("whitespace-only must reject");
    assert!(matches!(err, VoiceError::EmptyText), "got: {err:?}");
}

#[tokio::test]
async fn status_reflects_config_and_health_probe() {
    let (renderer, mock, _dir) = fixture().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&mock)
        .await;

    let status = renderer.status().await.unwrap();
    assert!(status.enabled);
    assert_eq!(status.voice_id, "evy-rachel-weisz");
    assert_eq!(status.model, "voxcpm-0.5b");
    assert!(status.server_reachable);
}
