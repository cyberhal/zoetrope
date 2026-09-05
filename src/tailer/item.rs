//! Portable provider-neutral replay items and ordering.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::event::{ActorId, EventKind, EventTime, Provider, SessionEvent, SessionKey};
use crate::formats::FileDecoder;
use crate::formats::claude::{ClaudeFile, decode_subagent_metadata};

#[derive(Debug, Clone)]
pub(crate) enum Timing {
    Dated(DateTime<Utc>),
    PendingStart(ActorId),
    PendingEnd(ActorId),
    Leader,
}

#[derive(Debug, Clone)]
pub struct ReplayItem {
    pub(crate) timing: Timing,
    pub event: SessionEvent,
}

impl ReplayItem {
    pub fn ts(&self) -> Option<DateTime<Utc>> {
        match self.timing {
            Timing::Dated(timestamp) => Some(timestamp),
            Timing::PendingStart(_) | Timing::PendingEnd(_) | Timing::Leader => None,
        }
    }

    pub(crate) fn new(event: SessionEvent, inherited: Option<DateTime<Utc>>) -> Self {
        let timing = match &event.time {
            EventTime::At(timestamp) => Timing::Dated(*timestamp),
            EventTime::AtAgentStart(actor) => Timing::PendingStart(actor.clone()),
            EventTime::AtAgentEnd(actor) => Timing::PendingEnd(actor.clone()),
            EventTime::Untimed => inherited.map_or(Timing::Leader, Timing::Dated),
        };
        Self { timing, event }
    }

    pub(crate) fn live(event: SessionEvent) -> Self {
        Self::new(event, None)
    }

    #[cfg(test)]
    pub(crate) fn at(timestamp: Option<DateTime<Utc>>, mut event: SessionEvent) -> Self {
        if let Some(timestamp) = timestamp {
            event.time = EventTime::At(timestamp);
        }
        Self::new(event, None)
    }
}

pub(crate) fn date_and_sort(items: &mut [ReplayItem]) {
    date_and_sort_inner(items, true);
}

pub(crate) fn date_and_sort_live(items: &mut [ReplayItem]) {
    date_and_sort_inner(items, false);
}

fn date_and_sort_inner(items: &mut [ReplayItem], complete: bool) {
    let earliest = items.iter().filter_map(ReplayItem::ts).min();
    let mut first: HashMap<ActorId, DateTime<Utc>> = HashMap::new();
    let mut last: HashMap<ActorId, DateTime<Utc>> = HashMap::new();
    for item in items.iter() {
        let Some(timestamp) = item.ts() else { continue };
        first
            .entry(item.event.actor.clone())
            .and_modify(|known| *known = (*known).min(timestamp))
            .or_insert(timestamp);
        last.entry(item.event.actor.clone())
            .and_modify(|known| *known = (*known).max(timestamp))
            .or_insert(timestamp);
    }
    for item in items.iter_mut() {
        let resolved = match &item.timing {
            Timing::PendingStart(actor) => first
                .get(actor)
                .copied()
                .or(complete.then_some(earliest).flatten())
                .map_or_else(|| Timing::PendingStart(actor.clone()), Timing::Dated),
            Timing::PendingEnd(actor) => last
                .get(actor)
                .copied()
                .or(complete.then_some(earliest).flatten())
                .map_or_else(|| Timing::PendingEnd(actor.clone()), Timing::Dated),
            timing => timing.clone(),
        };
        if let Timing::Dated(timestamp) = &resolved {
            item.event.time = EventTime::At(*timestamp);
        }
        item.timing = resolved;
    }
    items.sort_by(|left, right| {
        left.ts()
            .cmp(&right.ts())
            .then_with(|| event_rank(&left.event).cmp(&event_rank(&right.event)))
    });
}

fn event_rank(event: &SessionEvent) -> u8 {
    match event.kind {
        EventKind::AgentDiscovered(_) => 0,
        EventKind::AgentMetadata(_) | EventKind::WorkflowDeclared(_) => 1,
        _ => 2,
    }
}

/// A decoded portable session plus the state needed to decode later Claude
/// browser-directory appends without resetting provider-owned de-duplication.
#[derive(Debug)]
pub struct DecodedSession {
    pub session: SessionKey,
    pub items: Vec<ReplayItem>,
    pub info: crate::state::SessionInfo,
    pub feed: SessionFeed,
}

#[derive(Debug)]
pub struct SessionFeed {
    session: SessionKey,
    root: FileDecoder,
    claude_files: HashMap<ClaudeFile, FileDecoder>,
    first_activity: HashMap<ActorId, DateTime<Utc>>,
    last_activity: HashMap<ActorId, DateTime<Utc>>,
}

impl SessionFeed {
    /// Decode complete-line tails for a Claude browser directory. Static Codex
    /// uploads deliberately have no append mode.
    pub fn append_claude(
        &mut self,
        main: &str,
        files: &[ClaudeSessionFile<'_>],
    ) -> Option<Vec<SessionEvent>> {
        if self.session.provider != Provider::Claude {
            return None;
        }
        let mut events = decode_lines(&mut self.root, main);
        events.extend(self.decode_claude_files(files));
        Some(self.resolve_append_times(events))
    }

    fn decode_claude_files(&mut self, files: &[ClaudeSessionFile<'_>]) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        for file in files {
            if file.journal {
                let Some(workflow) = file.workflow else {
                    continue;
                };
                let source = ClaudeFile::WorkflowJournal {
                    workflow: workflow.to_owned(),
                };
                let decoder = self
                    .claude_files
                    .entry(source.clone())
                    .or_insert_with(|| FileDecoder::claude(self.session.clone(), source));
                events.extend(decode_lines(decoder, file.transcript));
                continue;
            }
            if !file.meta.trim().is_empty() {
                events.extend(decode_subagent_metadata(
                    &self.session,
                    file.agent_id,
                    file.workflow,
                    file.meta,
                ));
            }
            let source = ClaudeFile::Subagent {
                agent_id: file.agent_id.to_owned(),
                workflow: file.workflow.map(str::to_owned),
            };
            let decoder = self
                .claude_files
                .entry(source.clone())
                .or_insert_with(|| FileDecoder::claude(self.session.clone(), source));
            events.extend(decode_lines(decoder, file.transcript));
        }
        events
    }

    fn observe_times(&mut self, events: &[SessionEvent]) {
        for event in events {
            let EventTime::At(timestamp) = event.time else {
                continue;
            };
            self.first_activity
                .entry(event.actor.clone())
                .and_modify(|known| *known = (*known).min(timestamp))
                .or_insert(timestamp);
            self.last_activity
                .entry(event.actor.clone())
                .and_modify(|known| *known = (*known).max(timestamp))
                .or_insert(timestamp);
        }
    }

    fn resolve_append_times(&mut self, events: Vec<SessionEvent>) -> Vec<SessionEvent> {
        self.observe_times(&events);
        let mut ready = Vec::new();
        for mut event in events {
            let resolved = match &event.time {
                EventTime::AtAgentStart(actor) => self.first_activity.get(actor).copied(),
                EventTime::AtAgentEnd(actor) => self.last_activity.get(actor).copied(),
                _ => None,
            };
            if let Some(timestamp) = resolved {
                event.time = EventTime::At(timestamp);
            }
            ready.push(event);
        }
        ready
    }
}

/// One Claude-owned sidecar supplied by the portable browser directory feed.
pub struct ClaudeSessionFile<'a> {
    pub agent_id: &'a str,
    pub meta: &'a str,
    pub transcript: &'a str,
    pub workflow: Option<&'a str>,
    pub journal: bool,
}

/// Browser/static single-file replay with content-based format detection.
/// `claude_id` is the selected filename stem used only when Claude records do
/// not carry their own session id; Codex identity always comes from its header.
pub fn replay_from_jsonl(text: &str, claude_id: &str) -> DecodedSession {
    replay_from_session(text, &[], claude_id)
}

/// Decode a portable session and retain every per-file decoder for appends.
pub fn replay_from_session(
    main: &str,
    files: &[ClaudeSessionFile<'_>],
    claude_id: &str,
) -> DecodedSession {
    let (provider, recorded_id) = crate::formats::detect_session(main);
    let fallback = SessionKey::new(
        provider,
        recorded_id.unwrap_or_else(|| claude_id.to_owned()),
    );
    let mut root = match provider {
        Provider::Claude => FileDecoder::claude(fallback.clone(), ClaudeFile::Root),
        Provider::Codex => FileDecoder::codex(),
    };
    let mut events = decode_lines(&mut root, main);
    let session = root.session_key().unwrap_or(fallback);
    let mut feed = SessionFeed {
        session: session.clone(),
        root,
        claude_files: HashMap::new(),
        first_activity: HashMap::new(),
        last_activity: HashMap::new(),
    };
    if provider == Provider::Claude {
        events.extend(feed.decode_claude_files(files));
    }
    feed.observe_times(&events);
    let (items, info) = finish(events, &session);
    DecodedSession {
        session,
        items,
        info,
        feed,
    }
}

fn decode_lines(decoder: &mut FileDecoder, text: &str) -> Vec<SessionEvent> {
    text.lines()
        .flat_map(|line| decoder.decode_line(line))
        .collect()
}

pub(crate) fn finish(
    events: Vec<SessionEvent>,
    root: &SessionKey,
) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let mut info = crate::state::SessionInfo::new(root.provider);
    let mut items = Vec::new();
    let mut inherited: HashMap<ActorId, DateTime<Utc>> = HashMap::new();
    for event in events {
        if info.consume(&event, root) {
            continue;
        }
        let prior = inherited.get(&event.actor).copied();
        if let EventTime::At(timestamp) = event.time {
            inherited.insert(event.actor.clone(), timestamp);
        }
        items.push(ReplayItem::new(event, prior));
    }
    date_and_sort(&mut items);
    (items, info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_ledger_dates_to_the_agents_first_or_last_entry() {
        let sub = |time: &str| {
            format!(
                r#"{{"type":"user","uuid":"u","timestamp":"{time}","message":{{"role":"user","content":"x"}}}}"#
            )
        };
        let sub_text = format!(
            "{}\n{}\n",
            sub("2026-06-05T10:00:05.000Z"),
            sub("2026-06-05T10:00:15.000Z")
        );
        let journal = concat!(
            r#"{"type":"started","key":"k","agentId":"subX"}"#,
            "\n",
            r#"{"type":"result","key":"k","agentId":"subX","result":"done"}"#,
            "\n",
        );
        let subs = [
            ClaudeSessionFile {
                agent_id: "subX",
                meta: "{}",
                transcript: &sub_text,
                workflow: Some("wf"),
                journal: false,
            },
            ClaudeSessionFile {
                agent_id: "",
                meta: "",
                transcript: journal,
                workflow: Some("wf"),
                journal: true,
            },
        ];
        let DecodedSession { items, .. } = replay_from_session("", &subs, "fixture");
        let status_time = |wanted| {
            items.iter().find_map(|item| match &item.event.kind {
                EventKind::AgentStatus { agent_id, status }
                    if agent_id.0 == "subX" && *status == wanted =>
                {
                    item.ts()
                }
                _ => None,
            })
        };
        assert_eq!(
            status_time(crate::event::RecordedAgentStatus::Running),
            Some("2026-06-05T10:00:05.000Z".parse().unwrap())
        );
        assert_eq!(
            status_time(crate::event::RecordedAgentStatus::Completed),
            Some("2026-06-05T10:00:15.000Z".parse().unwrap())
        );
    }

    #[test]
    fn replay_from_jsonl_parses_orders_and_routes_noise() {
        let text = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-06-05T10:00:02.000Z","message":{"role":"user","content":"second"}}"#,
            "\n\n",
            r#"{"type":"user","uuid":"u0","timestamp":"2026-06-05T10:00:01.000Z","message":{"role":"user","content":"first"}}"#,
            "\ngarbage that should be skipped\n",
        );
        let DecodedSession { session, items, .. } = replay_from_session(text, &[], "fixture");
        assert_eq!(session, SessionKey::new(Provider::Claude, "fixture"));
        let activity: Vec<_> = items
            .iter()
            .filter(|item| {
                matches!(
                    item.event.kind,
                    EventKind::Prompt { .. } | EventKind::Activity
                )
            })
            .collect();
        assert_eq!(activity.len(), 2);
        assert!(activity[0].ts().unwrap() < activity[1].ts().unwrap());
        assert!(activity.iter().all(|item| item.event.actor.0 == "fixture"));
    }

    #[test]
    fn replay_from_session_emits_subagent_meta_and_sub_entries() {
        let sub = ClaudeSessionFile {
            agent_id: "a1000000000000001",
            meta: r#"{"agentType":"Explore","description":"map it","toolUseId":"toolu_1"}"#,
            transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"a1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
            workflow: None,
            journal: false,
        };
        let DecodedSession { items, .. } = replay_from_session("", &[sub], "fixture");
        assert!(items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AgentDiscovered(agent) if agent.id.0 == "a1000000000000001"
        )));
        assert!(items.iter().any(|item| {
            item.event.actor.0 == "a1000000000000001"
                && matches!(item.event.kind, EventKind::Activity)
        }));
    }

    #[test]
    fn replay_from_session_tags_workflow_subagents_and_journals() {
        let subs = [
            ClaudeSessionFile {
                agent_id: "w1000000000000001",
                meta: r#"{"agentType":"workflow-subagent","description":"review:bugs"}"#,
                transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"w1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
                workflow: Some("wf-99"),
                journal: false,
            },
            ClaudeSessionFile {
                agent_id: "",
                meta: "",
                transcript: r#"{"type":"started","key":"review","agentId":"w1000000000000001"}"#,
                workflow: Some("wf-99"),
                journal: true,
            },
        ];
        let DecodedSession { items, .. } = replay_from_session("", &subs, "fixture");
        assert!(items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AgentDiscovered(agent)
                if agent.id.0 == "w1000000000000001" && agent.parent.0 == "wf-99"
        )));
        assert!(items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AgentStatus { agent_id, status: crate::event::RecordedAgentStatus::Running }
                if agent_id.0 == "w1000000000000001"
        )));
        assert!(!items.iter().any(|item| item.event.actor.0.is_empty()));
    }

    #[test]
    fn content_detection_replays_codex_without_a_filename_hint() {
        let text = include_str!("../../tests/fixtures/codex/root-current.jsonl");
        let DecodedSession { session, items, .. } = replay_from_jsonl(text, "ignored");
        assert_eq!(session, SessionKey::new(Provider::Codex, "root-thread"));
        assert!(items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::SessionMetadata(metadata) if metadata.session.provider == Provider::Codex
        )));
        assert!(
            items
                .iter()
                .any(|item| matches!(item.event.kind, EventKind::Prompt { .. }))
        );
    }

    fn folded_model(decoded: &DecodedSession) -> crate::state::session::SessionModel {
        let mut model = crate::state::session::SessionModel::new(decoded.session.clone());
        for item in &decoded.items {
            model.apply_event(&item.event);
        }
        model
    }

    #[test]
    fn portable_codex_root_preserves_identity_accounting_and_child_placeholder() {
        let decoded = replay_from_jsonl(
            include_str!("../../tests/fixtures/codex/root-current.jsonl"),
            "ignored",
        );
        assert_eq!(
            decoded.session,
            SessionKey::new(Provider::Codex, "root-thread")
        );
        let model = folded_model(&decoded);
        let main = model.agent(crate::state::session::MAIN_ID).unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-test"));
        assert_eq!(main.output_tokens, 25);
        assert_eq!(main.tool_calls.len(), 3);
        assert_eq!(
            main.tool_calls
                .iter()
                .filter(|tool| tool.state == crate::state::session::ToolState::CompletedUnknown)
                .count(),
            1
        );
        assert!(model.agent("child-thread").is_some());
        assert!(
            model.agent("root-thread").is_none(),
            "root actor maps to main"
        );
    }

    #[test]
    fn portable_codex_child_is_logical_main_and_excludes_copied_ancestors() {
        let decoded = replay_from_jsonl(
            include_str!("../../tests/fixtures/codex/child-with-prefix.jsonl"),
            "ignored",
        );
        assert_eq!(
            decoded.session,
            SessionKey::new(Provider::Codex, "child-thread")
        );
        let model = folded_model(&decoded);
        let main = model.agent(crate::state::session::MAIN_ID).unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-child"));
        assert_eq!(main.output_tokens, 7);
        assert!(model.agent("grandchild-thread").is_some());
        assert!(model.agent("copied-grandchild").is_none());
        assert!(model.agent("child-thread").is_none());
        assert_eq!(model.agent_count(), 2);
    }

    #[test]
    fn content_detection_skips_noise_until_a_positive_header() {
        let text = format!(
            "not json\n{{\"type\":\"future_record\",\"payload\":{{}}}}\n{}",
            include_str!("../../tests/fixtures/codex/root-current.jsonl")
        );
        assert_eq!(
            replay_from_jsonl(&text, "ignored").session,
            SessionKey::new(Provider::Codex, "root-thread")
        );
    }

    #[test]
    fn content_detection_ignores_a_wrong_shaped_claude_type_before_codex() {
        let text = format!(
            "{{\"type\":\"user\",\"sessionId\":\"bogus\",\"payload\":{{}}}}\n{}",
            include_str!("../../tests/fixtures/codex/root-current.jsonl")
        );
        assert_eq!(
            replay_from_jsonl(&text, "ignored").session,
            SessionKey::new(Provider::Codex, "root-thread")
        );
    }

    #[test]
    fn claude_identity_ignores_session_ids_on_unrecognized_noise() {
        let text = concat!(
            r#"{"type":"future_record","sessionId":"bogus"}"#,
            "\n",
            r#"{"type":"user","sessionId":"actual","message":{"role":"user","content":"hello"}}"#,
        );
        assert_eq!(
            replay_from_jsonl(text, "fallback").session,
            SessionKey::new(Provider::Claude, "actual")
        );
    }

    #[test]
    fn whitespace_only_codex_identity_is_noise_before_valid_claude() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"  "}}"#,
            "\n",
            r#"{"type":"user","sessionId":"claude-real","message":{"role":"user","content":"hello"}}"#,
        );
        assert_eq!(
            replay_from_jsonl(text, "fallback").session,
            SessionKey::new(Provider::Claude, "claude-real")
        );
    }

    #[test]
    fn whitespace_only_claude_identity_uses_the_selected_filename() {
        let text =
            r#"{"type":"user","sessionId":"  ","message":{"role":"user","content":"hello"}}"#;
        assert_eq!(
            replay_from_jsonl(text, "selected-file").session,
            SessionKey::new(Provider::Claude, "selected-file")
        );
    }

    #[test]
    fn portable_feed_preserves_every_event_from_one_appended_line() {
        let mut decoded = replay_from_jsonl("", "fixture");
        let line = r#"{"type":"assistant","uuid":"a","timestamp":"2026-06-05T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"working"},{"type":"tool_use","id":"t1","name":"Read","input":{}},{"type":"tool_use","id":"t2","name":"Bash","input":{}}]}}"#;
        let events = decoded.feed.append_claude(line, &[]).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ToolStarted(_)))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "working"
        )));
    }

    #[test]
    fn portable_feed_delivers_an_unresolved_journal_status_without_withholding_it() {
        let mut decoded = replay_from_jsonl("", "fixture");
        let journal = [ClaudeSessionFile {
            agent_id: "",
            meta: "",
            transcript: r#"{"type":"result","key":"k","agentId":"late-child","result":"done"}"#,
            workflow: Some("wf"),
            journal: true,
        }];
        let events = decoded.feed.append_claude("", &journal).unwrap();
        assert!(events.iter().any(|event| matches!(
            (&event.time, &event.kind),
            (
                EventTime::AtAgentEnd(actor),
                EventKind::AgentStatus { agent_id, status: crate::event::RecordedAgentStatus::Completed }
            ) if actor.0 == "late-child" && agent_id.0 == "late-child"
        )));
    }

    #[test]
    fn claude_snapshot_plus_append_matches_one_shot_across_all_file_roles() {
        let main_initial = concat!(
            r#"{"type":"user","uuid":"p","origin":{"kind":"human"},"timestamp":"2026-06-05T10:00:00Z","message":{"role":"user","content":"review"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a","timestamp":"2026-06-05T10:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"I will delegate."}]}}"#,
        );
        let main_tail = r#"{"type":"assistant","uuid":"b","timestamp":"2026-06-05T10:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"ag1","name":"Agent","input":{"description":"review"}}]}}"#;
        let sub_initial = r#"{"type":"assistant","uuid":"s1","timestamp":"2026-06-05T10:00:03Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{}}]}}"#;
        let sub_tail = r#"{"type":"user","uuid":"s2","timestamp":"2026-06-05T10:00:04Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b1"}]}}"#;
        let meta = r#"{"agentType":"guide","description":"review","toolUseId":"ag1"}"#;
        let journal_initial = r#"{"type":"started","key":"k","agentId":"sub1"}"#;
        let journal_tail = r#"{"type":"result","key":"k","agentId":"sub1","result":"done"}"#;
        let initial_subs = [
            ClaudeSessionFile {
                agent_id: "sub1",
                meta,
                transcript: sub_initial,
                workflow: Some("wf"),
                journal: false,
            },
            ClaudeSessionFile {
                agent_id: "",
                meta: "",
                transcript: journal_initial,
                workflow: Some("wf"),
                journal: true,
            },
        ];
        let mut incremental = replay_from_session(main_initial, &initial_subs, "fixture");
        let tail_subs = [
            ClaudeSessionFile {
                agent_id: "sub1",
                meta: "",
                transcript: sub_tail,
                workflow: Some("wf"),
                journal: false,
            },
            ClaudeSessionFile {
                agent_id: "",
                meta: "",
                transcript: journal_tail,
                workflow: Some("wf"),
                journal: true,
            },
        ];
        let appended = incremental
            .feed
            .append_claude(main_tail, &tail_subs)
            .unwrap();
        let mut incremental_model = folded_model(&incremental);
        for event in appended {
            incremental_model.apply_event(&event);
        }

        let complete_main = format!("{main_initial}\n{main_tail}");
        let complete_sub = format!("{sub_initial}\n{sub_tail}");
        let complete_journal = format!("{journal_initial}\n{journal_tail}");
        let complete_subs = [
            ClaudeSessionFile {
                agent_id: "sub1",
                meta,
                transcript: &complete_sub,
                workflow: Some("wf"),
                journal: false,
            },
            ClaudeSessionFile {
                agent_id: "",
                meta: "",
                transcript: &complete_journal,
                workflow: Some("wf"),
                journal: true,
            },
        ];
        let one_shot = replay_from_session(&complete_main, &complete_subs, "fixture");
        let one_shot_model = folded_model(&one_shot);

        let summary = |model: &crate::state::session::SessionModel| {
            let main = model.agent(crate::state::session::MAIN_ID).unwrap();
            let sub = model.agent("sub1").unwrap();
            (
                main.tool_calls.len(),
                model
                    .provenance(sub)
                    .and_then(|context| context.reasoning.clone()),
                sub.parent.clone(),
                sub.tool_calls
                    .iter()
                    .map(|tool| tool.state)
                    .collect::<Vec<_>>(),
                sub.status,
            )
        };
        assert_eq!(summary(&incremental_model), summary(&one_shot_model));
    }

    #[test]
    fn equal_time_discovery_precedes_child_activity() {
        let timestamp = "2026-06-05T10:00:00Z".parse().unwrap();
        let child = ActorId::from("child");
        let mut items = vec![
            ReplayItem::new(
                SessionEvent {
                    actor: child.clone(),
                    time: EventTime::At(timestamp),
                    kind: EventKind::Reasoning {
                        fact_id: "child-reasoning".into(),
                        text: "work".into(),
                    },
                },
                None,
            ),
            ReplayItem::new(
                SessionEvent {
                    actor: ActorId::from("root"),
                    time: EventTime::At(timestamp),
                    kind: EventKind::AgentDiscovered(crate::event::AgentDescriptor {
                        id: child,
                        parent: ActorId::from("root"),
                        spawn: crate::event::SpawnProvenance {
                            tool_call_id: None,
                            time: EventTime::At(timestamp),
                            preceding_context: None,
                            task_description: None,
                        },
                        spawn_reference: None,
                        completion_policy: crate::event::AgentCompletionPolicy::InferFromSilence,
                        role: crate::event::AgentRole::Subagent,
                        label: None,
                        agent_type: None,
                        description: None,
                        interactive: false,
                    }),
                },
                None,
            ),
        ];
        date_and_sort(&mut items);
        assert!(matches!(items[0].event.kind, EventKind::AgentDiscovered(_)));
    }
}
