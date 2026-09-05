//! Stateful decoder for Codex rollout JSONL.
//!
//! Codex stores overlapping response and event views. This decoder chooses one
//! canonical source for each fact before anything reaches the domain. Spawned
//! thread files may also contain a copied ancestor prefix; that prefix is not
//! owned by the child and stays suppressed until Codex writes its explicit
//! `trigger_turn: true` ownership marker.
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::summary::{codex_tool_summary, task_description};
use crate::event::{
    ActorId, AgentCompletionPolicy, AgentDescriptor, AgentRole, AssistantChannel, EventKind,
    EventTime, Provider, RecordedAgentStatus, SessionEvent, SessionInfoPatch, SessionKey,
    SessionMetadata, SessionOrigin, SpawnProvenance, ToolCategory, ToolFinish, ToolOutcome,
    ToolStart, UsageObservation,
};

/// A non-fatal condition that cannot be represented as session activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderDiagnostic {
    /// A child rollout ended before Codex marked the start of child-owned data.
    MissingOwnedTurnMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Unknown,
    Owned,
    AwaitingOwnedTurn,
}

/// One stateful decoder per rollout file.
#[derive(Debug, Default)]
pub struct CodexDecoder {
    session: Option<SessionMetadata>,
    ownership: Ownership,
    current_turn: Option<String>,
    seen_prompts: HashSet<String>,
    seen_assistant_items: HashSet<String>,
    seen_reasoning_items: HashSet<String>,
    seen_model_turns: HashSet<String>,
    seen_call_starts: HashSet<String>,
    seen_call_finishes: HashSet<String>,
    call_labels: HashMap<String, String>,
    call_spawn_provenance: HashMap<String, SpawnProvenance>,
    latest_context_text: Option<String>,
    seen_agent_activity_ids: HashSet<String>,
    seen_unidentified_activity: HashSet<(String, ActivityKind, Option<i64>)>,
    last_usage: Option<UsageObservation>,
    usage_revision: u64,
    missing_marker_reported: bool,
}

impl Default for Ownership {
    fn default() -> Self {
        Self::Unknown
    }
}

impl CodexDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Metadata from the first valid header in this file.
    pub fn session(&self) -> Option<&SessionMetadata> {
        self.session.as_ref()
    }

    /// Decode one complete JSONL line. Blank, malformed, and unknown records
    /// are ignored; no record shape can panic the decoder.
    pub fn decode_line(&mut self, line: &str) -> Vec<SessionEvent> {
        let Ok(record) = serde_json::from_str::<Record>(line.trim()) else {
            return Vec::new();
        };

        match record {
            Record::SessionMeta { timestamp, payload } => self.decode_header(timestamp, payload),
            Record::InterAgentCommunicationMetadata { payload } => {
                if self.ownership == Ownership::AwaitingOwnedTurn && payload.trigger_turn {
                    self.ownership = Ownership::Owned;
                }
                Vec::new()
            }
            _ if self.ownership != Ownership::Owned => Vec::new(),
            Record::TurnContext { timestamp, payload } => {
                self.decode_turn_context(timestamp, payload)
            }
            Record::ResponseItem { timestamp, payload } => {
                self.decode_response_item(timestamp, payload)
            }
            Record::EventMsg { timestamp, payload } => {
                self.decode_event_message(timestamp, payload)
            }
            Record::Unknown => Vec::new(),
        }
    }

    /// Finish this file's input and return diagnostics that require end-of-file
    /// knowledge. Calling `finish` repeatedly is idempotent.
    pub fn finish(&mut self) -> Vec<DecoderDiagnostic> {
        if self.ownership == Ownership::AwaitingOwnedTurn && !self.missing_marker_reported {
            self.missing_marker_reported = true;
            vec![DecoderDiagnostic::MissingOwnedTurnMarker]
        } else {
            Vec::new()
        }
    }

    fn decode_header(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        payload: SessionMetaPayload,
    ) -> Vec<SessionEvent> {
        // A copied ancestor prefix can contain another session_meta. The first
        // valid header is the file's owner and can never be replaced by data in
        // that prefix.
        if self.session.is_some() || payload.id.trim().is_empty() {
            return Vec::new();
        }

        let origin = payload.source.into_origin();
        self.ownership = if matches!(origin, SessionOrigin::ThreadSpawn { .. }) {
            Ownership::AwaitingOwnedTurn
        } else {
            Ownership::Owned
        };
        let metadata = SessionMetadata {
            session: SessionKey {
                provider: Provider::Codex,
                id: payload.id.clone(),
            },
            cwd: payload.cwd,
            producer_version: payload.cli_version,
            origin,
        };
        self.session = Some(metadata.clone());
        let info = self.info(SessionInfoPatch {
            cwd: metadata.cwd.clone(),
            ..SessionInfoPatch::default()
        });
        let mut events = vec![SessionEvent {
            actor: ActorId(payload.id),
            time: event_time(timestamp),
            kind: EventKind::SessionMetadata(metadata),
        }];
        events.extend(info);
        events
    }

    fn decode_turn_context(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        payload: TurnContextPayload,
    ) -> Vec<SessionEvent> {
        let mut events: Vec<_> = self
            .info(SessionInfoPatch {
                cwd: payload.cwd,
                approval_policy: recorded_label(&payload.approval_policy, "type"),
                sandbox_policy: recorded_label(&payload.sandbox_policy, "type"),
                permission_profile: recorded_label(&payload.active_permission_profile, "id"),
                mode: recorded_label(&payload.collaboration_mode, "mode"),
                effort: recorded_label(&payload.effort, "effort"),
                ..SessionInfoPatch::default()
            })
            .into_iter()
            .collect();
        if let Some(turn_id) = payload.turn_id.filter(|id| !id.trim().is_empty()) {
            self.current_turn = Some(turn_id);
        }
        let Some(model) = payload.model.filter(|model| !model.trim().is_empty()) else {
            return events;
        };
        let key = self
            .current_turn
            .clone()
            .unwrap_or_else(|| format!("untimed:{model}"));
        if !self.seen_model_turns.insert(key) {
            return events;
        }
        events.extend(self.event(timestamp, EventKind::ModelSelected { model }));
        events
    }

    fn decode_response_item(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        payload: ResponseItem,
    ) -> Vec<SessionEvent> {
        match payload {
            ResponseItem::Message {
                id,
                role,
                phase,
                content,
            } if role.as_deref() == Some("assistant") => {
                let text = visible_text(&content, "output_text");
                if text.is_empty() {
                    return Vec::new();
                }
                let Some(fact_id) = mark_item(
                    &mut self.seen_assistant_items,
                    self.current_turn.as_deref(),
                    id,
                    &text,
                ) else {
                    return Vec::new();
                };
                let channel = match phase.as_deref() {
                    Some("commentary") => AssistantChannel::Commentary,
                    Some("final" | "final_answer") => AssistantChannel::Final,
                    Some(other) => AssistantChannel::Other(other.to_owned()),
                    None => AssistantChannel::Other("assistant".to_owned()),
                };
                self.latest_context_text = Some(text.clone());
                self.event(
                    timestamp,
                    EventKind::AssistantText {
                        fact_id,
                        channel,
                        text,
                    },
                )
                .into_iter()
                .collect()
            }
            ResponseItem::Message {
                id, role, content, ..
            } if role.as_deref() == Some("user") => {
                let text = visible_text(&content, "input_text");
                let Some(objective) = goal_objective(&text) else {
                    return Vec::new();
                };
                self.prompt_event(timestamp, id, objective)
            }
            ResponseItem::Reasoning { id, summary, .. } => {
                let text = visible_text(&summary, "summary_text");
                if text.is_empty() {
                    return Vec::new();
                }
                let Some(fact_id) = mark_item(
                    &mut self.seen_reasoning_items,
                    self.current_turn.as_deref(),
                    id,
                    &text,
                ) else {
                    return Vec::new();
                };
                self.latest_context_text = Some(text.clone());
                self.event(timestamp, EventKind::Reasoning { fact_id, text })
                    .into_iter()
                    .collect()
            }
            ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => self.tool_started(timestamp, call_id, name, arguments.as_deref()),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                input,
                ..
            } => self.tool_started(timestamp, call_id, name, input.as_deref()),
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => self.tool_finished(timestamp, call_id, &output),
            _ => Vec::new(),
        }
    }

    fn decode_event_message(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        payload: EventMessage,
    ) -> Vec<SessionEvent> {
        match payload {
            EventMessage::TaskStarted { turn_id } => {
                self.current_turn = turn_id.filter(|id| !id.trim().is_empty());
                self.latest_context_text = None;
                Vec::new()
            }
            EventMessage::UserMessage { message } => {
                let Some(message) = message.filter(|text| !text.trim().is_empty()) else {
                    return Vec::new();
                };
                self.prompt_event(timestamp, None, message)
            }
            EventMessage::TokenCount { info } => {
                let Some(total) = info.and_then(|info| info.total_token_usage) else {
                    return Vec::new();
                };
                let Some(session) = self.session.as_ref() else {
                    return Vec::new();
                };
                let observation = UsageObservation {
                    scope: format!("thread:{}", session.session.id),
                    revision: self.usage_revision.saturating_add(1),
                    input_tokens: total.input_tokens,
                    cached_input_tokens: total.cached_input_tokens,
                    cache_write_input_tokens: total.cache_write_input_tokens,
                    output_tokens: total.output_tokens,
                    reasoning_output_tokens: total.reasoning_output_tokens,
                };
                if self
                    .last_usage
                    .as_ref()
                    .is_some_and(|previous| same_usage_counts(previous, &observation))
                {
                    return Vec::new();
                }
                self.usage_revision = observation.revision;
                self.last_usage = Some(observation.clone());
                self.event(timestamp, EventKind::UsageObserved(observation))
                    .into_iter()
                    .collect()
            }
            EventMessage::SubAgentActivity(activity) => self.agent_activity(timestamp, activity),
            EventMessage::ItemCompleted { item } => match item {
                CompletedItem::SubAgentActivity(activity) => {
                    self.agent_activity(timestamp, activity)
                }
                // These are mirrors of response-item facts, not a second source.
                CompletedItem::Unknown => Vec::new(),
            },
            EventMessage::Unknown => Vec::new(),
        }
    }

    fn prompt_event(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        item_id: Option<String>,
        text: String,
    ) -> Vec<SessionEvent> {
        let text = text.trim().to_owned();
        let key = self.current_turn.clone().unwrap_or_else(|| {
            item_id.unwrap_or_else(|| {
                timestamp
                    .map(|time| time.to_rfc3339())
                    .unwrap_or_else(|| text.clone())
            })
        });
        if !self.seen_prompts.insert(key) {
            return Vec::new();
        }
        let info = self.info(SessionInfoPatch {
            last_prompt: Some(text.clone()),
            ..SessionInfoPatch::default()
        });
        self.event(timestamp, EventKind::Prompt { text })
            .into_iter()
            .chain(info)
            .collect()
    }

    fn info(&self, patch: SessionInfoPatch) -> Option<SessionEvent> {
        if patch == SessionInfoPatch::default() {
            return None;
        }
        self.event(None, EventKind::SessionInfo(patch))
    }

    fn tool_started(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        call_id: Option<String>,
        name: Option<String>,
        input: Option<&str>,
    ) -> Vec<SessionEvent> {
        let (Some(id), Some(name)) = (
            call_id.filter(|id| !id.trim().is_empty()),
            name.filter(|name| !name.is_empty()),
        ) else {
            return Vec::new();
        };
        if !self.seen_call_starts.insert(id.clone()) {
            return Vec::new();
        }
        let cwd = self
            .session
            .as_ref()
            .and_then(|session| session.cwd.as_deref());
        let summary = input.and_then(|input| codex_tool_summary(&name, input, cwd));
        let category = if name == "spawn_agent" {
            ToolCategory::AgentSpawn
        } else {
            ToolCategory::Ordinary
        };
        if category == ToolCategory::AgentSpawn {
            if let Some(label) = summary.clone() {
                self.call_labels.insert(id.clone(), label);
            }
            self.call_spawn_provenance.insert(
                id.clone(),
                SpawnProvenance {
                    tool_call_id: Some(id.clone()),
                    time: event_time(timestamp),
                    preceding_context: self.latest_context_text.clone(),
                    task_description: input
                        .and_then(|input| serde_json::from_str(input).ok())
                        .and_then(|input| task_description(&input)),
                },
            );
        }
        let spawn = (category == ToolCategory::AgentSpawn)
            .then(|| self.call_spawn_provenance.get(&id).cloned())
            .flatten();
        self.event(
            timestamp,
            EventKind::ToolStarted(ToolStart {
                id,
                name,
                category,
                summary,
                spawn,
            }),
        )
        .into_iter()
        .collect()
    }

    fn tool_finished(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        call_id: Option<String>,
        output: &Value,
    ) -> Vec<SessionEvent> {
        let Some(id) = call_id.filter(|id| !id.trim().is_empty()) else {
            return Vec::new();
        };
        if !self.seen_call_finishes.insert(id.clone()) {
            return Vec::new();
        }
        self.event(
            timestamp,
            EventKind::ToolFinished(ToolFinish {
                id,
                outcome: tool_outcome(output),
                completes_spawn: false,
                spawn_reference: spawn_reference(output),
            }),
        )
        .into_iter()
        .collect()
    }

    fn agent_activity(
        &mut self,
        fallback_timestamp: Option<DateTime<Utc>>,
        activity: SubAgentActivity,
    ) -> Vec<SessionEvent> {
        let (Some(agent_id), Some(kind)) = (
            activity.agent_thread_id.filter(|id| !id.trim().is_empty()),
            activity.kind,
        ) else {
            return Vec::new();
        };
        let timestamp = activity
            .occurred_at_ms
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .or(fallback_timestamp);
        let duplicate = match activity
            .event_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
        {
            Some(event_id) => !self.seen_agent_activity_ids.insert(event_id.clone()),
            None => !self.seen_unidentified_activity.insert((
                agent_id.clone(),
                kind,
                timestamp.map(|value| value.timestamp_millis()),
            )),
        };
        if duplicate {
            return Vec::new();
        }
        match kind {
            ActivityKind::Started => {
                let Some(parent) = self.actor() else {
                    return Vec::new();
                };
                let label = activity
                    .event_id
                    .as_ref()
                    .and_then(|id| self.call_labels.get(id))
                    .cloned()
                    .or_else(|| activity.agent_path.as_deref().and_then(path_label));
                let spawn = activity
                    .event_id
                    .as_ref()
                    .and_then(|id| self.call_spawn_provenance.get(id))
                    .cloned()
                    .unwrap_or_else(|| SpawnProvenance {
                        tool_call_id: activity.event_id.clone(),
                        time: EventTime::Untimed,
                        preceding_context: None,
                        task_description: None,
                    });
                self.event(
                    timestamp,
                    EventKind::AgentDiscovered(AgentDescriptor {
                        id: ActorId(agent_id),
                        parent,
                        spawn,
                        spawn_reference: activity.agent_path.clone(),
                        completion_policy: AgentCompletionPolicy::ExplicitLifecycle,
                        role: AgentRole::Subagent,
                        label,
                        agent_type: None,
                        description: None,
                        interactive: false,
                    }),
                )
                .into_iter()
                .collect()
            }
            ActivityKind::Interacted
            | ActivityKind::Completed
            | ActivityKind::Interrupted
            | ActivityKind::Failed => {
                let status = match kind {
                    ActivityKind::Interacted => RecordedAgentStatus::Running,
                    ActivityKind::Completed => RecordedAgentStatus::Completed,
                    ActivityKind::Interrupted => RecordedAgentStatus::Interrupted,
                    ActivityKind::Failed => RecordedAgentStatus::Failed,
                    _ => unreachable!(),
                };
                self.event(
                    timestamp,
                    EventKind::AgentStatus {
                        agent_id: ActorId(agent_id),
                        status,
                    },
                )
                .into_iter()
                .collect()
            }
            ActivityKind::Unknown => Vec::new(),
        }
    }

    fn actor(&self) -> Option<ActorId> {
        self.session
            .as_ref()
            .map(|metadata| ActorId(metadata.session.id.clone()))
    }

    fn event(&self, timestamp: Option<DateTime<Utc>>, kind: EventKind) -> Option<SessionEvent> {
        Some(SessionEvent {
            actor: self.actor()?,
            time: event_time(timestamp),
            kind,
        })
    }
}

fn spawn_reference(output: &Value) -> Option<String> {
    let value = match output {
        Value::String(text) => serde_json::from_str(text).ok()?,
        value @ Value::Object(_) => value.clone(),
        _ => return None,
    };
    value
        .get("task_name")
        .or_else(|| value.get("path"))
        .and_then(Value::as_str)
        .filter(|reference| !reference.is_empty())
        .map(str::to_owned)
}

fn mark_item(
    seen: &mut HashSet<String>,
    current_turn: Option<&str>,
    id: Option<String>,
    fallback: &str,
) -> Option<String> {
    let fact_id = id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| format!("{}:{fallback}", current_turn.unwrap_or("unknown-turn")));
    seen.insert(fact_id.clone()).then_some(fact_id)
}

fn event_time(timestamp: Option<DateTime<Utc>>) -> EventTime {
    timestamp.map_or(EventTime::Untimed, EventTime::At)
}

fn same_usage_counts(left: &UsageObservation, right: &UsageObservation) -> bool {
    left.scope == right.scope
        && left.input_tokens == right.input_tokens
        && left.cached_input_tokens == right.cached_input_tokens
        && left.cache_write_input_tokens == right.cache_write_input_tokens
        && left.output_tokens == right.output_tokens
        && left.reasoning_output_tokens == right.reasoning_output_tokens
}

fn visible_text(content: &[ContentPart], expected_kind: &str) -> String {
    content
        .iter()
        .filter_map(|part| {
            let text = match (expected_kind, part) {
                ("input_text", ContentPart::InputText { text })
                | ("output_text", ContentPart::OutputText { text })
                | ("summary_text", ContentPart::SummaryText { text }) => text,
                _ => return None,
            };
            (!text.trim().is_empty()).then(|| text.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn goal_objective(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("<codex_internal_context source=\"goal\">")?
        .strip_suffix("</codex_internal_context>")?
        .trim();
    let (_, after_open) = body.split_once("<objective>")?;
    let (objective, _) = after_open.split_once("</objective>")?;
    let objective = objective.trim();
    (!objective.is_empty()).then(|| objective.to_owned())
}

fn recorded_label(value: &Value, key: &str) -> Option<String> {
    let text = value.as_str().or_else(|| value.get(key)?.as_str())?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn path_label(path: &str) -> Option<String> {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .map(str::to_owned)
}

fn tool_outcome(output: &Value) -> ToolOutcome {
    match output {
        Value::Object(fields) => {
            if let Some(is_error) = fields.get("is_error").and_then(Value::as_bool) {
                return if is_error {
                    ToolOutcome::Failed
                } else {
                    ToolOutcome::Succeeded
                };
            }
            if let Some(exit_code) = fields.get("exit_code").and_then(Value::as_i64) {
                return if exit_code == 0 {
                    ToolOutcome::Succeeded
                } else {
                    ToolOutcome::Failed
                };
            }
            match fields.get("status").and_then(Value::as_str) {
                Some("ok" | "success" | "succeeded" | "completed") => ToolOutcome::Succeeded,
                Some("error" | "failed" | "cancelled") => ToolOutcome::Failed,
                _ => ToolOutcome::CompletedUnknown,
            }
        }
        Value::String(text) => {
            if let Ok(structured) = serde_json::from_str::<Value>(text) {
                let outcome = tool_outcome(&structured);
                if outcome != ToolOutcome::CompletedUnknown {
                    return outcome;
                }
            }
            let text = text.trim_start();
            if text.starts_with("Script completed") {
                ToolOutcome::Succeeded
            } else if text.starts_with("Script failed") || text.starts_with("exec_command failed") {
                ToolOutcome::Failed
            } else {
                ToolOutcome::CompletedUnknown
            }
        }
        Value::Array(parts) => {
            let mut saw_success = false;
            for part in parts {
                let Some(fields) = part.as_object() else {
                    continue;
                };
                if fields.get("type").and_then(Value::as_str) != Some("input_text") {
                    continue;
                }
                let Some(text) = fields.get("text").and_then(Value::as_str) else {
                    continue;
                };
                match tool_outcome(&Value::String(text.to_owned())) {
                    ToolOutcome::Failed => return ToolOutcome::Failed,
                    ToolOutcome::Succeeded => saw_success = true,
                    ToolOutcome::CompletedUnknown => {}
                }
            }
            if saw_success {
                ToolOutcome::Succeeded
            } else {
                ToolOutcome::CompletedUnknown
            }
        }
        _ => ToolOutcome::CompletedUnknown,
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Record {
    #[serde(rename = "session_meta")]
    SessionMeta {
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
        payload: SessionMetaPayload,
    },
    #[serde(rename = "turn_context")]
    TurnContext {
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
        payload: TurnContextPayload,
    },
    #[serde(rename = "response_item")]
    ResponseItem {
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
        payload: ResponseItem,
    },
    #[serde(rename = "event_msg")]
    EventMsg {
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
        payload: EventMessage,
    },
    #[serde(rename = "inter_agent_communication_metadata")]
    InterAgentCommunicationMetadata { payload: InterAgentMetadata },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    cli_version: Option<String>,
    #[serde(default)]
    source: WireSource,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum WireSource {
    Named(String),
    Subagent {
        subagent: WireSubagentSource,
    },
    #[default]
    Missing,
    Unknown(Value),
}

impl WireSource {
    fn into_origin(self) -> SessionOrigin {
        match self {
            WireSource::Named(name) if !name.is_empty() => SessionOrigin::TopLevel,
            WireSource::Subagent {
                subagent:
                    WireSubagentSource {
                        thread_spawn: Some(thread_spawn),
                    },
            } => SessionOrigin::ThreadSpawn {
                parent_thread_id: thread_spawn.parent_thread_id,
                agent_path: thread_spawn.agent_path,
                agent_nickname: thread_spawn.agent_nickname,
            },
            WireSource::Subagent { .. } => SessionOrigin::Auxiliary,
            WireSource::Named(_) | WireSource::Missing => SessionOrigin::Unknown,
            WireSource::Unknown(value) => {
                let _ = value;
                SessionOrigin::Unknown
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireSubagentSource {
    #[serde(default)]
    thread_spawn: Option<ThreadSpawnSource>,
}

#[derive(Debug, Deserialize)]
struct ThreadSpawnSource {
    #[serde(default)]
    parent_thread_id: String,
    #[serde(default)]
    agent_path: Option<String>,
    #[serde(default)]
    agent_nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnContextPayload {
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    approval_policy: Value,
    #[serde(default)]
    sandbox_policy: Value,
    #[serde(default)]
    active_permission_profile: Value,
    #[serde(default)]
    collaboration_mode: Value,
    #[serde(default)]
    effort: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponseItem {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        content: Vec<ContentPart>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Vec<ContentPart>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },
    #[serde(rename = "custom_tool_call")]
    CustomToolCall {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        input: Option<String>,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        output: Value,
    },
    #[serde(rename = "custom_tool_call_output")]
    CustomToolCallOutput {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        output: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "input_text")]
    InputText {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "output_text")]
    OutputText {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "summary_text")]
    SummaryText {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "encrypted_content")]
    EncryptedContent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum EventMessage {
    #[serde(rename = "task_started")]
    TaskStarted {
        #[serde(default)]
        turn_id: Option<String>,
    },
    #[serde(rename = "user_message")]
    UserMessage {
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(rename = "token_count")]
    TokenCount {
        #[serde(default)]
        info: Option<TokenInfo>,
    },
    #[serde(rename = "sub_agent_activity")]
    SubAgentActivity(SubAgentActivity),
    #[serde(rename = "item_completed")]
    ItemCompleted { item: CompletedItem },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    #[serde(default)]
    total_token_usage: Option<TokenCounts>,
}

#[derive(Debug, Deserialize)]
struct TokenCounts {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cached_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    reasoning_output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct InterAgentMetadata {
    #[serde(default)]
    trigger_turn: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CompletedItem {
    #[serde(rename = "SubAgentActivity")]
    SubAgentActivity(SubAgentActivity),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct SubAgentActivity {
    #[serde(default, alias = "id")]
    event_id: Option<String>,
    #[serde(default)]
    occurred_at_ms: Option<i64>,
    #[serde(default)]
    agent_thread_id: Option<String>,
    #[serde(default)]
    agent_path: Option<String>,
    #[serde(default)]
    kind: Option<ActivityKind>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
enum ActivityKind {
    #[serde(rename = "started")]
    Started,
    #[serde(rename = "interacted")]
    Interacted,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(text: &str) -> (CodexDecoder, Vec<SessionEvent>) {
        let mut decoder = CodexDecoder::new();
        let events = text
            .lines()
            .flat_map(|line| decoder.decode_line(line))
            .collect();
        (decoder, events)
    }

    fn semantic_sequence(events: &[SessionEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match &event.kind {
                EventKind::SessionMetadata(metadata) => {
                    format!("session {}", metadata.session.id)
                }
                EventKind::Activity => "activity".to_owned(),
                EventKind::Prompt { text } => format!("prompt {text}"),
                EventKind::AssistantText { channel, text, .. } => {
                    format!("assistant {channel:?} {text}")
                }
                EventKind::Reasoning { text, .. } => format!("reasoning {text}"),
                EventKind::ModelSelected { model } => format!("model {model}"),
                EventKind::UsageObserved(usage) => {
                    format!("usage {} output={:?}", usage.scope, usage.output_tokens)
                }
                EventKind::ToolStarted(tool) => {
                    format!("tool-start {} {} {:?}", tool.id, tool.name, tool.summary)
                }
                EventKind::ToolFinished(tool) => {
                    format!("tool-finish {} {:?}", tool.id, tool.outcome)
                }
                EventKind::WorkflowDeclared(workflow) => {
                    format!("workflow-declared {}", workflow.id.0)
                }
                EventKind::AgentDiscovered(agent) => format!(
                    "agent-start {} parent={} label={:?}",
                    agent.id.0, agent.parent.0, agent.label
                ),
                EventKind::AgentStatus { agent_id, status } => {
                    format!("agent-status {} {status:?}", agent_id.0)
                }
                EventKind::AgentMetadata(metadata) => {
                    format!("agent-metadata {}", metadata.id.0)
                }
                EventKind::SessionInfo(_) => "session-info".to_owned(),
            })
            .collect()
    }

    fn decoded_summary(name: &str, input: &str, custom: bool) -> Option<String> {
        let transcript = [
            serde_json::json!({"type": "session_meta", "payload": {
                "id": "summary-root", "cwd": "/project", "source": "cli"
            }}),
            serde_json::json!({"type": "response_item", "payload": {
                "type": if custom { "custom_tool_call" } else { "function_call" },
                "call_id": "summary-call", "name": name,
                "input": input, "arguments": input
            }}),
        ]
        .map(|record| record.to_string())
        .join("\n");
        let (_, events) = decode(&transcript);
        events.into_iter().find_map(|event| match event.kind {
            EventKind::ToolStarted(tool) => tool.summary,
            _ => None,
        })
    }

    #[test]
    fn codex_json_arguments_show_commands_paths_and_descriptions() {
        let cases = [
            (
                "exec_command",
                serde_json::json!({"cmd": "cargo test", "description": "run tests"}),
                "cargo test",
            ),
            (
                "shell",
                serde_json::json!({"command": ["git", "status", "--short"]}),
                "git status --short",
            ),
            (
                "view_image",
                serde_json::json!({"path": "/project/output.png"}),
                "output.png",
            ),
            (
                "spawn_agent",
                serde_json::json!({"description": "Review the parser"}),
                "Review the parser",
            ),
            (
                "web__run",
                serde_json::json!({"search_query": [{"q": "Rust parser docs"}]}),
                "Rust parser docs",
            ),
            (
                "write_stdin",
                serde_json::json!({"session_id": 42, "chars": ""}),
                "session 42",
            ),
            (
                "mcp__node_repl__js",
                serde_json::json!({"title": "Inspect the panel", "code": "console.log(panel)"}),
                "Inspect the panel",
            ),
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|(name, input, _)| decoded_summary(name, &input.to_string(), false))
            .collect();
        let expected: Vec<_> = cases
            .iter()
            .map(|(_, _, summary)| Some(summary.to_string()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn direct_patch_calls_show_relative_paths_without_patch_contents() {
        let patch = "*** Begin Patch\n*** Update File: /project/src/旧.rs\n*** Move to: /project/src/new.rs\n@@\n-secret old text\n+new text\n*** Add File: /project-other/extra.rs\n+extra\n*** End Patch";
        assert_eq!(
            decoded_summary("apply_patch", patch, true).as_deref(),
            Some("src/旧.rs, src/new.rs, /project-other/extra.rs")
        );
    }

    #[test]
    fn stdin_summary_requires_a_recorded_session_id() {
        assert_eq!(
            decoded_summary("write_stdin", r#"{"session_id":null}"#, false),
            None
        );
        assert_eq!(
            decoded_summary("write_stdin", r#"{"session_id":42}"#, false).as_deref(),
            Some("session 42")
        );
    }

    #[test]
    fn code_mode_reads_nested_literal_arguments_but_does_not_resolve_expressions() {
        let input = r#"// tools.fake({cmd: 'not a call'});
            const example = "tools.fake({cmd: 'also not a call'})";
            /* tools.fake({path: '/ignored'}); */
            text(await tools.web__run({search_query: [{q: 'Rust parser docs'}]}));
            text(await tools.write_stdin({session_id: 42, chars: ''}));
            text(await tools.exec_command({cmd: command}));"#;
        assert_eq!(
            decoded_summary("exec", input, true).as_deref(),
            Some(
                "web__run: Rust parser docs; write_stdin: session 42; exec_command: { cmd : command }"
            )
        );
    }

    #[test]
    fn code_mode_does_not_infer_commands_from_regexes_or_partial_expressions() {
        let input = r#"const pattern = /tools.fake()/;
            await tools.exec_command({cmd: "printf " + suffix});
            await tools.exec_command({cmd: `echo ${value}`});"#;
        assert_eq!(
            decoded_summary("exec", input, true).as_deref(),
            Some(
                "exec_command: { cmd : \"printf \" + suffix }; exec_command: { cmd : `echo ${value}` }"
            )
        );
    }

    #[test]
    fn code_mode_does_not_claim_a_literal_that_dynamic_fields_may_override() {
        let input = r#"await tools.exec_command({cmd: 'cargo test', ...options});"#;
        assert_eq!(
            decoded_summary("exec", input, true).as_deref(),
            Some("exec_command: { cmd : 'cargo test' , . . . options }")
        );
    }

    #[test]
    fn long_code_mode_summaries_keep_later_operations_visible() {
        let input = format!(
            r#"await tools.exec_command({{cmd: "{}"}});
            await tools.view_image({{path: '/project/output.png'}});"#,
            "检查项目 ".repeat(100)
        );
        let summary = decoded_summary("exec", &input, true).unwrap();
        assert!(summary.chars().count() <= 200, "{summary}");
        assert!(summary.starts_with("exec_command: 检查项目"), "{summary}");
        assert!(summary.ends_with("view_image: output.png"), "{summary}");
    }

    #[test]
    fn code_mode_summarizes_multiple_operations_without_creating_extra_calls() {
        let transcript = [
            serde_json::json!({"type": "session_meta", "payload": {
                "id": "summary-root", "cwd": "/project", "source": "cli"
            }}),
            serde_json::json!({"type": "response_item", "payload": {
                "type": "custom_tool_call", "call_id": "multi", "name": "exec",
                "input": r#"// @exec: {"max_output_tokens": 1000}
                    text(await tools.apply_patch('*** Begin Patch\n*** Update File: /project/src/main.rs\n@@\n-old\n+new\n*** End Patch'));
                    await Promise.all([
                        tools.exec_command({cmd: "cargo test --lib"}),
                        tools.view_image({path: `/project/output.png`})
                    ]);"#
            }}),
            serde_json::json!({"type": "response_item", "payload": {
                "type": "custom_tool_call_output", "call_id": "multi", "output": "Script completed"
            }}),
        ].map(|record| record.to_string()).join("\n");
        let (_, events) = decode(&transcript);
        let calls: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolStarted(tool) => Some((
                    tool.id.as_str(),
                    tool.name.as_str(),
                    tool.summary.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [(
                "multi",
                "exec",
                Some(
                    "apply_patch: src/main.rs; exec_command: cargo test --lib; view_image: output.png"
                )
            )]
        );
        let finishes: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolFinished(tool) => Some((tool.id.as_str(), tool.outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(finishes, [("multi", ToolOutcome::Succeeded)]);
    }

    #[test]
    fn root_fixture_normalizes_each_canonical_fact_once() {
        let (mut decoder, events) = decode(include_str!(
            "../../tests/fixtures/codex/root-current.jsonl"
        ));

        let prompts: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Prompt { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, ["Map the dependency graph."]);

        let assistant: Vec<(&AssistantChannel, &str)> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AssistantText { channel, text, .. } => Some((channel, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant,
            [(&AssistantChannel::Commentary, "I will inspect the graph.")]
        );

        let reasoning: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Reasoning { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, ["The graph has one risky seam."]);

        let models: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ModelSelected { model } => Some(model.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, ["gpt-test"]);

        let starts: Vec<(&str, &str, ToolCategory, Option<&str>)> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolStarted(tool) => Some((
                    tool.id.as_str(),
                    tool.name.as_str(),
                    tool.category,
                    tool.summary.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            [
                (
                    "call-spawn",
                    "spawn_agent",
                    ToolCategory::AgentSpawn,
                    Some("child-a")
                ),
                (
                    "call-ok",
                    "exec",
                    ToolCategory::Ordinary,
                    Some("cargo check")
                ),
                (
                    "call-fail",
                    "exec",
                    ToolCategory::Ordinary,
                    Some("cargo test")
                ),
            ]
        );

        let finishes: Vec<(&str, ToolOutcome)> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolFinished(tool) => Some((tool.id.as_str(), tool.outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(
            finishes,
            [
                ("call-spawn", ToolOutcome::CompletedUnknown),
                ("call-ok", ToolOutcome::Succeeded),
                ("call-fail", ToolOutcome::Failed),
            ]
        );

        let discovered: Vec<&AgentDescriptor> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AgentDiscovered(agent) => Some(agent),
                _ => None,
            })
            .collect();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, ActorId::from("child-thread"));
        assert_eq!(discovered[0].parent, ActorId::from("root-thread"));
        assert_eq!(
            discovered[0].spawn.tool_call_id.as_deref(),
            Some("call-spawn")
        );
        assert_eq!(discovered[0].label.as_deref(), Some("child-a"));
        assert_eq!(discovered[0].role, AgentRole::Subagent);
        assert_eq!(
            discovered[0].spawn.preceding_context.as_deref(),
            Some("The graph has one risky seam.")
        );

        let usages: Vec<&UsageObservation> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::UsageObserved(usage) => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 2, "an identical cumulative sample is ignored");
        assert_eq!(usages[0].output_tokens, Some(10));
        assert_eq!(usages[0].revision, 1);
        assert_eq!(usages[1].output_tokens, Some(25));
        assert_eq!(usages[1].revision, 2);
        assert_eq!(usages[1].scope, "thread:root-thread");

        let statuses: Vec<(ActorId, RecordedAgentStatus)> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AgentStatus { agent_id, status } => Some((agent_id.clone(), *status)),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [(
                ActorId::from("child-thread"),
                RecordedAgentStatus::Completed
            )]
        );
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn root_fixture_has_a_human_reviewable_semantic_snapshot() {
        let (_, events) = decode(include_str!(
            "../../tests/fixtures/codex/root-current.jsonl"
        ));

        assert_eq!(
            semantic_sequence(&events),
            [
                "session root-thread",
                "session-info",
                "prompt Map the dependency graph.",
                "session-info",
                "session-info",
                "model gpt-test",
                "assistant Commentary I will inspect the graph.",
                "reasoning The graph has one risky seam.",
                "tool-start call-spawn spawn_agent Some(\"child-a\")",
                "agent-start child-thread parent=root-thread label=Some(\"child-a\")",
                "tool-finish call-spawn CompletedUnknown",
                "tool-start call-ok exec Some(\"cargo check\")",
                "tool-finish call-ok Succeeded",
                "tool-start call-fail exec Some(\"cargo test\")",
                "tool-finish call-fail Failed",
                "usage thread:root-thread output=Some(10)",
                "usage thread:root-thread output=Some(25)",
                "agent-status child-thread Completed",
            ]
        );
    }

    #[test]
    fn child_prefix_is_suppressed_until_the_exact_owned_turn_marker() {
        let (mut decoder, events) = decode(include_str!(
            "../../tests/fixtures/codex/child-with-prefix.jsonl"
        ));

        let metadata: Vec<&SessionMetadata> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::SessionMetadata(metadata) => Some(metadata),
                _ => None,
            })
            .collect();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].session.id, "child-thread");
        assert_eq!(
            metadata[0].origin,
            SessionOrigin::ThreadSpawn {
                parent_thread_id: "root-thread".to_owned(),
                agent_path: Some("/root/child-a".to_owned()),
                agent_nickname: Some("Ada".to_owned()),
            }
        );

        assert!(events.iter().all(|event| {
            !matches!(&event.kind, EventKind::Prompt { text } if text.contains("copied"))
                && !matches!(&event.kind, EventKind::ToolStarted(tool) if tool.id == "copied-call")
                && !matches!(&event.kind, EventKind::UsageObserved(usage) if usage.output_tokens == Some(999))
                && !matches!(&event.kind, EventKind::AgentDiscovered(agent) if agent.id == ActorId::from("copied-grandchild"))
        }));

        let owned_text: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AssistantText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(owned_text, ["Child-owned output."]);
        let nested = events.iter().find_map(|event| match &event.kind {
            EventKind::AgentDiscovered(agent) if agent.id == ActorId::from("grandchild-thread") => {
                Some(agent)
            }
            _ => None,
        });
        assert_eq!(
            nested.map(|agent| &agent.parent),
            Some(&ActorId::from("child-thread"))
        );
        assert_eq!(
            nested.and_then(|agent| agent.label.as_deref()),
            Some("grandchild")
        );
        let usage = events.iter().find_map(|event| match &event.kind {
            EventKind::UsageObserved(usage) => Some(usage),
            _ => None,
        });
        assert_eq!(usage.and_then(|usage| usage.output_tokens), Some(7));
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn child_without_owned_turn_marker_emits_only_metadata_and_a_diagnostic() {
        let (mut decoder, events) = decode(include_str!(
            "../../tests/fixtures/codex/child-without-marker.jsonl"
        ));
        assert!(events.iter().all(|event| matches!(
            event.kind,
            EventKind::SessionMetadata(_) | EventKind::SessionInfo(_)
        )));
        assert!(events.iter().any(|event| matches!(&event.kind,
            EventKind::SessionInfo(info) if info.cwd.as_deref() == Some("/workspace/demo"))));
        assert_eq!(
            decoder.finish(),
            [DecoderDiagnostic::MissingOwnedTurnMarker]
        );
        assert!(
            decoder.finish().is_empty(),
            "the diagnostic is emitted once"
        );
    }

    #[test]
    fn non_adjacent_mirrored_agent_activity_is_deduplicated_by_stable_id() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"root-thread","source":"cli"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"done-1","kind":"completed","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"touch-1","kind":"interacted","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"done-1","kind":"completed","agent_thread_id":"child"}}"#,
        );
        let (_, events) = decode(text);
        let statuses: Vec<_> = events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::AgentStatus { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [RecordedAgentStatus::Completed, RecordedAgentStatus::Running]
        );
    }

    #[test]
    fn distinct_completion_cycles_survive_interacted_transition() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"root-thread","source":"cli"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"done-1","kind":"completed","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"touch-1","kind":"interacted","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","event_id":"done-2","kind":"completed","agent_thread_id":"child"}}"#,
        );
        let (_, events) = decode(text);
        let statuses: Vec<_> = events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::AgentStatus { status, .. } => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [
                RecordedAgentStatus::Completed,
                RecordedAgentStatus::Running,
                RecordedAgentStatus::Completed,
            ]
        );
    }

    #[test]
    fn unidentified_activity_fallback_uses_resolved_timestamp() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"root-thread","source":"cli"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-01T10:00:01Z","type":"event_msg","payload":{"type":"sub_agent_activity","kind":"completed","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-01T10:00:02Z","type":"event_msg","payload":{"type":"sub_agent_activity","kind":"completed","agent_thread_id":"child"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-01T10:00:02Z","type":"event_msg","payload":{"type":"sub_agent_activity","kind":"completed","agent_thread_id":"child"}}"#,
        );
        let (_, events) = decode(text);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::AgentStatus { .. }))
                .count(),
            2,
            "distinct times survive while an exact no-id mirror is collapsed"
        );
    }

    #[test]
    fn goal_context_is_the_only_response_item_user_fallback() {
        let (mut decoder, events) = decode(include_str!(
            "../../tests/fixtures/codex/goal-continuation.jsonl"
        ));
        let prompts: Vec<&str> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Prompt { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, ["Finish the release checklist."]);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText {
                channel: AssistantChannel::Final,
                text,
                ..
            } if text == "The checklist is complete."
        )));
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn assistant_final_phase_aliases_share_one_domain_channel() {
        let mut decoder = CodexDecoder::new();
        decoder.decode_line(
            r#"{"type":"session_meta","payload":{"id":"phase-thread","source":"cli"}}"#,
        );
        let events: Vec<SessionEvent> = ["final", "final_answer"]
            .into_iter()
            .enumerate()
            .flat_map(|(index, phase)| {
                decoder.decode_line(&format!(
                    r#"{{"type":"response_item","payload":{{"type":"message","id":"answer-{index}","role":"assistant","phase":"{phase}","content":[{{"type":"output_text","text":"answer {index}"}}]}}}}"#
                ))
            })
            .collect();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| matches!(
            &event.kind,
            EventKind::AssistantText {
                channel: AssistantChannel::Final,
                ..
            }
        )));
    }

    #[test]
    fn auxiliary_source_is_typed_without_becoming_a_thread_spawn() {
        let (mut decoder, events) =
            decode(include_str!("../../tests/fixtures/codex/auxiliary.jsonl"));
        let metadata = events.iter().find_map(|event| match &event.kind {
            EventKind::SessionMetadata(metadata) => Some(metadata),
            _ => None,
        });
        assert_eq!(metadata.map(|m| &m.origin), Some(&SessionOrigin::Auxiliary));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "Internal review complete."
        )));
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn spawn_output_preserves_only_an_exact_structural_reference() {
        let (_, events) = decode(concat!(
            r#"{"type":"session_meta","payload":{"id":"root","cwd":"/workspace","source":"cli"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"spawn","output":"{\"task_name\":\"/root/child\"}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"ordinary","output":{"message":"/root/not-a-reference"}}}"#,
            "\n"
        ));
        let references: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::ToolFinished(finish) => finish.spawn_reference.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(references, ["/root/child"]);
    }

    #[test]
    fn malformed_missing_wrong_typed_and_large_records_are_non_fatal() {
        let mut decoder = CodexDecoder::new();
        for line in [
            "",
            "not json",
            r#"{"type":"session_meta","payload":{"id":7}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":[]}}"#,
            r#"{"type":"future_record","payload":{"type":"future_payload"}}"#,
            r#"{"type":"response_item""#,
        ] {
            assert!(
                decoder.decode_line(line).is_empty(),
                "ignored input: {line}"
            );
        }

        let large_unknown = format!(
            r#"{{"type":"future_record","payload":{{"text":"{}"}}}}"#,
            "x".repeat(256 * 1024)
        );
        assert!(decoder.decode_line(&large_unknown).is_empty());

        let valid = r#"{"timestamp":"2026-09-05T10:00:00Z","type":"session_meta","payload":{"id":"after-errors","source":"cli"}}"#;
        let events = decoder.decode_line(valid);
        assert!(matches!(
            &events[..],
            [SessionEvent {
                kind: EventKind::SessionMetadata(SessionMetadata { session, .. }),
                ..
            }] if session.id == "after-errors"
        ));
    }
}
