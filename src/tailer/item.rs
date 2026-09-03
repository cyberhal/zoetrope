//! Portable provider-neutral replay items and ordering.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::event::{ActorId, EventKind, EventTime, Provider, SessionEvent, SessionKey};
use crate::formats::claude::{ClaudeDecoder, ClaudeFile, decode_subagent_metadata};
use crate::formats::codex::CodexDecoder;

#[cfg(test)]
type OriginalUpdate = crate::tailer::Update;
#[cfg(not(test))]
type OriginalUpdate = ();

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
    #[cfg(test)]
    pub(crate) update: crate::tailer::Update,
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
        Self {
            timing,
            #[cfg(test)]
            update: crate::tailer::Update::Event(event.clone()),
            event,
        }
    }

    pub(crate) fn live(event: impl IntoSessionEvent) -> Self {
        let (event, update) = event.into_parts();
        with_original_update(Self::new(event, None), update)
    }

    #[cfg(test)]
    pub(crate) fn at(timestamp: Option<DateTime<Utc>>, update: impl IntoSessionEvent) -> Self {
        let (mut event, original) = update.into_parts();
        if let Some(timestamp) = timestamp {
            event.time = EventTime::At(timestamp);
        }
        let mut item = Self::new(event, None);
        if let Some(update) = original {
            item.update = update;
        }
        item
    }
}

#[cfg(test)]
fn with_original_update(mut item: ReplayItem, update: Option<OriginalUpdate>) -> ReplayItem {
    if let Some(update) = update {
        item.update = update;
    }
    item
}

#[cfg(not(test))]
fn with_original_update(item: ReplayItem, update: Option<OriginalUpdate>) -> ReplayItem {
    debug_assert!(update.is_none());
    item
}

#[cfg(test)]
pub(crate) fn test_event(update: &crate::tailer::Update) -> SessionEvent {
    use crate::formats::claude::{ClaudeDecoder, ClaudeFile, decode_subagent_metadata};
    use crate::tailer::{Source, Update};
    let key = SessionKey {
        provider: Provider::Claude,
        id: "s".to_owned(),
    };
    let events = match update {
        Update::Event(event) => return event.clone(),
        Update::Entry { source, entry } => {
            let file = match source {
                Source::Main => ClaudeFile::Root,
                Source::Sub(agent_id) => ClaudeFile::Subagent {
                    agent_id: agent_id.clone(),
                    workflow: None,
                },
                Source::Journal(workflow) => ClaudeFile::WorkflowJournal {
                    workflow: workflow.clone(),
                },
            };
            let mut decoder = ClaudeDecoder::new(key, file);
            decoder.decode_test_entry(entry.clone())
        }
        Update::SubagentMeta {
            agent_id,
            workflow,
            meta,
        } => decode_subagent_metadata(
            &key,
            agent_id,
            workflow.as_deref(),
            &serde_json::json!({
                "agentType": meta.agent_type,
                "description": meta.description,
                "toolUseId": meta.tool_use_id,
                "stoppedByUser": meta.stopped_by_user,
            })
            .to_string(),
        ),
    };
    events
        .into_iter()
        .find(|event| {
            !matches!(
                event.kind,
                EventKind::SessionMetadata(_) | EventKind::SessionInfo(_)
            )
        })
        .unwrap_or(SessionEvent {
            actor: ActorId::from("s"),
            time: EventTime::Untimed,
            kind: EventKind::Reasoning {
                text: String::new(),
            },
        })
}

pub(crate) trait IntoSessionEvent {
    fn into_parts(self) -> (SessionEvent, Option<OriginalUpdate>);
}

impl IntoSessionEvent for SessionEvent {
    fn into_parts(self) -> (SessionEvent, Option<OriginalUpdate>) {
        (self, None)
    }
}

#[cfg(test)]
impl IntoSessionEvent for crate::tailer::Update {
    fn into_parts(self) -> (SessionEvent, Option<OriginalUpdate>) {
        (test_event(&self), Some(self))
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

/// Browser/static single-file replay with content-based format detection.
pub fn replay_from_jsonl(text: &str) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let provider = detect_provider(text);
    let key = SessionKey {
        provider,
        id: "session".to_owned(),
    };
    let events = match provider {
        Provider::Claude => {
            let mut decoder = ClaudeDecoder::new(key, ClaudeFile::Root);
            text.lines()
                .flat_map(|line| decoder.decode_line(line))
                .collect()
        }
        Provider::Codex => {
            let mut decoder = CodexDecoder::new();
            text.lines()
                .flat_map(|line| decoder.decode_line(line))
                .collect()
        }
    };
    finish(events)
}

fn detect_provider(text: &str) -> Provider {
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|kind| kind.as_str()) == Some("session_meta")
            && value.get("payload").is_some()
        {
            return Provider::Codex;
        }
        return Provider::Claude;
    }
    Provider::Claude
}

pub struct DemoSubagent<'a> {
    pub agent_id: &'a str,
    pub meta: &'a str,
    pub transcript: &'a str,
    pub workflow: Option<&'a str>,
    pub journal: bool,
}

pub fn replay_from_session(
    main: &str,
    subagents: &[DemoSubagent<'_>],
) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let key = SessionKey {
        provider: Provider::Claude,
        id: "session".to_owned(),
    };
    let mut events = decode_claude_text(main, &key, ClaudeFile::Root);
    for subagent in subagents {
        if subagent.journal {
            let Some(workflow) = subagent.workflow else {
                continue;
            };
            events.extend(decode_claude_text(
                subagent.transcript,
                &key,
                ClaudeFile::WorkflowJournal {
                    workflow: workflow.to_owned(),
                },
            ));
            continue;
        }
        events.extend(decode_subagent_metadata(
            &key,
            subagent.agent_id,
            subagent.workflow,
            subagent.meta,
        ));
        events.extend(decode_claude_text(
            subagent.transcript,
            &key,
            ClaudeFile::Subagent {
                agent_id: subagent.agent_id.to_owned(),
                workflow: subagent.workflow.map(str::to_owned),
            },
        ));
    }
    finish(events)
}

fn decode_claude_text(text: &str, key: &SessionKey, source: ClaudeFile) -> Vec<SessionEvent> {
    let mut decoder = ClaudeDecoder::new(key.clone(), source);
    text.lines()
        .flat_map(|line| decoder.decode_line(line))
        .collect()
}

pub(crate) fn finish(events: Vec<SessionEvent>) -> (Vec<ReplayItem>, crate::state::SessionInfo) {
    let mut info = crate::state::SessionInfo::default();
    let mut items = Vec::new();
    let mut inherited: HashMap<ActorId, DateTime<Utc>> = HashMap::new();
    for event in events {
        if let EventKind::SessionInfo(patch) = &event.kind {
            info.apply(patch);
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
            DemoSubagent {
                agent_id: "subX",
                meta: "{}",
                transcript: &sub_text,
                workflow: Some("wf"),
                journal: false,
            },
            DemoSubagent {
                agent_id: "",
                meta: "",
                transcript: journal,
                workflow: Some("wf"),
                journal: true,
            },
        ];
        let (items, _) = replay_from_session("", &subs);
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
        let (items, _) = replay_from_jsonl(text);
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
        assert!(activity.iter().all(|item| item.event.actor.0 == "session"));
    }

    #[test]
    fn replay_from_session_emits_subagent_meta_and_sub_entries() {
        let sub = DemoSubagent {
            agent_id: "a1000000000000001",
            meta: r#"{"agentType":"Explore","description":"map it","toolUseId":"toolu_1"}"#,
            transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"a1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
            workflow: None,
            journal: false,
        };
        let (items, _) = replay_from_session("", &[sub]);
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
            DemoSubagent {
                agent_id: "w1000000000000001",
                meta: r#"{"agentType":"workflow-subagent","description":"review:bugs"}"#,
                transcript: r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"w1000000000000001","timestamp":"2026-06-05T10:00:05.000Z","message":{"role":"user","content":"task"}}"#,
                workflow: Some("wf-99"),
                journal: false,
            },
            DemoSubagent {
                agent_id: "",
                meta: "",
                transcript: r#"{"type":"started","key":"review","agentId":"w1000000000000001"}"#,
                workflow: Some("wf-99"),
                journal: true,
            },
        ];
        let (items, _) = replay_from_session("", &subs);
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
        let (items, _) = replay_from_jsonl(text);
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
                        },
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
