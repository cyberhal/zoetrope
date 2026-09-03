//! Provider-neutral facts emitted by session-format decoders.
//!
//! Wire schemas belong to `crate::formats`; reducers and timelines consume
//! these types so provider-specific accounting and record shapes stop at the
//! decoder boundary.

use chrono::{DateTime, Utc};

/// A session producer supported by zoetrope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Claude,
    Codex,
}

/// Provider-qualified session identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub provider: Provider,
    pub id: String,
}

#[cfg(test)]
impl From<&str> for SessionKey {
    fn from(id: &str) -> Self {
        Self {
            provider: Provider::Claude,
            id: id.to_owned(),
        }
    }
}

#[cfg(test)]
impl From<String> for SessionKey {
    fn from(id: String) -> Self {
        Self {
            provider: Provider::Claude,
            id,
        }
    }
}

/// Stable identity of an agent within a session family.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorId(pub String);

impl From<String> for ActorId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ActorId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// When a fact belongs on the content timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventTime {
    At(DateTime<Utc>),
    /// An untimed fact whose true position is the target agent's first event.
    AtAgentStart(ActorId),
    /// An untimed fact whose true position is the target agent's last event.
    AtAgentEnd(ActorId),
    /// Session metadata with no honest timeline position.
    Untimed,
}

/// One normalized session fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvent {
    pub actor: ActorId,
    pub time: EventTime,
    pub kind: EventKind,
}

/// Semantic facts shared by every provider adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    SessionMetadata(SessionMetadata),
    SessionInfo(SessionInfoPatch),
    /// A transcript record with no richer semantic payload. It keeps provider
    /// activity and ordering observable without inventing visible content.
    Activity,
    Prompt {
        text: String,
    },
    AssistantText {
        channel: AssistantChannel,
        text: String,
    },
    Reasoning {
        text: String,
    },
    ModelSelected {
        model: String,
    },
    UsageObserved(UsageObservation),
    ToolStarted(ToolStart),
    ToolFinished(ToolFinish),
    WorkflowDeclared(WorkflowDescriptor),
    AgentDiscovered(AgentDescriptor),
    AgentMetadata(AgentMetadataPatch),
    AgentStatus {
        agent_id: ActorId,
        status: RecordedAgentStatus,
    },
}

/// Identity metadata retained from a session header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    pub session: SessionKey,
    pub cwd: Option<String>,
    pub producer_version: Option<String>,
    pub origin: SessionOrigin,
}

/// How a session relates to a larger provider-owned session family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOrigin {
    TopLevel,
    ThreadSpawn {
        parent_thread_id: String,
        agent_path: Option<String>,
        agent_nickname: Option<String>,
    },
    /// Provider-internal helper session, not a user-visible spawned thread.
    Auxiliary,
    Unknown,
}

/// Session-level values that do not constitute timeline activity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfoPatch {
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub permission_mode: Option<String>,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub queued_ops_delta: u32,
    pub file_snapshots_delta: u32,
}

/// Which visible assistant stream produced a text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantChannel {
    Commentary,
    Final,
    Other(String),
}

/// Absolute token counters for one stable accounting scope.
///
/// Within a `scope`, the observation with the highest `revision` replaces
/// older values. Consumers never need to know whether a provider reports
/// per-request or cumulative session totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageObservation {
    pub scope: String,
    /// Monotonic within `scope`; higher revisions supersede lower ones even
    /// when delivery order differs from file order.
    pub revision: u64,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
}

/// A tool invocation at its start time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStart {
    pub id: String,
    pub name: String,
    pub category: ToolCategory,
    pub summary: Option<String>,
    /// Context attached at the call site for spawning tools. Kept on the call
    /// because structural child metadata can arrive from another file later.
    pub spawn: Option<SpawnProvenance>,
}

/// Semantic category used by consumers instead of provider tool-name checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Ordinary,
    AgentSpawn,
    WorkflowSpawn,
}

/// A tool result paired to a prior call by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFinish {
    pub id: String,
    pub outcome: ToolOutcome,
    /// Whether this completion is also evidence that a synchronously spawned
    /// child finished. Provider adapters decide this semantic distinction.
    pub completes_spawn: bool,
}

/// What a result record proves about the invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    /// A result exists, but its payload contains no trustworthy status.
    CompletedUnknown,
}

/// A structurally discovered non-root actor and its strongest known provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDescriptor {
    pub id: ActorId,
    pub parent: ActorId,
    pub spawn: SpawnProvenance,
    pub role: AgentRole,
    pub label: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub interactive: bool,
}

/// Provider-normalized context for the call that caused an agent to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnProvenance {
    pub tool_call_id: Option<String>,
    pub time: EventTime,
    /// Nearest preceding assistant text or reasoning supplied by the adapter.
    pub preceding_context: Option<String>,
}

/// Structural role of a discovered non-root actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    Subagent,
    WorkflowGroup,
}

/// A workflow label that may arrive before or after its group is discovered.
///
/// This records metadata only; consumers must not fabricate an empty workflow
/// group until a real member or other structural evidence discovers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDescriptor {
    pub id: ActorId,
    pub parent: ActorId,
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Non-structural metadata for an actor discovered elsewhere.
///
/// Consumers retain a patch that arrives early, but do not create a graph node
/// until `AgentDiscovered` supplies structural evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMetadataPatch {
    pub id: ActorId,
    pub label: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub interactive: Option<bool>,
    pub spawn: Option<SpawnProvenance>,
}

/// Lifecycle evidence written explicitly by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedAgentStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}
