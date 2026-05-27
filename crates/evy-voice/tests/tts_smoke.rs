//! Smoke test against a real local TTS server on :8789. Ignored by
//! default — opt in with `cargo test -p evy-voice -- --ignored` when
//! the operator wants to verify against the launchd-managed
//! `com.subctl.tts` service.

use std::sync::Arc;

use evy_voice::{TtsClient, VoiceConfig, VoiceConfigStore, VoiceRenderer};
use tempfile::TempDir;

#[ignore = "requires a running TTS server on :8789; run with --ignored"]
#[tokio::test]
async fn renders_against_real_tts() {
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("voice.json");
    let cfg = VoiceConfig {
        enabled: true,
        default_voice_id: "evy-rachel-weisz".into(),
        model: "voxcpm-0.5b".into(),
        tts_server: "http://localhost:8789".into(),
    };
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    let store = Arc::new(VoiceConfigStore::open(&cfg_path).await.unwrap());
    let client = Arc::new(TtsClient::new(cfg.tts_server.clone()));
    let renderer = VoiceRenderer::new(store, client, dir.path().join("cache"));

    let out = renderer
        .render("hello jason, this is evy", None)
        .await
        .expect("real TTS smoke must succeed when --ignored is passed");
    assert!(out.bytes_written > 0 || out.cached);
    assert!(out.audio_path.exists());
}
