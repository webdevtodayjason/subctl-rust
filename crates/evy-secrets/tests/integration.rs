//! Integration tests for `evy-secrets`.
//!
//! These live in `tests/` rather than `#[cfg(test)] mod tests` because
//! they need to mutate the parent process `PATH` to point at a fixture
//! `op` script. Unit tests stay focused on per-backend semantics.
//!
//! ⚠ Each test runs in the same process; we serialize the
//! `PATH` / `OP_SERVICE_ACCOUNT_TOKEN` mutations behind a `Mutex` to
//! avoid cross-test interference under `cargo test --jobs N`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use evy_secrets::{EnvBackend, FileBackend, OnePasswordBackend, SecretsBackend, SecretsResolver};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Process-wide guard for env mutations. tokio test threads still share
/// one process — env writes from one test must not race another's
/// reads. We use a `tokio::sync::Mutex` (not `std::sync::Mutex`) so the
/// guard can legitimately be held across `.await` points; clippy's
/// `await_holding_lock` lint flags the std version (correctly) because
/// it would deadlock the runtime under contention.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Copy the on-disk fixture to a fresh tempdir as `op`, mark it
/// executable, and prepend the tempdir to `PATH`. Returns the TempDir
/// (must be held for the duration of the test — drop deletes the
/// staged binary) and the prior PATH so the caller can restore it.
fn install_op_mock_on_path() -> (TempDir, Option<std::ffi::OsString>) {
    let dir = TempDir::new().expect("tempdir");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("op-mock.sh");
    assert!(
        fixture.exists(),
        "op-mock.sh missing at {}",
        fixture.display()
    );

    let target = dir.path().join("op");
    std::fs::copy(&fixture, &target).expect("copy fixture");

    // Make sure the copy is executable. The fixture is committed
    // executable, but we re-chmod defensively in case git's
    // core.fileMode is off on the operator's machine.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target, perms).expect("chmod");
    }

    let prior = std::env::var_os("PATH");
    let new_path = match &prior {
        Some(p) => {
            let mut s = std::ffi::OsString::from(dir.path());
            s.push(":");
            s.push(p);
            s
        }
        None => std::ffi::OsString::from(dir.path()),
    };
    // SAFETY (test-only): env mutation serialized by env_lock().
    unsafe { std::env::set_var("PATH", &new_path) };
    (dir, prior)
}

fn restore_path(prior: Option<std::ffi::OsString>) {
    match prior {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }
}

#[tokio::test]
async fn onepassword_backend_reads_through_op_mock() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    // Token value doesn't matter for the known-ref case — the mock
    // only checks that it's non-empty.
    unsafe { std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "fake-test-token") };

    let backend = OnePasswordBackend::new("fake-test-token".into(), "Engineering".into());
    let got = backend.resolve("openai/api-key").await;

    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };
    restore_path(prior);

    let got = got.expect("resolve ok").expect("some value");
    assert_eq!(got.value, "sk-mock-engineering-openai");
    assert_eq!(got.source, "onepassword");
}

#[tokio::test]
async fn onepassword_backend_unknown_ref_is_none() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    unsafe { std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "fake") };

    let backend = OnePasswordBackend::new("fake".into(), "Engineering".into());
    let got = backend.resolve("nope-no-such-item").await;

    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };
    restore_path(prior);

    let got = got.expect("non-zero exit must be Ok(None)");
    assert!(got.is_none(), "got: {got:?}");
}

#[tokio::test]
async fn onepassword_backend_empty_stdout_is_none() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    unsafe { std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "fake") };

    let backend = OnePasswordBackend::new("fake".into(), "Engineering".into());
    let got = backend.resolve("blank-item").await;

    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };
    restore_path(prior);

    let got = got.expect("exit 0 + empty body must be Ok(None)");
    assert!(got.is_none(), "got: {got:?}");
}

#[tokio::test]
async fn onepassword_backend_threads_token_to_child_env() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    // Important: the *backend* is constructed with the token we want
    // the child to see. We DON'T set OP_SERVICE_ACCOUNT_TOKEN in the
    // parent — this proves the backend is actually injecting it.
    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };

    let backend = OnePasswordBackend::new("backend-supplied-token".into(), "Engineering".into());
    let got = backend.resolve("echo-token").await;

    restore_path(prior);

    let got = got
        .expect("resolve ok")
        .expect("mock should echo token back");
    assert_eq!(
        got.value, "backend-supplied-token",
        "OnePasswordBackend must inject OP_SERVICE_ACCOUNT_TOKEN into the child env"
    );
}

#[tokio::test]
async fn resolver_priority_env_beats_file_beats_op_mock() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    unsafe { std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "fake") };

    // Stage a file backend with one key; an env var (highest priority)
    // for that same key; and the op mock as the tail of the chain.
    let key = "evy-secrets-priority-key";
    let env_var = "EVY_SECRETS_PRIORITY_KEY";
    unsafe { std::env::set_var(env_var, "from-env") };

    let tmp = TempDir::new().expect("tempdir for file backend");
    let file_path = tmp.path().join("secrets.json");
    std::fs::write(&file_path, r#"{"evy-secrets-priority-key": "from-file"}"#).expect("write");

    let resolver = SecretsResolver::new(vec![
        Arc::new(EnvBackend::new()) as Arc<dyn SecretsBackend>,
        Arc::new(FileBackend::new(&file_path)) as Arc<dyn SecretsBackend>,
        Arc::new(OnePasswordBackend::new("fake".into(), "Engineering".into()))
            as Arc<dyn SecretsBackend>,
    ]);

    // Env wins.
    let got = resolver.resolve(key).await.expect("ok");
    assert_eq!(got.value, "from-env");
    assert_eq!(got.source, "env");

    // Remove env → file wins.
    unsafe { std::env::remove_var(env_var) };
    let got = resolver.resolve(key).await.expect("ok");
    assert_eq!(got.value, "from-file");
    assert_eq!(got.source, "file");

    // Remove file too → resolver falls all the way through to op,
    // which doesn't know this ref → NotFound.
    std::fs::remove_file(&file_path).expect("rm");
    let err = resolver.resolve(key).await.expect_err("not found");
    assert!(matches!(
        err,
        evy_secrets::SecretsError::NotFound(ref k) if k == key
    ));

    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };
    restore_path(prior);
}

#[tokio::test]
async fn resolver_pulls_from_op_mock_when_only_source() {
    let _g = env_lock().lock().await;
    let (_dir, prior) = install_op_mock_on_path();
    unsafe { std::env::set_var("OP_SERVICE_ACCOUNT_TOKEN", "fake") };

    // No env var, no file → chain falls through to 1Password.
    let resolver = SecretsResolver::new(vec![
        Arc::new(EnvBackend::new()) as Arc<dyn SecretsBackend>,
        Arc::new(OnePasswordBackend::new("fake".into(), "Engineering".into()))
            as Arc<dyn SecretsBackend>,
    ]);

    let got = resolver.resolve("openai/api-key").await;

    unsafe { std::env::remove_var("OP_SERVICE_ACCOUNT_TOKEN") };
    restore_path(prior);

    let got = got.expect("ok");
    assert_eq!(got.value, "sk-mock-engineering-openai");
    assert_eq!(got.source, "onepassword");
}

/// `op` missing from `PATH` must surface as `Ok(None)` (the
/// "1Password backend isn't wired up on this host" case), not an
/// error. Lives here rather than in `onepassword.rs` so it can share
/// the `env_lock()` with the rest of the PATH-mutating tests.
#[tokio::test]
async fn onepassword_backend_missing_op_cli_is_none_not_error() {
    let _g = env_lock().lock().await;
    let saved = std::env::var_os("PATH");
    // SAFETY (test-only): mutation serialized by env_lock().
    unsafe { std::env::set_var("PATH", "/nonexistent-evy-secrets-test-path") };

    let backend = OnePasswordBackend::new("tok".into(), "Personal".into());
    let got = backend.resolve("openai/api-key").await;

    // Restore PATH before the assert so a panic doesn't leave the
    // process with a broken environment for subsequent tests.
    match saved {
        Some(v) => unsafe { std::env::set_var("PATH", v) },
        None => unsafe { std::env::remove_var("PATH") },
    }

    let got = got.expect("missing op must be Ok(None), not Err");
    assert!(got.is_none(), "got: {got:?}");
}
