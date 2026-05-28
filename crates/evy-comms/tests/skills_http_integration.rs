//! End-to-end tests for `GET /api/evy/skills`.
//!
//! Spins up a real axum server backed by an `AppState` whose `skills()`
//! returns either a populated registry or `None`. No LLM, no chat
//! partner — only the read-only skills surface is exercised.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evy_comms::{
    AppState, EventBroadcaster, HttpConfig, HttpServer, JobSummary, SkillsListResponse,
    StubAppState, WorkerSummary,
};
use evy_policy::Policy;
use evy_skills::SkillRegistry;
use evy_thinking::ThinkingPartner;
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

struct SkillsTestState {
    skills: Option<Arc<SkillRegistry>>,
}

#[async_trait]
impl AppState for SkillsTestState {
    async fn workers(&self) -> Vec<WorkerSummary> {
        Vec::new()
    }
    async fn jobs(&self) -> Vec<JobSummary> {
        Vec::new()
    }
    async fn policy(&self) -> Policy {
        Policy::default()
    }
    fn thinking_partner(&self) -> Option<Arc<ThinkingPartner>> {
        None
    }
    fn skills(&self) -> Option<Arc<SkillRegistry>> {
        self.skills.clone()
    }
}

async fn spawn(state: Arc<dyn AppState>) -> (String, CancellationToken) {
    let broadcaster = EventBroadcaster::new(64);
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

/// Build a registry with two skills on disk.
fn fixture_registry() -> (TempDir, Arc<SkillRegistry>) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("plan")).unwrap();
    std::fs::write(
        dir.path().join("plan").join("SKILL.md"),
        r#"---
name: plan
description: draft a multi-step plan
triggers: ["plan", "outline"]
priority: 7
---

Plan body.
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("debugging")).unwrap();
    std::fs::write(
        dir.path().join("debugging").join("SKILL.md"),
        r#"---
name: debugging
description: walk through a failing test
triggers: ["debug"]
---

Debugging body.
"#,
    )
    .unwrap();
    let reg = Arc::new(SkillRegistry::load(dir.path()).expect("load"));
    (dir, reg)
}

#[tokio::test]
async fn skills_list_returns_503_when_registry_absent() {
    let (base, shutdown) = spawn(Arc::new(StubAppState)).await;
    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/skills"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["kind"], "unavailable");
    shutdown.cancel();
}

#[tokio::test]
async fn skills_list_returns_loaded_catalog() {
    let (_dir, reg) = fixture_registry();
    let state = Arc::new(SkillsTestState {
        skills: Some(reg.clone()),
    });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/api/evy/skills"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: SkillsListResponse = res.json().await.expect("json");
    assert_eq!(body.skills.len(), 2);

    // Alphabetised: debugging < plan
    assert_eq!(body.skills[0].name, "debugging");
    assert_eq!(body.skills[0].description, "walk through a failing test");
    assert_eq!(body.skills[0].triggers, vec!["debug".to_string()]);
    assert_eq!(body.skills[0].priority, 0);

    assert_eq!(body.skills[1].name, "plan");
    assert_eq!(body.skills[1].priority, 7);
    assert_eq!(
        body.skills[1].triggers,
        vec!["plan".to_string(), "outline".to_string()]
    );
    shutdown.cancel();
}

#[tokio::test]
async fn skills_list_master_alias_route_also_works() {
    let (_dir, reg) = fixture_registry();
    let state = Arc::new(SkillsTestState { skills: Some(reg) });
    let (base, shutdown) = spawn(state).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/api/master/skills"))
        .send()
        .await
        .expect("send");
    assert_eq!(res.status(), 200);
    let body: SkillsListResponse = res.json().await.expect("json");
    assert_eq!(body.skills.len(), 2);
    shutdown.cancel();
}
