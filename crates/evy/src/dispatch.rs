//! Cutover Phase 2 slice 2c — dispatch → register → emit.
//!
//! [`dispatch_and_register`] is the seam that turns a successful provider
//! dispatch into a tracked worker: it dispatches the mandate, records the
//! resulting handle in the shared [`WorkerRegistry`], and emits a
//! [`DaemonEvent::WorkerRegistered`] onto the SSE bus. The spawn endpoint (2j)
//! calls this with the real `ClaudeCodeProvider`/`CodexProvider`; it's written
//! against the `&dyn Provider` trait so it's unit-testable with a mock — no tmux,
//! no quota.

use evy_comms::{DaemonEvent, EventBroadcaster};
use evy_core::{Mandate, Provider, Result, WorkerId, WorkerRecord, WorkerRegistry};

/// Dispatch `mandate` via `provider`, register the resulting worker as `Running`
/// in `registry`, and emit `WorkerRegistered`. Returns the new worker id.
/// `now_ms` is unix-millis (caller supplies the clock).
///
/// # Errors
/// Propagates any [`evy_core::Error`] the provider's `dispatch` returns; on
/// error nothing is registered and no event is emitted.
pub async fn dispatch_and_register(
    provider: &dyn Provider,
    mandate: &Mandate,
    registry: &WorkerRegistry,
    broadcaster: &EventBroadcaster,
    now_ms: i64,
) -> Result<WorkerId> {
    let handle = provider.dispatch(mandate).await?;
    let worker_id = handle.id();
    let provider_kind = provider.kind();
    let mandate_id = handle.mandate_id();

    registry.register(WorkerRecord::running(
        worker_id,
        provider_kind,
        mandate_id,
        now_ms,
    ));
    broadcaster.emit(DaemonEvent::WorkerRegistered {
        worker_id,
        provider: provider_kind,
        mandate_id,
    });
    Ok(worker_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use evy_core::{MandateId, PolicyMode, ProviderKind, WorkerHandle, WorkerStatus};
    use std::collections::HashMap;

    struct MockHandle {
        id: WorkerId,
        mandate_id: MandateId,
    }

    #[async_trait]
    impl WorkerHandle for MockHandle {
        fn id(&self) -> WorkerId {
            self.id
        }
        fn mandate_id(&self) -> MandateId {
            self.mandate_id
        }
        async fn status(&self) -> Result<WorkerStatus> {
            Ok(WorkerStatus::Running)
        }
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
        async fn wait(&self) -> Result<WorkerStatus> {
            Ok(WorkerStatus::Succeeded)
        }
    }

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::ClaudeCode
        }
        async fn dispatch(&self, mandate: &Mandate) -> Result<Box<dyn WorkerHandle>> {
            Ok(Box::new(MockHandle {
                id: WorkerId::new(),
                mandate_id: mandate.id,
            }))
        }
        async fn healthcheck(&self) -> Result<()> {
            Ok(())
        }
    }

    fn test_mandate() -> Mandate {
        Mandate {
            id: MandateId::new(),
            provider: ProviderKind::ClaudeCode,
            goal: "stand up the desk".into(),
            context: String::new(),
            deliverable: String::new(),
            done_when: Vec::new(),
            constraints: Vec::new(),
            policy_mode: PolicyMode::Gated,
            timeout: None,
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_registers_worker_and_emits_event() {
        let registry = WorkerRegistry::new();
        let broadcaster = EventBroadcaster::default();
        let mut rx = broadcaster.subscribe();
        let mandate = test_mandate();

        let id = dispatch_and_register(&MockProvider, &mandate, &registry, &broadcaster, 1000)
            .await
            .expect("dispatch_and_register");

        // Registry populated → /api/evy/workers would now be non-empty (criterion #1).
        assert_eq!(registry.len(), 1);
        let rec = registry.get(&id).expect("worker present");
        assert_eq!(rec.status, WorkerStatus::Running);
        assert_eq!(rec.provider, ProviderKind::ClaudeCode);
        assert_eq!(rec.mandate_id, mandate.id);

        // WorkerRegistered emitted with the same id.
        match rx.try_recv().expect("event emitted") {
            DaemonEvent::WorkerRegistered {
                worker_id,
                provider,
                mandate_id,
            } => {
                assert_eq!(worker_id, id);
                assert_eq!(provider, ProviderKind::ClaudeCode);
                assert_eq!(mandate_id, mandate.id);
            }
            other => panic!("expected WorkerRegistered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_failure_registers_nothing() {
        struct FailProvider;
        #[async_trait]
        impl Provider for FailProvider {
            fn kind(&self) -> ProviderKind {
                ProviderKind::ClaudeCode
            }
            async fn dispatch(&self, _m: &Mandate) -> Result<Box<dyn WorkerHandle>> {
                Err(evy_core::Error::Provider {
                    kind: ProviderKind::ClaudeCode,
                    reason: "nope".into(),
                })
            }
            async fn healthcheck(&self) -> Result<()> {
                Ok(())
            }
        }
        let registry = WorkerRegistry::new();
        let broadcaster = EventBroadcaster::default();
        let res = dispatch_and_register(
            &FailProvider,
            &test_mandate(),
            &registry,
            &broadcaster,
            1000,
        )
        .await;
        assert!(res.is_err());
        assert_eq!(registry.len(), 0); // nothing registered on dispatch failure
    }
}
