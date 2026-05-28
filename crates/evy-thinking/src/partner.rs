//! [`ThinkingPartner`] — composition of [`LlmBackend`] + in-memory
//! session store + the structured planning UX.
//!
//! Owns nothing the operator can lose by accident: persistence is the
//! daemon's job (see [`ThinkingPartner::with_message_hook`]), and the
//! partner does not spawn workers under any circumstance. Promoting a
//! draft plan into a [`Mandate`](evy_core::Mandate) is an explicit
//! operator action handled in `crates/evy/` — never an implicit
//! side-effect of a conversation.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info};

use crate::backend::{LlmBackend, StreamChunk};
use crate::error::{Result, ThinkingError};
use crate::session::{Message, Role, Session, SessionId, SessionStatus};
use crate::templates::{conclusion_user_turn, kickoff_user_turn, planning_system_prompt};

/// Type alias for the per-message hook the daemon wires in.
///
/// The hook is **synchronous** — invoke `tokio::spawn` inside if you
/// need to call an async API like `ObservationLog::append`. The partner
/// invokes the hook from inside an async context, so spawning is
/// available; we kept the signature sync to keep the public API simple
/// for surfaces that only want to print messages.
pub type MessageHook = Arc<dyn Fn(&Message) + Send + Sync>;

/// The thinking-partner surface. Cheap to clone if you wrap it in an
/// `Arc` — internally already `Arc`-shares the backend and sessions
/// store.
pub struct ThinkingPartner {
    backend: Arc<dyn LlmBackend>,
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    on_message: Option<MessageHook>,
}

impl ThinkingPartner {
    /// Construct with a backend and no message hook.
    ///
    /// Use [`with_message_hook`](Self::with_message_hook) to attach a
    /// persistence sink before the daemon starts handling sessions.
    #[must_use]
    pub fn new(backend: Arc<dyn LlmBackend>) -> Self {
        Self {
            backend,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            on_message: None,
        }
    }

    /// Attach a per-message hook. The hook is invoked for every message
    /// the partner records — both operator turns and the partner's own
    /// replies. Use this to mirror sessions into
    /// `evy-memory::ObservationLog`.
    ///
    /// The hook is sync; `tokio::spawn` inside for async work. See
    /// [`MessageHook`] for the rationale.
    #[must_use]
    pub fn with_message_hook(mut self, hook: MessageHook) -> Self {
        self.on_message = Some(hook);
        self
    }

    /// Start a new planning session.
    ///
    /// This makes a synchronous LLM call: PHASE 1 of the planning UX
    /// requires the partner to ask 3-5 clarifying questions *before*
    /// the operator says anything. The opening questions are appended
    /// to the session as a [`Role::Partner`] message; callers retrieve
    /// them via [`session`](Self::session) and the
    /// [`Session::messages`] tail.
    ///
    /// # Errors
    /// - [`ThinkingError::Input`] if `topic` is blank.
    /// - Any error from the underlying backend's `respond` call.
    pub async fn start_session(&self, topic: String) -> Result<SessionId> {
        if topic.trim().is_empty() {
            return Err(ThinkingError::Input("topic is empty".to_string()));
        }

        let mut session = Session::new(topic.clone());
        let id = session.id;

        // The "session opened" system row is purely cosmetic — surfaces
        // that want to render "started thinking about X" can show it.
        // It is filtered from LLM input by AnthropicBackend.
        let opened = Message::new(id, Role::System, format!("Session opened: {topic}"));
        self.emit(&opened);
        session.push(opened);

        // Push the kickoff user turn. Anthropic's Messages API requires
        // `messages` to contain at least one entry whose role is
        // `user` — without this the API rejects the call with HTTP
        // 400. Recorded in the session log so it round-trips through
        // serde + the hook; surfaces typically suppress it from the
        // operator-visible thread.
        let kickoff = Message::new(id, Role::Operator, kickoff_user_turn());
        self.emit(&kickoff);
        session.push(kickoff);

        let system_prompt = planning_system_prompt(&topic);
        let opening = self
            .backend
            .respond(&system_prompt, &session.messages)
            .await?;

        let partner_msg = Message::new(id, Role::Partner, opening);
        self.emit(&partner_msg);
        session.push(partner_msg);

        info!(
            session = %id.0,
            turns = session.messages.len(),
            "evy-thinking: session opened",
        );

        self.sessions.lock().await.insert(id, session);
        Ok(id)
    }

    /// Append an operator turn and produce the partner's reply.
    ///
    /// # Errors
    /// - [`ThinkingError::UnknownSession`] if `id` is unknown.
    /// - [`ThinkingError::Input`] if `operator_input` is blank.
    /// - [`ThinkingError::BackendRefused`] if the session is not
    ///   [`SessionStatus::Active`].
    /// - Any error from the underlying backend's `respond` call.
    pub async fn send(&self, id: SessionId, operator_input: String) -> Result<String> {
        if operator_input.trim().is_empty() {
            return Err(ThinkingError::Input("operator_input is empty".to_string()));
        }

        // Step 1: snapshot the prompt + message history under the lock,
        // bumping the operator turn into the session and emitting the
        // hook. We DO drop the guard before the backend call — the
        // backend can take seconds and we don't want to block other
        // sessions on it. (Per session, that's still serialised because
        // each `send` re-acquires the lock for the post-step.)
        let (system_prompt, history) = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            if session.status != SessionStatus::Active {
                return Err(ThinkingError::BackendRefused(format!(
                    "session is {:?}, not Active",
                    session.status
                )));
            }
            let op_msg = Message::new(id, Role::Operator, operator_input);
            // Emit the hook *before* releasing the lock: callers (the
            // daemon) treat the hook as the durable record. If the
            // hook spawns an async task and that task races the next
            // send(), it's still ordered correctly because we serialise
            // hook invocations on the sessions mutex.
            self.emit(&op_msg);
            session.push(op_msg);
            let prompt = planning_system_prompt(&session.topic);
            (prompt, session.messages.clone())
        };

        debug!(session = %id.0, turns = history.len(), "evy-thinking: send");

        // Step 2: backend call WITHOUT the lock held.
        let reply = self.backend.respond(&system_prompt, &history).await?;

        // Step 3: re-acquire to append the partner turn. Re-check the
        // session still exists — conclude() may have closed it while
        // the backend was thinking.
        {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            let partner_msg = Message::new(id, Role::Partner, reply.clone());
            self.emit(&partner_msg);
            session.push(partner_msg);
        }

        Ok(reply)
    }

    /// Streaming variant of [`start_session`](Self::start_session).
    ///
    /// Opens a fresh planning session, drives the backend's streaming
    /// response into `sink`, and once the full opening turn has been
    /// emitted appends it to the session log + fires the message hook.
    /// Returns the freshly-minted [`SessionId`] so the HTTP layer can
    /// include it in the SSE `done` frame.
    ///
    /// # Errors
    /// Same as [`start_session`](Self::start_session).
    pub async fn stream_start_session(
        &self,
        topic: String,
        sink: &mpsc::Sender<StreamChunk>,
    ) -> Result<SessionId> {
        if topic.trim().is_empty() {
            return Err(ThinkingError::Input("topic is empty".to_string()));
        }

        let mut session = Session::new(topic.clone());
        let id = session.id;

        let opened = Message::new(id, Role::System, format!("Session opened: {topic}"));
        self.emit(&opened);
        session.push(opened);

        let kickoff = Message::new(id, Role::Operator, kickoff_user_turn());
        self.emit(&kickoff);
        session.push(kickoff);

        let system_prompt = planning_system_prompt(&topic);
        let opening = self
            .backend
            .stream_respond(&system_prompt, &session.messages, sink)
            .await?;

        let partner_msg = Message::new(id, Role::Partner, opening);
        self.emit(&partner_msg);
        session.push(partner_msg);

        info!(
            session = %id.0,
            turns = session.messages.len(),
            "evy-thinking: streaming session opened",
        );

        self.sessions.lock().await.insert(id, session);
        Ok(id)
    }

    /// Streaming variant of [`send`](Self::send).
    ///
    /// Same lock-discipline as the blocking path:
    /// 1. acquire lock → push operator turn → snapshot history → drop
    /// 2. backend streams chunks into `sink`, returning assembled text
    /// 3. re-acquire lock → push partner turn → drop
    ///
    /// # Errors
    /// Same as [`send`](Self::send).
    pub async fn stream_send(
        &self,
        id: SessionId,
        operator_input: String,
        sink: &mpsc::Sender<StreamChunk>,
    ) -> Result<String> {
        if operator_input.trim().is_empty() {
            return Err(ThinkingError::Input("operator_input is empty".to_string()));
        }

        let (system_prompt, history) = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            if session.status != SessionStatus::Active {
                return Err(ThinkingError::BackendRefused(format!(
                    "session is {:?}, not Active",
                    session.status
                )));
            }
            let op_msg = Message::new(id, Role::Operator, operator_input);
            self.emit(&op_msg);
            session.push(op_msg);
            let prompt = planning_system_prompt(&session.topic);
            (prompt, session.messages.clone())
        };

        debug!(session = %id.0, turns = history.len(), "evy-thinking: stream_send");

        let reply = self
            .backend
            .stream_respond(&system_prompt, &history, sink)
            .await?;

        {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            let partner_msg = Message::new(id, Role::Partner, reply.clone());
            self.emit(&partner_msg);
            session.push(partner_msg);
        }

        Ok(reply)
    }

    /// Conclude a session — appends the conclusion user turn, asks the
    /// backend for PHASE 3 final summary, marks the session
    /// [`SessionStatus::Concluded`].
    ///
    /// # Errors
    /// - [`ThinkingError::UnknownSession`] if `id` is unknown.
    /// - [`ThinkingError::BackendRefused`] if the session is not
    ///   [`SessionStatus::Active`].
    /// - Any error from the underlying backend's `respond` call. On
    ///   backend failure the session is left
    ///   [`SessionStatus::Active`] so the operator can retry.
    pub async fn conclude(&self, id: SessionId) -> Result<()> {
        let (system_prompt, history) = {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            if session.status != SessionStatus::Active {
                return Err(ThinkingError::BackendRefused(format!(
                    "session is {:?}, cannot conclude",
                    session.status
                )));
            }
            let conclude_msg = Message::new(id, Role::Operator, conclusion_user_turn());
            self.emit(&conclude_msg);
            session.push(conclude_msg);
            let prompt = planning_system_prompt(&session.topic);
            (prompt, session.messages.clone())
        };

        let summary = self.backend.respond(&system_prompt, &history).await?;

        {
            let mut guard = self.sessions.lock().await;
            let session = guard
                .get_mut(&id)
                .ok_or(ThinkingError::UnknownSession(id))?;
            let partner_msg = Message::new(id, Role::Partner, summary);
            self.emit(&partner_msg);
            session.push(partner_msg);
            session.status = SessionStatus::Concluded;
            info!(
                session = %id.0,
                turns = session.messages.len(),
                "evy-thinking: session concluded",
            );
        }

        Ok(())
    }

    /// Fetch a snapshot of one session. Returns `None` if the id is
    /// unknown.
    ///
    /// # Errors
    /// Currently infallible (returns `Ok(None)` for unknown ids), but
    /// kept as `Result` so persistence-backed implementations later
    /// can surface I/O errors.
    pub async fn session(&self, id: SessionId) -> Result<Option<Session>> {
        Ok(self.sessions.lock().await.get(&id).cloned())
    }

    /// List every session the partner currently holds, newest-active
    /// first. Returns a clone so the caller can iterate without holding
    /// the lock.
    ///
    /// # Errors
    /// Currently infallible; kept as `Result` for forward-compatibility
    /// with persistence-backed implementations.
    pub async fn list_sessions(&self) -> Result<Vec<Session>> {
        let guard = self.sessions.lock().await;
        let mut out: Vec<Session> = guard.values().cloned().collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.last_activity));
        Ok(out)
    }

    /// Drop a session from the in-memory store.
    ///
    /// Returns `true` when the id matched an entry that was removed;
    /// `false` when the id was unknown (no-op). Phase 6 — the HTTP
    /// `DELETE /api/evy/sessions/:id` handler uses this signal to
    /// return 204 vs. 404. The on-disk learning-loop record (if a
    /// message hook is wired) is **not** touched — drop only evicts
    /// the in-memory replay buffer.
    ///
    /// # Errors
    /// Currently infallible; kept as `Result` so a persistence-backed
    /// future implementation can surface I/O failures.
    pub async fn drop_session(&self, id: SessionId) -> Result<bool> {
        Ok(self.sessions.lock().await.remove(&id).is_some())
    }

    fn emit(&self, msg: &Message) {
        if let Some(hook) = &self.on_message {
            hook(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Each backend call captures (system_prompt, full message log).
    type Capture = (String, Vec<Message>);
    type CaptureLog = Arc<StdMutex<Vec<Capture>>>;

    /// A backend that returns canned strings in order. Captures the
    /// system prompt + full message history from each call for
    /// assertions.
    struct ScriptedBackend {
        replies: StdMutex<Vec<String>>,
        captures: CaptureLog,
    }

    impl ScriptedBackend {
        fn new(replies: Vec<&str>) -> (Arc<Self>, CaptureLog) {
            let captures: CaptureLog = Arc::new(StdMutex::new(Vec::new()));
            let b = Arc::new(Self {
                replies: StdMutex::new(replies.into_iter().map(String::from).collect()),
                captures: captures.clone(),
            });
            (b, captures)
        }
    }

    #[async_trait]
    impl LlmBackend for ScriptedBackend {
        async fn respond(&self, system_prompt: &str, messages: &[Message]) -> Result<String> {
            self.captures
                .lock()
                .unwrap()
                .push((system_prompt.to_string(), messages.to_vec()));
            let mut q = self.replies.lock().unwrap();
            if q.is_empty() {
                Err(ThinkingError::BackendRefused(
                    "scripted backend ran out of replies".to_string(),
                ))
            } else {
                Ok(q.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn start_session_records_opening_clarifying_questions() {
        let (backend, captures) = ScriptedBackend::new(vec![
            "1. What is the migration target?\n2. What's the downtime budget?\n3. Who owns the cutover?",
        ]);
        let partner = ThinkingPartner::new(backend);
        let id = partner
            .start_session("brownfield migration".into())
            .await
            .unwrap();

        let session = partner.session(id).await.unwrap().expect("session exists");
        assert_eq!(session.status, SessionStatus::Active);
        assert_eq!(
            session.messages.len(),
            3,
            "system marker + kickoff user turn + partner opening"
        );
        assert_eq!(session.messages[0].role, Role::System);
        assert_eq!(session.messages[1].role, Role::Operator);
        assert_eq!(session.messages[2].role, Role::Partner);
        assert!(session.messages[2].content.contains("migration target"));

        let caps = captures.lock().unwrap();
        assert_eq!(caps.len(), 1, "one backend call to open the session");
        assert!(caps[0].0.contains("brownfield migration"));
        // The history sent to the backend at start includes the
        // System scaffolding row + the kickoff Operator turn.
        // AnthropicBackend filters the System row, leaving exactly one
        // `user`-role wire message — the API minimum.
        assert_eq!(caps[0].1.len(), 2);
        assert_eq!(caps[0].1[0].role, Role::System);
        assert_eq!(caps[0].1[1].role, Role::Operator);
    }

    #[tokio::test]
    async fn start_session_rejects_empty_topic() {
        let (backend, _) = ScriptedBackend::new(vec![]);
        let partner = ThinkingPartner::new(backend);
        let err = partner
            .start_session("   ".into())
            .await
            .expect_err("must fail");
        assert!(matches!(err, ThinkingError::Input(_)));
    }

    #[tokio::test]
    async fn three_turn_planning_session_roundtrip() {
        let (backend, _captures) = ScriptedBackend::new(vec![
            // Opening clarifying questions (PHASE 1).
            "1. What's the target version?\n2. Downtime budget?\n3. Who owns it?\n4. Existing tests?\n5. Rollback plan?\n\nAnswer what you can; we'll iterate.",
            // Draft plan (PHASE 2 turn 1).
            "**Goal** — migrate to PG16.\n**Unknowns** — extension list\n**Approach** — 1) audit ...\n**Risks** — downtime\n\nAnything else to refine, or shall we conclude?",
            // Refined plan (PHASE 2 turn 2).
            "**Goal** — migrate to PG16, no downtime.\n**Unknowns** — extension list\n**Approach** — 1) blue/green ...\n**Risks** — replication lag\n\nAnything else to refine, or shall we conclude?",
            // Conclusion (PHASE 3).
            "**Goal** — migrate to PG16.\n**Unknowns** — none outstanding\n**Approach** — finalised\n**Risks** — none material\n**Next steps** — 1) provision green ...",
        ]);
        let partner = ThinkingPartner::new(backend);

        let id = partner
            .start_session("postgres migration".into())
            .await
            .unwrap();
        let _ = partner
            .send(id, "Target is PG16, zero downtime.".into())
            .await
            .unwrap();
        let refined = partner
            .send(id, "We'll need blue/green.".into())
            .await
            .unwrap();
        assert!(refined.contains("blue/green"));
        partner.conclude(id).await.unwrap();

        let session = partner.session(id).await.unwrap().expect("session exists");
        assert_eq!(session.status, SessionStatus::Concluded);
        // System + kickoff_op + opening_p + (op1, p1) + (op2, p2) + (conclude_op, p3) = 9
        assert_eq!(session.messages.len(), 9);
        let last_partner = session.last_of(Role::Partner).expect("partner replied");
        assert!(last_partner.content.contains("Next steps"));
    }

    #[tokio::test]
    async fn send_on_unknown_session_returns_unknown_session() {
        let (backend, _) = ScriptedBackend::new(vec![]);
        let partner = ThinkingPartner::new(backend);
        let bogus = SessionId::new();
        let err = partner
            .send(bogus, "hi".into())
            .await
            .expect_err("must fail");
        assert!(matches!(err, ThinkingError::UnknownSession(_)));
    }

    #[tokio::test]
    async fn send_after_conclude_is_rejected() {
        let (backend, _) = ScriptedBackend::new(vec!["q?", "summary"]);
        let partner = ThinkingPartner::new(backend);
        let id = partner.start_session("t".into()).await.unwrap();
        partner.conclude(id).await.unwrap();
        let err = partner
            .send(id, "more?".into())
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, ThinkingError::BackendRefused(_)),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_input_rejected_without_backend_call() {
        let (backend, captures) = ScriptedBackend::new(vec!["q?"]);
        let partner = ThinkingPartner::new(backend);
        let id = partner.start_session("t".into()).await.unwrap();
        let err = partner.send(id, "   ".into()).await.expect_err("must fail");
        assert!(matches!(err, ThinkingError::Input(_)));
        // start_session made 1 call; send_empty must not have called.
        assert_eq!(captures.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn message_hook_fires_for_every_message() {
        let (backend, _) = ScriptedBackend::new(vec!["q?", "draft", "summary"]);
        let captured: Arc<StdMutex<Vec<(Role, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let hook: MessageHook = Arc::new(move |m: &Message| {
            captured_clone
                .lock()
                .unwrap()
                .push((m.role, m.content.clone()));
        });
        let partner = ThinkingPartner::new(backend).with_message_hook(hook);
        let id = partner.start_session("t".into()).await.unwrap();
        partner.send(id, "hi".into()).await.unwrap();
        partner.conclude(id).await.unwrap();

        let log = captured.lock().unwrap();
        // System + Operator(kickoff) + Partner(open) + Operator(send)
        //   + Partner + Operator(conclude) + Partner = 7
        assert_eq!(log.len(), 7);
        assert_eq!(log[0].0, Role::System);
        assert_eq!(log[1].0, Role::Operator); // kickoff
        assert_eq!(log[2].0, Role::Partner); // opening clarifying questions
        assert_eq!(log[3].0, Role::Operator); // operator send
        assert_eq!(log[4].0, Role::Partner);
        assert_eq!(log[5].0, Role::Operator); // conclusion turn
        assert_eq!(log[6].0, Role::Partner);
    }

    #[tokio::test]
    async fn drop_session_removes_known_id_and_reports_true() {
        let (backend, _) = ScriptedBackend::new(vec!["q?"]);
        let partner = ThinkingPartner::new(backend);
        let id = partner.start_session("t".into()).await.unwrap();
        let ok = partner.drop_session(id).await.unwrap();
        assert!(ok, "first drop must report true");
        // Session no longer reachable.
        let s = partner.session(id).await.unwrap();
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn drop_session_unknown_id_returns_false() {
        let (backend, _) = ScriptedBackend::new(vec![]);
        let partner = ThinkingPartner::new(backend);
        let bogus = SessionId::new();
        let ok = partner.drop_session(bogus).await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn list_sessions_sorts_newest_active_first() {
        let (backend, _) = ScriptedBackend::new(vec!["q1?", "q2?", "q3?"]);
        let partner = ThinkingPartner::new(backend);
        let a = partner.start_session("first".into()).await.unwrap();
        // Brief sleep to ensure distinct last_activity timestamps.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let b = partner.start_session("second".into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let c = partner.start_session("third".into()).await.unwrap();

        let list = partner.list_sessions().await.unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, c);
        assert_eq!(list[1].id, b);
        assert_eq!(list[2].id, a);
    }
}
