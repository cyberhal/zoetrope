//! Stateful adapter from Claude Code's private JSONL schema to session facts.
//!
//! The wire DTOs remain in the crate-private transcript module. This adapter is
//! their only semantic consumer: everything downstream sees [`SessionEvent`]
//! values.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::event::{
    ActorId, AgentCompletionPolicy, AgentDescriptor, AgentMetadataPatch, AgentRole,
    AssistantChannel, EventKind, EventTime, Provider, RecordedAgentStatus, SessionEvent,
    SessionInfoPatch, SessionKey, SessionMetadata, SessionOrigin, SpawnProvenance, ToolCategory,
    ToolFinish, ToolOutcome, ToolStart, UsageObservation, WorkflowDescriptor,
};
use crate::transcript::{
    self, AgentToolInput, ContentBlock, Entry, FlatValueEntry, UserContent, UserContentBlock,
};

/// Meaning of one Claude-owned file inside a session manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClaudeFile {
    Root,
    Subagent {
        agent_id: String,
        workflow: Option<String>,
    },
    WorkflowJournal {
        workflow: String,
    },
}

/// One decoder per Claude JSONL file.
#[derive(Debug)]
pub struct ClaudeDecoder {
    session: SessionKey,
    source: ClaudeFile,
    metadata_emitted: bool,
    sequence: u64,
    seen_prompts: HashSet<(Option<DateTime<Utc>>, String)>,
    seen_tools: HashSet<String>,
    seen_results: HashSet<String>,
    seen_usage: HashMap<String, u64>,
    latest_context: Option<String>,
}

impl ClaudeDecoder {
    pub fn new(session: SessionKey, source: ClaudeFile) -> Self {
        debug_assert_eq!(session.provider, Provider::Claude);
        Self {
            session,
            source,
            metadata_emitted: false,
            sequence: 0,
            seen_prompts: HashSet::new(),
            seen_tools: HashSet::new(),
            seen_results: HashSet::new(),
            seen_usage: HashMap::new(),
            latest_context: None,
        }
    }

    pub fn decode_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let Some(entry) = transcript::parse_line(line) else {
            return Vec::new();
        };
        self.decode_entry(entry)
    }

    #[cfg(test)]
    pub(crate) fn decode_test_entry(&mut self, entry: Entry) -> Vec<SessionEvent> {
        self.decode_entry(entry)
    }

    fn decode_entry(&mut self, entry: Entry) -> Vec<SessionEvent> {
        self.sequence = self.sequence.saturating_add(1);
        let mut events = Vec::new();
        let activity_time = entry_time(&entry);
        let is_activity = matches!(&entry, Entry::Assistant(_) | Entry::User(_));
        if matches!(self.source, ClaudeFile::Root) && !self.metadata_emitted {
            self.metadata_emitted = true;
            let cwd = entry_cwd(&entry).map(str::to_owned);
            events.push(self.event(
                entry_time(&entry),
                EventKind::SessionMetadata(SessionMetadata {
                    session: self.session.clone(),
                    cwd: cwd.clone(),
                    producer_version: None,
                    origin: SessionOrigin::TopLevel,
                }),
            ));
            if cwd.is_some() {
                events.push(self.event(
                    None,
                    EventKind::SessionInfo(SessionInfoPatch {
                        cwd,
                        ..SessionInfoPatch::default()
                    }),
                ));
            }
        }

        let semantic_start = events.len();
        match entry {
            Entry::Assistant(entry) => self.decode_assistant(*entry, &mut events),
            Entry::User(entry) => self.decode_user(*entry, &mut events),
            Entry::Started(entry) if matches!(self.source, ClaudeFile::WorkflowJournal { .. }) => {
                if let Some(agent_id) = entry.agent_id.filter(|id| !id.is_empty()) {
                    events.push(SessionEvent {
                        actor: self.actor(),
                        time: EventTime::AtAgentStart(ActorId(agent_id.clone())),
                        kind: EventKind::AgentStatus {
                            agent_id: ActorId(agent_id),
                            status: RecordedAgentStatus::Running,
                        },
                    });
                }
            }
            Entry::Result(entry) if matches!(self.source, ClaudeFile::WorkflowJournal { .. }) => {
                if let Some(agent_id) = entry.agent_id.filter(|id| !id.is_empty()) {
                    events.push(SessionEvent {
                        actor: self.actor(),
                        time: EventTime::AtAgentEnd(ActorId(agent_id.clone())),
                        kind: EventKind::AgentStatus {
                            agent_id: ActorId(agent_id),
                            status: RecordedAgentStatus::Completed,
                        },
                    });
                }
            }
            Entry::AiTitle(entry) => self.info(
                SessionInfoPatch {
                    title: entry.title,
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            Entry::Mode(entry) => self.info(
                SessionInfoPatch {
                    mode: string_field(&entry, "mode"),
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            Entry::PermissionMode(entry) => self.info(
                SessionInfoPatch {
                    permission_mode: string_field(&entry, "permissionMode"),
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            Entry::LastPrompt(entry) => self.info(
                SessionInfoPatch {
                    last_prompt: string_field(&entry, "lastPrompt"),
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            Entry::QueueOperation(entry) => self.info(
                SessionInfoPatch {
                    queued_ops_delta: u32::from(
                        string_field(&entry, "operation").as_deref() == Some("enqueue"),
                    ),
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            Entry::FileHistorySnapshot(_) => self.info(
                SessionInfoPatch {
                    file_snapshots_delta: 1,
                    ..SessionInfoPatch::default()
                },
                &mut events,
            ),
            _ => {}
        }
        if is_activity
            && !events[semantic_start..].iter().any(|event| {
                matches!(
                    event.kind,
                    EventKind::Activity
                        | EventKind::Prompt { .. }
                        | EventKind::AssistantText { .. }
                        | EventKind::Reasoning { .. }
                        | EventKind::ModelSelected { .. }
                        | EventKind::UsageObserved(_)
                        | EventKind::ToolStarted(_)
                        | EventKind::ToolFinished(_)
                )
            })
        {
            events.push(self.event(activity_time, EventKind::Activity));
        }
        events
    }

    fn decode_assistant(
        &mut self,
        entry: transcript::AssistantEntry,
        events: &mut Vec<SessionEvent>,
    ) {
        let timestamp = entry.envelope.timestamp;
        let Some(message) = entry.message else { return };
        if let Some(model) = message.model.filter(|model| !model.trim().is_empty()) {
            events.push(self.event(timestamp, EventKind::ModelSelected { model }));
        }
        if let Some(usage) = message.usage
            && let Some(output_tokens) = usage.output_tokens
        {
            let scope = entry
                .envelope
                .request_id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("line:{}", self.sequence));
            let revision = self.seen_usage.entry(scope.clone()).or_default();
            *revision = revision.saturating_add(1);
            let revision = *revision;
            events.push(self.event(
                timestamp,
                EventKind::UsageObserved(UsageObservation {
                    scope,
                    revision,
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cache_read_input_tokens,
                    cache_write_input_tokens: usage.cache_creation_input_tokens,
                    output_tokens: Some(output_tokens),
                    reasoning_output_tokens: None,
                }),
            ));
        }

        let mut nearest = self.latest_context.clone();
        for block in message.content {
            match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    nearest = Some(excerpt(&text));
                    self.latest_context = nearest.clone();
                    events.push(self.event(
                        timestamp,
                        EventKind::AssistantText {
                            channel: AssistantChannel::Other("assistant".to_owned()),
                            text,
                        },
                    ));
                }
                ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                    nearest = Some(excerpt(&thinking));
                    self.latest_context = nearest.clone();
                    events.push(self.event(timestamp, EventKind::Reasoning { text: thinking }));
                }
                ContentBlock::ToolUse(tool) => {
                    let (Some(id), Some(name)) = (
                        tool.id.filter(|id| !id.is_empty()),
                        tool.name.filter(|name| !name.is_empty()),
                    ) else {
                        continue;
                    };
                    if !self.seen_tools.insert(id.clone()) {
                        continue;
                    }
                    let category = match name.as_str() {
                        "Workflow" => ToolCategory::WorkflowSpawn,
                        "Agent" | "Task" => ToolCategory::AgentSpawn,
                        _ => ToolCategory::Ordinary,
                    };
                    let summary = summarize_tool(&name, &tool.input, entry.envelope.cwd.as_deref());
                    let spawn = (category != ToolCategory::Ordinary).then(|| SpawnProvenance {
                        tool_call_id: Some(id.clone()),
                        time: timestamp.map_or(EventTime::Untimed, EventTime::At),
                        preceding_context: nearest.clone(),
                    });
                    events.push(self.event(
                        timestamp,
                        EventKind::ToolStarted(ToolStart {
                            id,
                            name,
                            category,
                            summary,
                            spawn,
                        }),
                    ));
                    if category != ToolCategory::Ordinary {
                        self.latest_context = nearest.clone();
                    }
                }
                _ => {}
            }
        }
    }

    fn decode_user(&mut self, entry: transcript::UserEntry, events: &mut Vec<SessionEvent>) {
        let timestamp = entry.envelope.timestamp;
        if matches!(self.source, ClaudeFile::Root) {
            if let Some(workflow) = entry.workflow_launch() {
                events.push(self.event(
                    timestamp,
                    EventKind::WorkflowDeclared(WorkflowDescriptor {
                        id: ActorId(workflow.run_id),
                        parent: self.actor(),
                        name: workflow.name,
                        description: workflow.summary,
                    }),
                ));
            }
            if let Some(text) = entry.prompt_text() {
                if let Some(notification) = transcript::parse_task_notification(text) {
                    let status = match notification.status {
                        transcript::TaskStatus::Completed => Some(RecordedAgentStatus::Completed),
                        transcript::TaskStatus::Stopped => Some(RecordedAgentStatus::Interrupted),
                        transcript::TaskStatus::Failed => Some(RecordedAgentStatus::Failed),
                        transcript::TaskStatus::Other => None,
                    };
                    if let Some(status) = status {
                        events.push(self.event(
                            timestamp,
                            EventKind::AgentStatus {
                                agent_id: ActorId(notification.agent_id),
                                status,
                            },
                        ));
                    }
                } else if entry.is_human_prompt()
                    && self.seen_prompts.insert((timestamp, text.to_owned()))
                {
                    events.push(self.event(
                        timestamp,
                        EventKind::Prompt {
                            text: text.to_owned(),
                        },
                    ));
                }
            }
        }
        if let Some(message) = entry.message
            && let Some(UserContent::Blocks(blocks)) = message.content
        {
            for block in blocks {
                if let UserContentBlock::ToolResult(result) = block
                    && let Some(id) = result.tool_use_id.filter(|id| !id.is_empty())
                    && self.seen_results.insert(id.clone())
                {
                    events.push(self.event(
                        timestamp,
                        EventKind::ToolFinished(ToolFinish {
                            id,
                            outcome: if result.is_error == Some(true) {
                                ToolOutcome::Failed
                            } else {
                                ToolOutcome::Succeeded
                            },
                            completes_spawn: true,
                            spawn_reference: None,
                        }),
                    ));
                }
            }
        }
    }

    fn info(&self, patch: SessionInfoPatch, events: &mut Vec<SessionEvent>) {
        events.push(self.event(None, EventKind::SessionInfo(patch)));
    }

    fn actor(&self) -> ActorId {
        match &self.source {
            ClaudeFile::Root => ActorId(self.session.id.clone()),
            ClaudeFile::Subagent { agent_id, .. } => ActorId(agent_id.clone()),
            ClaudeFile::WorkflowJournal { workflow } => ActorId(workflow.clone()),
        }
    }

    fn event(&self, timestamp: Option<DateTime<Utc>>, kind: EventKind) -> SessionEvent {
        SessionEvent {
            actor: self.actor(),
            time: timestamp.map_or(EventTime::Untimed, EventTime::At),
            kind,
        }
    }
}

/// Convert a Claude `meta.json` sidecar into structural and metadata facts.
pub fn decode_subagent_metadata(
    session: &SessionKey,
    agent_id: &str,
    workflow: Option<&str>,
    text: &str,
) -> Vec<SessionEvent> {
    let Some(meta) = transcript::parse_meta(text) else {
        return Vec::new();
    };
    let root = ActorId(session.id.clone());
    let parent = workflow.map_or_else(|| root.clone(), |id| ActorId(id.to_owned()));
    let mut events = Vec::new();
    if let Some(workflow) = workflow {
        events.push(SessionEvent {
            actor: root.clone(),
            time: EventTime::AtAgentStart(ActorId(agent_id.to_owned())),
            kind: EventKind::AgentDiscovered(AgentDescriptor {
                id: ActorId(workflow.to_owned()),
                parent: root.clone(),
                spawn: SpawnProvenance {
                    tool_call_id: None,
                    time: EventTime::AtAgentStart(ActorId(agent_id.to_owned())),
                    preceding_context: None,
                },
                spawn_reference: None,
                completion_policy: AgentCompletionPolicy::InferFromSilence,
                role: AgentRole::WorkflowGroup,
                label: None,
                agent_type: None,
                description: None,
                interactive: false,
            }),
        });
    }
    let spawn = SpawnProvenance {
        tool_call_id: meta.tool_use_id.clone(),
        time: EventTime::AtAgentStart(ActorId(agent_id.to_owned())),
        preceding_context: None,
    };
    let interactive = meta.agent_type.as_deref() == Some("fork");
    events.push(SessionEvent {
        actor: root,
        time: EventTime::AtAgentStart(ActorId(agent_id.to_owned())),
        kind: EventKind::AgentDiscovered(AgentDescriptor {
            id: ActorId(agent_id.to_owned()),
            parent,
            spawn: spawn.clone(),
            spawn_reference: None,
            completion_policy: AgentCompletionPolicy::InferFromSilence,
            role: AgentRole::Subagent,
            label: meta.agent_type.clone(),
            agent_type: meta.agent_type.clone(),
            description: meta.description.clone(),
            interactive,
        }),
    });
    events.push(SessionEvent {
        actor: ActorId(session.id.clone()),
        time: EventTime::AtAgentStart(ActorId(agent_id.to_owned())),
        kind: EventKind::AgentMetadata(AgentMetadataPatch {
            id: ActorId(agent_id.to_owned()),
            label: meta.agent_type.clone(),
            agent_type: meta.agent_type,
            description: meta.description,
            interactive: Some(interactive),
            spawn: Some(spawn),
        }),
    });
    events
}

fn entry_time(entry: &Entry) -> Option<DateTime<Utc>> {
    match entry {
        Entry::User(entry) => entry.envelope.timestamp,
        Entry::Assistant(entry) => entry.envelope.timestamp,
        Entry::System(entry) => entry.envelope.timestamp,
        Entry::Attachment(entry) => entry.envelope.timestamp,
        _ => None,
    }
}

fn entry_cwd(entry: &Entry) -> Option<&str> {
    match entry {
        Entry::User(entry) => entry.envelope.cwd.as_deref(),
        Entry::Assistant(entry) => entry.envelope.cwd.as_deref(),
        Entry::System(entry) => entry.envelope.cwd.as_deref(),
        Entry::Attachment(entry) => entry.envelope.cwd.as_deref(),
        _ => None,
    }
}

fn string_field(entry: &FlatValueEntry, key: &str) -> Option<String> {
    entry
        .fields
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 240 {
        format!("{}…", flat.chars().take(239).collect::<String>())
    } else {
        flat
    }
}

fn summarize_tool(name: &str, input: &serde_json::Value, cwd: Option<&str>) -> Option<String> {
    let pick = |key: &str| {
        input
            .get(key)
            .and_then(|value| value.as_str())
            .map(truncate_summary)
    };
    let pick_path = |key: &str| {
        input
            .get(key)
            .and_then(|value| value.as_str())
            .map(|path| short_path(path, cwd))
    };
    match name {
        "Bash" => pick("command").or_else(|| pick("description")),
        "Read" | "Write" | "Edit" => pick_path("file_path").or_else(|| pick_path("path")),
        "Agent" | "Task" | "Workflow" => {
            let typed: AgentToolInput = serde_json::from_value(input.clone()).unwrap_or_default();
            typed
                .description
                .or(typed.subagent_type)
                .map(|text| truncate_summary(&text))
        }
        "WebFetch" => pick("url"),
        "ToolSearch" => pick("query"),
        _ => pick("description").or_else(|| pick("query")),
    }
}

fn truncate_summary(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 200 {
        format!("{}…", flat.chars().take(199).collect::<String>())
    } else {
        flat
    }
}

fn short_path(path: &str, cwd: Option<&str>) -> String {
    let relative = cwd
        .and_then(|cwd| path.strip_prefix(cwd).map(|rest| (cwd, rest)))
        .filter(|(cwd, rest)| rest.starts_with('/') || cwd.ends_with('/'))
        .map(|(_, rest)| rest.trim_start_matches('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path);
    let count = relative.chars().count();
    if count <= 200 {
        relative.to_owned()
    } else {
        format!(
            "…{}",
            relative.chars().skip(count - 199).collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characterization_fixture_normalizes_without_provider_wire_types() {
        let mut decoder = ClaudeDecoder::new(
            SessionKey {
                provider: Provider::Claude,
                id: "characterization".into(),
            },
            ClaudeFile::Root,
        );
        let events: Vec<_> = include_str!("../../tests/fixtures/claude/characterization.jsonl")
            .lines()
            .flat_map(|line| decoder.decode_line(line))
            .collect();
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Prompt { text } if text == "Map the dependency graph."
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ModelSelected { model } if model == "claude-test"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ToolStarted(_)))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    EventKind::ToolStarted(tool)
                        if matches!(
                            tool.category,
                            ToolCategory::AgentSpawn | ToolCategory::WorkflowSpawn
                        )
                ))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ToolFinished(ToolFinish {
                outcome: ToolOutcome::Failed,
                ..
            })
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::UsageObserved(usage) if usage.output_tokens == Some(12)
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::SessionInfo(info) if info.title.as_deref() == Some("Dependency map")
        )));
    }

    fn decoded_tool_summary(
        name: &str,
        input: serde_json::Value,
        cwd: Option<&str>,
    ) -> Option<String> {
        let mut decoder = ClaudeDecoder::new(
            SessionKey::new(Provider::Claude, "summary-test"),
            ClaudeFile::Root,
        );
        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "tool-line",
            "cwd": cwd,
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tool-1",
                    "name": name,
                    "input": input,
                }],
            },
        })
        .to_string();
        decoder.decode_line(&line).into_iter().find_map(|event| {
            if let EventKind::ToolStarted(tool) = event.kind {
                tool.summary
            } else {
                None
            }
        })
    }

    #[test]
    fn tool_summaries_relativize_paths_and_leave_other_tools_readable() {
        assert_eq!(
            decoded_tool_summary(
                "Edit",
                serde_json::json!({ "file_path": "/proj/src/main.rs" }),
                Some("/proj")
            )
            .as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            decoded_tool_summary(
                "Read",
                serde_json::json!({ "file_path": "/other/x.rs" }),
                Some("/proj")
            )
            .as_deref(),
            Some("/other/x.rs")
        );
        assert_eq!(
            decoded_tool_summary(
                "Bash",
                serde_json::json!({ "command": "cargo test" }),
                Some("/proj")
            )
            .as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn tool_paths_strip_cwd_only_at_a_component_boundary() {
        let summary = |path: &str, cwd: &str| {
            decoded_tool_summary("Read", serde_json::json!({ "file_path": path }), Some(cwd))
        };
        assert_eq!(
            summary(
                "/Users/me/projects/zoetrope/src/a.rs",
                "/Users/me/projects/zoetrope"
            )
            .as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            summary(
                "/Users/me/projects/zoetrope-web/src/a.rs",
                "/Users/me/projects/zoetrope"
            )
            .as_deref(),
            Some("/Users/me/projects/zoetrope-web/src/a.rs")
        );
        assert_eq!(
            summary("/project/x.rs", "/proj").as_deref(),
            Some("/project/x.rs")
        );
        assert_eq!(summary("/proj/x.rs", "/proj/").as_deref(), Some("x.rs"));
        assert_eq!(summary("/proj", "/proj").as_deref(), Some("/proj"));
    }
}
