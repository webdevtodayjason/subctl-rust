//! Integration test: VoiceConfigStore picks up an external modification
//! within the 200ms debounce window.

use std::time::Duration;

use evy_voice::{VoiceConfig, VoiceConfigStore};
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test]
async fn external_write_propagates_to_subscribers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("voice.json");

    let store = VoiceConfigStore::open(&path).await.unwrap();
    assert!(!store.enabled(), "default config is disabled");

    let mut rx = store.subscribe();
    // Subscriber starts with the current value already loaded; mark it
    // seen so the next `changed()` waits for a real update.
    rx.borrow_and_update();

    // Simulate an external editor flipping `enabled: true`. We write
    // through the same atomic-rename pattern the store uses, mirroring
    // how the v3 dashboard's POST /voice/config behaves.
    let next = VoiceConfig {
        enabled: true,
        default_voice_id: "evy-rachel-weisz".into(),
        model: "voxcpm-0.5b".into(),
        tts_server: "http://localhost:8789".into(),
    };
    let tmp = dir.path().join(".voice.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&next).unwrap()).unwrap();
    std::fs::rename(&tmp, &path).unwrap();

    // 500ms ceiling per the spec — 200ms debounce + slack.
    timeout(Duration::from_millis(500), rx.changed())
        .await
        .expect("watch channel must signal within 500ms")
        .expect("watch channel must not be closed");

    let observed = rx.borrow().clone();
    assert!(observed.enabled, "subscriber saw the flipped flag");
    assert_eq!(observed.default_voice_id, "evy-rachel-weisz");
}

#[tokio::test]
async fn set_publishes_immediately_without_waiting_for_watcher() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("voice.json");

    let store = VoiceConfigStore::open(&path).await.unwrap();
    let mut rx = store.subscribe();
    rx.borrow_and_update();

    let next = VoiceConfig {
        enabled: true,
        default_voice_id: "evy-other".into(),
        model: "voxcpm-1.0b".into(),
        tts_server: "http://localhost:9999".into(),
    };
    store.set(next.clone()).await.unwrap();

    // `set()` publishes via send_replace before writing to disk, so the
    // first changed() is well under the debounce window.
    timeout(Duration::from_millis(100), rx.changed())
        .await
        .expect("set() must publish via watch channel immediately")
        .expect("watch channel must not be closed");
    assert_eq!(rx.borrow().clone(), next);

    // And the file on disk reflects the change.
    let on_disk: VoiceConfig = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk, next);
}

#[tokio::test]
async fn defaults_seeded_when_file_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("voice.json");
    assert!(!path.exists(), "precondition: file missing");

    let store = VoiceConfigStore::open(&path).await.unwrap();
    assert_eq!(store.current(), VoiceConfig::default());
    assert!(path.exists(), "store must seed defaults on first open");
}
