//! herdr's own Gram entries about its local agents, so Gram is the owner's one
//! inbox: `blocked` (the agent's question; resolved once it moves on), `recap`
//! (a finished turn's recap or final reply) and `died` (its process exited).
//!
//! They follow the transitions that make agent alerts (needs input, finished,
//! exited) but never push, and are written whether or not push is set up. They
//! are the owner's alone: the agent and guest Gram views and the gram push leave
//! out any entry with a `kind`. Subagents (`codex exec`) get none, and remote
//! agents are left to their own machine.
//!
//! The app loop only decides what happened. Store I/O runs on one writer thread
//! per server, in order, so a wait and its resolution cannot land out of order;
//! a recap's transcript is read on a thread of its own first.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use crate::app::App;
use crate::detect::{Agent, AgentState};
use crate::persist::gram::{GramDirection, GramEvent, GramEventKind, GramItem};

/// A blocked entry's text when the agent's question cannot be read off its screen.
const WAITING_TEXT: &str = "Waiting for your answer";
/// A recap's text when no reply can be read for the turn.
const FINISHED_TEXT: &str = "Finished a turn";
const DIED_TEXT: &str = "Process exited";
/// Longest entry text, in characters: a recap is a glance, not the transcript.
const MAX_TEXT_CHARS: usize = 800;
/// Waits between reads of a just-finished turn's transcript. The harness may
/// flush the reply a moment after its screen goes idle, and omp writes its
/// recap after the turn.
#[cfg_attr(test, allow(dead_code))]
const RECAP_READ_PAUSES: [Duration; 8] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(3),
    Duration::from_secs(3),
];

/// One piece of work for the store.
#[derive(Debug)]
pub(crate) enum GramJob {
    Event(GramEvent),
    /// A finished turn whose recap must be read before it is written.
    Recap(RecapRequest),
}

/// A recap to write once its text is read.
#[derive(Debug)]
pub(crate) struct RecapRequest {
    /// The entry, its text still empty.
    pub(crate) item: GramItem,
    pub(crate) agent: Option<Agent>,
    pub(crate) session_file: Option<PathBuf>,
    /// What the agent's hook reported with the turn, if anything.
    pub(crate) reported: Option<String>,
}

/// What produced a batch of pane state updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GramTrigger {
    /// The agents' detected or reported state changed.
    StateChange,
    /// The pane's process exited on its own.
    PaneDeath,
    /// herdr shut the pane's runtime down itself (a restart, archive or
    /// session transfer): its agent is gone, but nothing died.
    Shutdown,
}

impl App {
    /// Record the owner's Gram entries for these agent transitions. A sibling
    /// of `emit_apns_agent_notifications`, called from the same sites with the
    /// same updates.
    pub(super) fn record_gram_events(
        &self,
        pane_updates: &[crate::app::actions::PaneStateUpdate],
        trigger: GramTrigger,
    ) {
        // Tests and a server handing off to its replacement do not persist.
        if self.no_session || !self.policy.persist_session || pane_updates.is_empty() {
            return;
        }
        let from_pane_death = trigger != GramTrigger::StateChange;
        let session = session_name();
        let mut jobs = Vec::new();
        for update in pane_updates {
            let Some(pane_id) = self.public_pane_id(update.ws_idx, update.pane_id) else {
                continue;
            };
            if update.state != AgentState::Blocked || from_pane_death {
                // The writer drops this unless the pane has an open blocked entry.
                jobs.push(GramJob::Event(GramEvent::Unblocked {
                    session: session.clone(),
                    pane_id: pane_id.clone(),
                }));
            }
            let toast =
                crate::app::actions::notification_toast_for_pane_state_update(false, update);
            let Some(kind) = super::push_kind_for(from_pane_death, toast) else {
                continue;
            };
            let Some((pane, terminal)) = self
                .state
                .workspaces
                .get(update.ws_idx)
                .and_then(|ws| ws.pane_state(update.pane_id))
                .and_then(|pane| {
                    let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
                    Some((pane, terminal))
                })
            else {
                continue;
            };
            if terminal.is_subagent() {
                continue;
            }
            // An exited agent no longer has an effective label; the update still
            // names what ran there.
            let Some(agent_label) = terminal
                .effective_agent_label()
                .or(update.agent_label.as_deref())
                .or(update.previous_agent_label.as_deref())
            else {
                continue;
            };
            let from = super::push_title_agent(terminal.agent_name.as_deref(), agent_label);
            let mut item = event_item(from, pane_id, session.clone());
            match kind {
                crate::push::PushKind::NeedsInput => {
                    let question = self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .and_then(|runtime| {
                            crate::detect::pending_request::pending_request(&runtime.visible_text())
                        });
                    item.kind = Some(GramEventKind::Blocked);
                    item.text = clip(question.as_deref().unwrap_or(WAITING_TEXT));
                    jobs.push(GramJob::Event(GramEvent::Blocked(item)));
                }
                crate::push::PushKind::Finished => {
                    item.kind = Some(GramEventKind::Recap);
                    jobs.push(GramJob::Recap(RecapRequest {
                        item,
                        agent: terminal.effective_known_agent(),
                        session_file: self.agent_session_file(terminal),
                        reported: update
                            .completed_turn
                            .as_ref()
                            .and_then(|turn| turn.message.clone()),
                    }));
                }
                crate::push::PushKind::Died if trigger == GramTrigger::Shutdown => {}
                crate::push::PushKind::Died => {
                    item.kind = Some(GramEventKind::Died);
                    item.text = DIED_TEXT.to_owned();
                    jobs.push(GramJob::Event(GramEvent::Append(item)));
                }
                crate::push::PushKind::Gram => {}
            }
        }
        dispatch(jobs);
    }

    /// A pane that exited or closed leaves no wait to answer, also when its agent
    /// was already gone from the state (no update to record).
    pub(super) fn resolve_gram_wait(&self, pane_id: &str) {
        if self.no_session || !self.policy.persist_session {
            return;
        }
        dispatch(vec![GramJob::Event(GramEvent::Unblocked {
            session: session_name(),
            pane_id: pane_id.to_owned(),
        })]);
    }

    /// The session file a terminal's agent reported: a path session ref, or the
    /// path reported beside an id ref (Claude Code, Codex).
    pub(super) fn agent_session_file(
        &self,
        terminal: &crate::terminal::TerminalState,
    ) -> Option<PathBuf> {
        let (source, agent, session_ref) = Self::terminal_resume_source(terminal)?;
        terminal
            .reported_agent_session_path_for(&source, &agent, &session_ref)
            .map(PathBuf::from)
            .or_else(|| {
                (session_ref.kind == crate::agent_resume::AgentSessionRefKind::Path)
                    .then(|| PathBuf::from(&session_ref.value))
            })
            .or_else(|| {
                // Claude reports its transcript path only at SessionStart, which a server
                // that took over by live handoff or restart never saw: find it by id.
                // ponytail: Claude only; codex's dated rollout tree is not searched.
                (agent == "claude"
                    && session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id)
                    .then(|| self.find_claude_transcript(&session_ref.value))
                    .flatten()
            })
    }

    /// `<config home>/projects/*/<id>.jsonl` under the default Claude config home or
    /// any configured Claude account's.
    fn find_claude_transcript(&self, id: &str) -> Option<PathBuf> {
        crate::config::default_config_dir("claude")
            .into_iter()
            .chain(
                self.loaded_accounts
                    .iter()
                    .filter(|account| account.kind == "claude")
                    .map(|account| PathBuf::from(&account.config_dir)),
            )
            .find_map(|home| super::agents::find_claude_session_file(&home, id))
            .map(|(file, _)| file)
    }
}

/// This server's herdr session, as clients name it.
fn session_name() -> String {
    crate::session::active_name().unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.into())
}

/// An agent-to-owner entry about the agent `from` at `pane_id`; the caller sets
/// its kind and text.
fn event_item(from: &str, pane_id: String, session: String) -> GramItem {
    GramItem {
        id: crate::persist::gram::new_id(),
        direction: GramDirection::AgentToOwner,
        from: from.chars().take(64).collect(),
        to: None,
        text: String::new(),
        grabbed_by: None,
        grabbed_unix_ms: None,
        created_unix_ms: super::unix_millis_now(),
        read_by_owner: false,
        file: None,
        expired_file: None,
        origin_id: crate::persist::machine::get_or_create(),
        // No sender binding: an event entry is never a guest's to see.
        sender: None,
        kind: None,
        pane_id: Some(pane_id),
        session: Some(session),
        resolved: false,
        source_key: None,
    }
}

/// `text` trimmed and cut to [`MAX_TEXT_CHARS`] on a character boundary.
fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

/// The recap's text and the source it was read from. Each read that finds no
/// finished turn yet waits the next of `pauses` and tries again; when they run
/// out, the best text found stands, else the hook's report, else a fallback.
fn recap_text(request: &RecapRequest, pauses: &[Duration]) -> (String, Option<String>) {
    let read = |path: &std::path::Path| -> Option<(String, String, bool)> {
        match request.agent? {
            Agent::Omp => crate::agent_transcript::recap(path)
                .ok()
                .flatten()
                .map(|recap| (recap.text, format!("omp:{}", recap.turn_id), true)),
            Agent::Claude => crate::agent_transcript::reply::claude(path)
                .ok()
                .flatten()
                .map(|reply| (reply.text, reply.key, reply.complete)),
            Agent::Codex => crate::agent_transcript::reply::codex(path)
                .ok()
                .flatten()
                .map(|reply| (reply.text, reply.key, reply.complete)),
            _ => None,
        }
    };
    let mut best = None;
    if let Some(path) = request.session_file.as_deref() {
        let mut pauses = pauses.iter();
        loop {
            if let Some((text, key, complete)) = read(path) {
                best = Some((text, key));
                if complete {
                    break;
                }
            }
            let Some(pause) = pauses.next() else {
                break;
            };
            std::thread::sleep(*pause);
        }
    }
    match best {
        Some((text, key)) if !text.trim().is_empty() => (clip(&text), Some(key)),
        _ => (
            clip(
                request
                    .reported
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(FINISHED_TEXT),
            ),
            None,
        ),
    }
}

/// Hand jobs to this server's writer; recaps read their transcript first.
fn dispatch(jobs: Vec<GramJob>) {
    if jobs.is_empty() {
        return;
    }
    #[cfg(test)]
    {
        test_sink::capture(jobs);
    }
    #[cfg(not(test))]
    for job in jobs {
        match job {
            GramJob::Event(event) => send(event),
            GramJob::Recap(request) => {
                let spawned = crate::thread_spawn::spawn_named("herdr-gram-recap", move || {
                    let (text, source_key) = recap_text(&request, &RECAP_READ_PAUSES);
                    let mut item = request.item;
                    item.text = text;
                    item.source_key = source_key;
                    send(GramEvent::Append(item));
                });
                if let Err(err) = spawned {
                    tracing::warn!(%err, "failed to start the gram recap reader");
                }
            }
        }
    }
}

/// Queue one event on the writer thread, starting it on first use.
#[cfg_attr(test, allow(dead_code))]
fn send(event: GramEvent) {
    static WRITER: OnceLock<Option<mpsc::Sender<GramEvent>>> = OnceLock::new();
    let writer = WRITER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        match crate::thread_spawn::spawn_named("herdr-gram-events", move || write_events(rx)) {
            Ok(_) => Some(tx),
            Err(err) => {
                tracing::warn!(%err, "failed to start the gram event writer");
                None
            }
        }
    });
    if let Some(writer) = writer {
        let _ = writer.send(event);
    }
}

/// The writer: applies queued events in order, each burst in one locked
/// read-modify-write. It remembers which of this session's panes have an open
/// blocked entry (seeded from the store, so a wait left open by an earlier run
/// still resolves) and drops the resolve for any other pane without touching
/// the file.
#[cfg_attr(test, allow(dead_code))]
fn write_events(events: mpsc::Receiver<GramEvent>) {
    let session = session_name();
    let mut open: HashSet<String> =
        crate::persist::gram::open_blocked_panes(&crate::persist::gram::load(), &session)
            .into_iter()
            .collect();
    while let Ok(first) = events.recv() {
        let burst: Vec<GramEvent> = std::iter::once(first)
            .chain(events.try_iter())
            .filter(|event| match event {
                GramEvent::Blocked(item) => {
                    open.extend(item.pane_id.clone());
                    true
                }
                GramEvent::Unblocked { pane_id, .. } => open.remove(pane_id),
                GramEvent::Append(_) => true,
            })
            .collect();
        if burst.is_empty() {
            continue;
        }
        if let Err(err) = crate::persist::gram::record_events(burst) {
            tracing::warn!(%err, "failed to record gram events");
        }
    }
}

#[cfg(test)]
pub(crate) mod test_sink {
    use std::cell::RefCell;

    use super::GramJob;

    thread_local! {
        static SINK: RefCell<Option<Vec<GramJob>>> = const { RefCell::new(None) };
    }

    /// Records into the installed sink; without one the jobs are dropped, so a
    /// test never writes the real store.
    pub(super) fn capture(jobs: Vec<GramJob>) {
        SINK.with(|sink| {
            if let Some(captured) = sink.borrow_mut().as_mut() {
                captured.extend(jobs);
            }
        });
    }

    /// Captures this thread's jobs until dropped.
    pub(crate) struct Capture(());

    impl Capture {
        pub(crate) fn install() -> Self {
            SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
            Self(())
        }

        pub(crate) fn take(&self) -> Vec<GramJob> {
            SINK.with(|sink| sink.borrow_mut().as_mut().map(std::mem::take))
                .unwrap_or_default()
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            SINK.with(|sink| *sink.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(agent: Option<Agent>, session_file: Option<PathBuf>) -> RecapRequest {
        RecapRequest {
            item: event_item("claude", "w1:p1".into(), "default".into()),
            agent,
            session_file,
            reported: None,
        }
    }

    fn session_file(tag: &str, lines: &[serde_json::Value]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-gram-recap-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        path
    }

    /// An app whose one pane runs `agent`, detected as a subagent or not, with
    /// alerts going to a direct-push device.
    fn agent_app(agent: Agent, subagent: bool) -> (App, crate::layout::PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.policy.persist_session = true;
        app.state.push_config = crate::config::PushConfig {
            mode: crate::config::PushMode::Direct,
            enabled: true,
            key_path: Some("/tmp/AuthKey.p8".to_string()),
            key_id: Some("ABC123DEFG".to_string()),
            team_id: Some("TEAM123456".to_string()),
            topic: Some("app.herdr.ios".to_string()),
            ..crate::config::PushConfig::default()
        };
        let workspace = crate::workspace::Workspace::test_new("gram-events");
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            runtime_epoch: None,
            agent,
            subagent,
            observed_at: std::time::Instant::now(),
        });
        (app, pane_id)
    }

    fn observe(app: &mut App, pane_id: crate::layout::PaneId, agent: Agent, state: AgentState) {
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            runtime_epoch: None,
            agent: Some(agent),
            state,
            visible_blocker: state == AgentState::Blocked,
            visible_working: state == AgentState::Working,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
    }

    /// What each captured job would write: `blocked`, `unblocked`, `recap`, `died`.
    fn summary(jobs: Vec<GramJob>) -> Vec<String> {
        jobs.into_iter()
            .map(|job| match job {
                GramJob::Event(GramEvent::Blocked(item)) => {
                    format!(
                        "blocked {} {} {}",
                        item.from,
                        item.text,
                        item.session.unwrap()
                    )
                }
                GramJob::Event(GramEvent::Unblocked { pane_id, .. }) => {
                    format!("unblocked {pane_id}")
                }
                GramJob::Event(GramEvent::Append(item)) => {
                    format!("{:?} {}", item.kind.unwrap(), item.text).to_lowercase()
                }
                GramJob::Recap(request) => {
                    format!("recap {} {:?}", request.item.from, request.agent)
                }
            })
            .collect()
    }

    #[test]
    fn a_main_agents_wait_and_finish_become_owner_entries() {
        let (mut app, pane_id) = agent_app(Agent::Claude, false);
        let pane = app.public_pane_id(0, pane_id).unwrap();
        let jobs = test_sink::Capture::install();
        let alerts = crate::push::test_sink::Capture::install();

        observe(&mut app, pane_id, Agent::Claude, AgentState::Working);
        observe(&mut app, pane_id, Agent::Claude, AgentState::Blocked);
        observe(&mut app, pane_id, Agent::Claude, AgentState::Blocked);
        observe(&mut app, pane_id, Agent::Claude, AgentState::Working);
        observe(&mut app, pane_id, Agent::Claude, AgentState::Idle);

        assert_eq!(
            summary(jobs.take()),
            vec![
                format!("unblocked {pane}"),
                format!("blocked claude {WAITING_TEXT} {}", session_name()),
                format!("unblocked {pane}"),
                format!("unblocked {pane}"),
                "recap claude Some(Claude)".to_string(),
            ]
        );
        let kinds: Vec<_> = alerts
            .take()
            .alerts
            .iter()
            .map(|alert| alert.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                crate::push::PushKind::NeedsInput,
                crate::push::PushKind::Finished
            ],
            "the alerts are unchanged, and no gram alert joins them"
        );
        let info = app.agent_info(0, pane_id).unwrap();
        assert!(!info.subagent);
        assert!(serde_json::to_value(&info)
            .unwrap()
            .get("subagent")
            .is_none());
    }

    #[test]
    fn a_codex_exec_subagent_is_listed_but_otherwise_hidden() {
        let (mut app, pane_id) = agent_app(Agent::Codex, true);
        let jobs = test_sink::Capture::install();
        let alerts = crate::push::test_sink::Capture::install();

        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Working,
            AgentState::Idle,
        ] {
            observe(&mut app, pane_id, Agent::Codex, state);
        }

        assert!(
            summary(jobs.take())
                .iter()
                .all(|job| job.starts_with("unblocked")),
            "no entry for a subagent"
        );
        let sent = alerts.take();
        assert!(sent.alerts.is_empty(), "{:?}", sent.alerts);

        // agent.list keeps it, flagged.
        let info = app.agent_info(0, pane_id).unwrap();
        assert!(info.subagent);
        assert_eq!(serde_json::to_value(&info).unwrap()["subagent"], true);
        // The agent panel and the Live Activity leave it out.
        assert!(crate::ui::agent_panel_entries_from(&app.state, &app.terminal_runtimes).is_empty());
        let state = super::super::live_activity_content_state(&app.collect_agent_infos());
        assert_eq!(state["totalCount"], 0);
        assert_eq!(state["headline"], "No agents");
        // No local toast either.
        assert!(app.state.toast.is_none());
    }

    #[test]
    fn interactive_codex_is_a_main_agent() {
        let (mut app, pane_id) = agent_app(Agent::Codex, false);
        let jobs = test_sink::Capture::install();
        observe(&mut app, pane_id, Agent::Codex, AgentState::Working);
        observe(&mut app, pane_id, Agent::Codex, AgentState::Idle);
        assert!(summary(jobs.take()).contains(&"recap codex Some(Codex)".to_string()));
        assert_eq!(
            crate::ui::agent_panel_entries_from(&app.state, &app.terminal_runtimes).len(),
            1
        );
    }

    #[test]
    fn closing_a_waiting_agents_pane_resolves_its_wait() {
        let (mut app, pane_id) = agent_app(Agent::Codex, false);
        let pane = app.public_pane_id(0, pane_id).unwrap();
        observe(&mut app, pane_id, Agent::Codex, AgentState::Blocked);
        let jobs = test_sink::Capture::install();

        let response = app.handle_api_request(crate::api::schema::Request {
            id: "close".into(),
            method: crate::api::schema::Method::PaneClose(crate::api::schema::PaneTarget {
                pane_id: pane.clone(),
            }),
        });

        assert!(response.contains(r#""type":"ok""#), "{response}");
        assert_eq!(summary(jobs.take()), vec![format!("unblocked {pane}")]);
    }

    #[tokio::test]
    async fn an_agents_exit_resolves_its_wait_and_notes_the_death() {
        for subagent in [false, true] {
            let (mut app, pane_id) = agent_app(Agent::Codex, subagent);
            let pane = app.public_pane_id(0, pane_id).unwrap();
            observe(&mut app, pane_id, Agent::Codex, AgentState::Working);
            observe(&mut app, pane_id, Agent::Codex, AgentState::Blocked);
            let jobs = test_sink::Capture::install();
            let alerts = crate::push::test_sink::Capture::install();

            app.handle_internal_event(crate::events::AppEvent::PaneDied {
                pane_id,
                runtime_epoch: None,
                exit_reason: crate::platform::ChildExitReason::Exited,
            });

            // The exit update resolves the wait, and so does the pane's exit event,
            // which also covers an agent the state had already let go; the writer
            // drops the second resolve.
            let unblocked = format!("unblocked {pane}");
            let expected = if subagent {
                vec![unblocked.clone(), unblocked]
            } else {
                vec![
                    unblocked.clone(),
                    format!("died {DIED_TEXT}").to_lowercase(),
                    unblocked,
                ]
            };
            assert_eq!(summary(jobs.take()), expected, "subagent: {subagent}");
            // A screen-detected agent's exit has no label left for an alert, so
            // the died push (unchanged here) sends nothing either way.
            assert!(alerts.take().alerts.is_empty());
            for (_, runtime) in app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
        }
    }

    #[test]
    fn nothing_is_recorded_without_a_persisting_session() {
        let (mut app, pane_id) = agent_app(Agent::Claude, false);
        app.policy.persist_session = false;
        let jobs = test_sink::Capture::install();
        observe(&mut app, pane_id, Agent::Claude, AgentState::Working);
        observe(&mut app, pane_id, Agent::Claude, AgentState::Blocked);
        assert!(jobs.take().is_empty());
    }

    #[test]
    fn clip_cuts_long_text_on_a_char_boundary() {
        assert_eq!(clip("  short  "), "short");
        let long = "é".repeat(MAX_TEXT_CHARS + 50);
        let clipped = clip(&long);
        assert_eq!(clipped.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn a_claude_recap_is_the_turns_final_reply() {
        let path = session_file(
            "claude",
            &[
                serde_json::json!({"type": "user", "message": {"role": "user", "content": "go"}}),
                serde_json::json!({"type": "assistant", "uuid": "u1", "message": {"id": "m1", "role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": "All green."}]}}),
            ],
        );
        assert_eq!(
            recap_text(&request(Some(Agent::Claude), Some(path)), &[]),
            ("All green.".to_string(), Some("claude:u1".to_string()))
        );
    }

    #[test]
    fn a_codex_recap_is_its_last_agent_message_clipped() {
        let reply = "x".repeat(2000);
        let path = session_file(
            "codex",
            &[
                serde_json::json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t9", "last_agent_message": reply}}),
            ],
        );
        let (text, key) = recap_text(&request(Some(Agent::Codex), Some(path)), &[]);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1);
        assert_eq!(key.as_deref(), Some("codex:t9"));
    }

    #[test]
    fn an_unreadable_turn_still_gets_a_recap() {
        assert_eq!(
            recap_text(&request(Some(Agent::Claude), None), &[]),
            (FINISHED_TEXT.to_string(), None)
        );
        let missing = std::env::temp_dir().join("herdr-gram-recap-missing/session.jsonl");
        assert_eq!(
            recap_text(&request(Some(Agent::Codex), Some(missing)), &[]),
            (FINISHED_TEXT.to_string(), None)
        );
        let mut reported = request(Some(Agent::Pi), None);
        reported.reported = Some("Shipped it.".into());
        assert_eq!(
            recap_text(&reported, &[]),
            ("Shipped it.".to_string(), None)
        );
    }
}
