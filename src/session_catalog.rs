//! Native, provider-neutral session discovery.
//!
//! Discovery reads only bounded rollout headers. It does not parse transcript
//! bodies; provider decoders retain ownership of that work.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::event::{Provider, SessionKey};
use crate::transcript;

pub const DEFAULT_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionKind {
    Root,
    Spawned,
    Auxiliary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub key: SessionKey,
    pub path: PathBuf,
    pub cwd: Option<PathBuf>,
    pub kind: SessionKind,
    pub parent_thread_id: Option<String>,
    pub agent_path: Option<String>,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestFileRole {
    Root,
    Spawned,
    ClaudeSubagent {
        agent_id: String,
        workflow: Option<String>,
    },
    ClaudeSubagentMetadata {
        agent_id: String,
        workflow: Option<String>,
    },
    ClaudeWorkflowJournal {
        workflow: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: PathBuf,
    pub role: ManifestFileRole,
    pub session: Option<SessionKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticMetadataEvent {
    pub child: SessionKey,
    pub parent: SessionKey,
    pub agent_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionManifest {
    pub root: SessionRef,
    pub files: Vec<ManifestFile>,
    pub metadata: Vec<SyntheticMetadataEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTarget {
    LatestForCwd(PathBuf),
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct DiscoveryRoots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
}

impl DiscoveryRoots {
    pub fn from_home(home: &Path) -> Self {
        Self {
            claude_projects: home.join(".claude/projects"),
            codex_sessions: home.join(".codex/sessions"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    pub candidates_read: usize,
    pub bytes_read: usize,
}

#[derive(Debug, Clone)]
struct CachedHeader {
    len: u64,
    modified: SystemTime,
    identity: FileIdentity,
    session: Option<SessionRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity(Option<(u64, u64)>);

pub struct SessionCatalog {
    roots: DiscoveryRoots,
    header_bytes: usize,
    codex: HashMap<PathBuf, CachedHeader>,
    last_stats: RefreshStats,
}

impl SessionCatalog {
    pub fn new(roots: DiscoveryRoots) -> Self {
        Self::with_header_limit(roots, DEFAULT_HEADER_BYTES)
    }

    pub fn with_header_limit(roots: DiscoveryRoots, header_bytes: usize) -> Self {
        Self {
            roots,
            header_bytes: header_bytes.max(1),
            codex: HashMap::new(),
            last_stats: RefreshStats::default(),
        }
    }

    pub fn last_refresh_stats(&self) -> RefreshStats {
        self.last_stats
    }

    /// Refresh provider indexes once. Live following owns the cadence; this
    /// catalog owns file eligibility and bounded header caching.
    pub(crate) fn refresh(&mut self) {
        self.refresh_codex();
    }

    pub(crate) fn manifest_for_root(&self, root: &SessionRef) -> SessionManifest {
        self.manifest_for(root.clone())
    }

    pub(crate) fn latest_for_cwd_cached(&self, cwd: &Path) -> Option<SessionRef> {
        let wanted = comparable_path(cwd);
        let mut candidates = self.claude_candidates(&wanted);
        candidates.extend(self.codex_root_candidates(&wanted));
        candidates.into_iter().max_by(candidate_order)
    }

    /// Whether a replaced tracked file has enough positive provider evidence
    /// to rebuild its family. Partial and unknown content must keep the old
    /// snapshot visible until a complete replacement can be classified.
    pub(crate) fn replacement_ready(&self, path: &Path) -> bool {
        read_explicit_codex(path, self.header_bytes).is_some()
            || looks_like_claude(path, self.header_bytes)
    }

    /// Resolve a replaced root without scanning the Codex history until the
    /// watched handle has a complete, valid header again.
    pub(crate) fn replacement_manifest(&mut self, path: &Path) -> Option<SessionManifest> {
        if let Some(root) = read_explicit_codex(path, self.header_bytes) {
            self.refresh_codex();
            let root = self
                .codex
                .get(&comparable_path(path))
                .and_then(|cached| cached.session.clone())
                .unwrap_or(root);
            return Some(self.manifest_for(root));
        }
        looks_like_claude(path, self.header_bytes)
            .then(|| self.claude_ref(&comparable_path(path), None))
            .flatten()
            .map(claude_manifest)
    }

    pub fn candidates_for_cwd(&mut self, cwd: &Path) -> Vec<SessionRef> {
        self.refresh_codex();
        let wanted = comparable_path(cwd);
        let mut candidates = self.claude_candidates(&wanted);
        candidates.extend(self.codex_root_candidates(&wanted));
        candidates.sort_by(candidate_order);
        candidates
    }

    pub fn latest_for_cwd(&mut self, cwd: &Path) -> Option<SessionRef> {
        self.candidates_for_cwd(cwd)
            .into_iter()
            .max_by(candidate_order)
    }

    pub fn manifest(&mut self, target: &WatchTarget) -> Option<SessionManifest> {
        match target {
            WatchTarget::LatestForCwd(cwd) => {
                let root = self.latest_for_cwd(cwd)?;
                Some(self.manifest_for(root))
            }
            WatchTarget::File(path) => {
                self.refresh_codex();
                let path = comparable_path(path);
                let codex = self
                    .codex
                    .get(&path)
                    .and_then(|cached| cached.session.clone())
                    .or_else(|| read_explicit_codex(&path, self.header_bytes));
                let root = match codex {
                    Some(session) => session,
                    None if looks_like_claude(&path, self.header_bytes)
                        || !is_rollout_name(&path) =>
                    {
                        self.claude_ref(&path, None)?
                    }
                    None => return None,
                };
                Some(self.manifest_for(root))
            }
        }
    }

    fn manifest_for(&self, root: SessionRef) -> SessionManifest {
        match root.key.provider {
            Provider::Claude => claude_manifest(root),
            Provider::Codex => self.codex_manifest(root),
        }
    }

    fn claude_candidates(&self, cwd: &Path) -> Vec<SessionRef> {
        let project = self
            .roots
            .claude_projects
            .join(transcript::sanitize_cwd(cwd));
        let Ok(entries) = fs::read_dir(project) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| transcript::is_session_file(&entry.path()))
            .filter_map(|entry| self.claude_ref(&entry.path(), Some(cwd)))
            .collect()
    }

    fn claude_ref(&self, path: &Path, cwd: Option<&Path>) -> Option<SessionRef> {
        let name = path.file_stem()?.to_str()?;
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            return None;
        }
        Some(SessionRef {
            key: SessionKey {
                provider: Provider::Claude,
                id: name.to_owned(),
            },
            path: path.to_owned(),
            cwd: cwd.map(Path::to_owned),
            kind: SessionKind::Root,
            parent_thread_id: None,
            agent_path: None,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        })
    }

    fn codex_root_candidates(&self, wanted: &Path) -> Vec<SessionRef> {
        let sessions = self.codex_sessions();
        sessions
            .iter()
            .filter(|session| {
                session.kind == SessionKind::Root
                    && session
                        .cwd
                        .as_deref()
                        .is_some_and(|cwd| comparable_path(cwd) == wanted)
            })
            .map(|root| {
                let mut candidate = (*root).clone();
                let family = family_ids(&sessions, &root.key.id);
                for child in &sessions {
                    if family.contains(&child.key.id) {
                        candidate.modified = candidate.modified.max(child.modified);
                    }
                }
                candidate
            })
            .collect()
    }

    fn refresh_codex(&mut self) {
        let mut paths = Vec::new();
        collect_rollouts(&self.roots.codex_sessions, 3, &mut paths);
        paths.sort();
        let seen: HashSet<_> = paths.iter().cloned().collect();
        self.codex.retain(|path, _| seen.contains(path));
        self.last_stats = RefreshStats::default();

        for path in paths {
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let identity = file_identity(&metadata);
            let unchanged = self.codex.get(&path).is_some_and(|cached| {
                cached.len == metadata.len()
                    && cached.modified == modified
                    && identity.0.is_some()
                    && cached.identity == identity
            });
            if unchanged {
                continue;
            }
            self.last_stats.candidates_read += 1;
            let session = read_codex_header(
                &path,
                metadata.len(),
                modified,
                self.header_bytes,
                &mut self.last_stats.bytes_read,
            );
            self.codex.insert(
                path,
                CachedHeader {
                    len: metadata.len(),
                    modified,
                    identity,
                    session,
                },
            );
        }
    }

    fn codex_manifest(&self, root: SessionRef) -> SessionManifest {
        let catalog_sessions = self.codex_sessions();
        let included = family_ids(&catalog_sessions, &root.key.id);
        let mut sessions: Vec<_> = catalog_sessions
            .into_iter()
            .filter(|session| included.contains(&session.key.id) && session.key != root.key)
            .cloned()
            .collect();
        sessions.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.path.cmp(&b.path)));
        sessions.dedup_by(|a, b| a.key == b.key);
        let parent_by_id: HashMap<_, _> = sessions
            .iter()
            .filter_map(|session| {
                session
                    .parent_thread_id
                    .as_ref()
                    .map(|parent| (session.key.id.clone(), parent.clone()))
            })
            .collect();
        sessions.sort_by(|a, b| {
            family_depth(a, &root.key.id, &parent_by_id)
                .cmp(&family_depth(b, &root.key.id, &parent_by_id))
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| a.path.cmp(&b.path))
        });
        sessions.insert(0, root.clone());
        let files = sessions
            .iter()
            .map(|session| ManifestFile {
                path: session.path.clone(),
                role: if session.key == root.key {
                    ManifestFileRole::Root
                } else {
                    ManifestFileRole::Spawned
                },
                session: Some(session.key.clone()),
            })
            .collect();
        let metadata = sessions
            .iter()
            .filter_map(|session| {
                if session.kind != SessionKind::Spawned {
                    return None;
                }
                let parent = session.parent_thread_id.as_ref()?;
                if parent.is_empty() || !included.contains(parent) {
                    return None;
                }
                Some(SyntheticMetadataEvent {
                    child: session.key.clone(),
                    parent: SessionKey {
                        provider: Provider::Codex,
                        id: parent.clone(),
                    },
                    agent_path: session.agent_path.clone(),
                })
            })
            .collect();
        SessionManifest {
            root,
            files,
            metadata,
        }
    }

    fn codex_sessions(&self) -> Vec<&SessionRef> {
        self.codex
            .values()
            .filter_map(|cached| cached.session.as_ref())
            .collect()
    }
}

fn family_depth(
    session: &SessionRef,
    root_id: &str,
    parent_by_id: &HashMap<String, String>,
) -> usize {
    let mut depth = 1;
    let mut parent = session.parent_thread_id.as_deref();
    while let Some(id) = parent {
        if id == root_id {
            return depth;
        }
        depth += 1;
        if depth > parent_by_id.len() + 1 {
            break;
        }
        parent = parent_by_id.get(id).map(String::as_str);
    }
    // The family closure excludes disconnected chains; this is only a
    // defensive deterministic fallback for malformed cyclic ancestry.
    usize::MAX
}

fn family_ids(sessions: &[&SessionRef], root_id: &str) -> HashSet<String> {
    let mut included = HashSet::from([root_id.to_owned()]);
    loop {
        let before = included.len();
        for child in sessions {
            if child.kind == SessionKind::Spawned
                && child
                    .parent_thread_id
                    .as_ref()
                    .is_some_and(|parent| included.contains(parent))
            {
                included.insert(child.key.id.clone());
            }
        }
        if included.len() == before {
            return included;
        }
    }
}

fn claude_manifest(root: SessionRef) -> SessionManifest {
    let mut files = vec![ManifestFile {
        path: root.path.clone(),
        role: ManifestFileRole::Root,
        session: Some(root.key.clone()),
    }];
    if let Some(subagents) = transcript::subagents_dir(&root.path) {
        for child in transcript::scan_subagent_files(&subagents, None) {
            let agent_id = child.agent_id;
            let workflow = child.workflow;
            files.push(ManifestFile {
                path: child.transcript,
                role: ManifestFileRole::ClaudeSubagent {
                    agent_id: agent_id.clone(),
                    workflow: workflow.clone(),
                },
                session: None,
            });
            if child.meta.is_file() {
                files.push(ManifestFile {
                    path: child.meta,
                    role: ManifestFileRole::ClaudeSubagentMetadata { agent_id, workflow },
                    session: None,
                });
            }
        }
        for workflow in transcript::scan_workflow_ids(&subagents) {
            let journal = transcript::workflow_journal(&subagents, &workflow);
            if journal.is_file() {
                files.push(ManifestFile {
                    path: journal,
                    role: ManifestFileRole::ClaudeWorkflowJournal {
                        workflow: workflow.clone(),
                    },
                    session: None,
                });
            }
            for child in transcript::scan_subagent_files(
                &transcript::workflow_dir(&subagents, &workflow),
                Some(&workflow),
            ) {
                let agent_id = child.agent_id;
                let workflow = child.workflow;
                files.push(ManifestFile {
                    path: child.transcript,
                    role: ManifestFileRole::ClaudeSubagent {
                        agent_id: agent_id.clone(),
                        workflow: workflow.clone(),
                    },
                    session: None,
                });
                if child.meta.is_file() {
                    files.push(ManifestFile {
                        path: child.meta,
                        role: ManifestFileRole::ClaudeSubagentMetadata { agent_id, workflow },
                        session: None,
                    });
                }
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    SessionManifest {
        root,
        files,
        metadata: Vec::new(),
    }
}

fn candidate_order(a: &SessionRef, b: &SessionRef) -> std::cmp::Ordering {
    a.modified
        .cmp(&b.modified)
        .then_with(|| a.key.provider.cmp(&b.key.provider))
        .then_with(|| a.path.cmp(&b.path))
}

fn comparable_path(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| lexical_absolute(path))
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth > 0 {
            collect_rollouts(&path, depth - 1, out);
        } else if kind.is_file() && depth == 0 && is_rollout_name(&path) {
            out.push(comparable_path(&path));
        }
    }
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity(Some((metadata.dev(), metadata.ino())))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity(None)
}

fn is_rollout_name(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with("rollout-"))
}

#[derive(Deserialize)]
struct SessionMetaLine {
    #[serde(rename = "type")]
    kind: String,
    payload: SessionMetaPayload,
}
#[derive(Deserialize)]
struct SessionMetaPayload {
    id: String,
    cwd: PathBuf,
    #[serde(default)]
    source: Option<serde_json::Value>,
    #[serde(default)]
    parent_thread_id: Option<String>,
    #[serde(default)]
    agent_path: Option<String>,
}

fn read_codex_header(
    path: &Path,
    file_len: u64,
    modified: SystemTime,
    limit: usize,
    bytes_read: &mut usize,
) -> Option<SessionRef> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    *bytes_read += bytes.len();
    let first = match bytes.iter().position(|byte| *byte == b'\n') {
        Some(end) => &bytes[..end],
        None if file_len <= bytes.len() as u64 => bytes.as_slice(),
        None => return None,
    };
    let line: SessionMetaLine = serde_json::from_slice(first).ok()?;
    if line.kind != "session_meta" || line.payload.id.is_empty() || !line.payload.cwd.is_absolute()
    {
        return None;
    }
    let source = line.payload.source.as_ref()?;
    let thread_spawn = source.pointer("/subagent/thread_spawn");
    let auxiliary = source.pointer("/subagent/other").is_some();
    let kind = if thread_spawn.is_some() {
        SessionKind::Spawned
    } else if auxiliary || source.get("subagent").is_some() {
        SessionKind::Auxiliary
    } else {
        SessionKind::Root
    };
    let parent_thread_id = thread_spawn
        .and_then(|value| value.get("parent_thread_id"))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or(line.payload.parent_thread_id);
    let agent_path = thread_spawn
        .and_then(|value| value.get("agent_path"))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or(line.payload.agent_path);
    Some(SessionRef {
        key: SessionKey {
            provider: Provider::Codex,
            id: line.payload.id,
        },
        path: path.to_owned(),
        cwd: Some(line.payload.cwd),
        kind,
        parent_thread_id,
        agent_path,
        modified,
    })
}

fn read_explicit_codex(path: &Path, limit: usize) -> Option<SessionRef> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut ignored = 0;
    read_codex_header(
        path,
        metadata.len(),
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        limit,
        &mut ignored,
    )
}

fn looks_like_claude(path: &Path, limit: usize) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .by_ref()
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(bytes.len());
    serde_json::from_slice::<serde_json::Value>(&bytes[..end])
        .ok()
        .and_then(|value| value.get("type")?.as_str().map(str::to_owned))
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "user"
                    | "assistant"
                    | "system"
                    | "attachment"
                    | "ai-title"
                    | "last-prompt"
                    | "mode"
                    | "permission-mode"
                    | "file-history-snapshot"
                    | "queue-operation"
                    | "started"
                    | "result"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::Duration;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zoetrope-catalog-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn roots(&self) -> DiscoveryRoots {
            DiscoveryRoots {
                claude_projects: self.0.join("claude"),
                codex_sessions: self.0.join("codex"),
            }
        }

        fn rollout(&self, day: &str, name: &str, header: &str) -> PathBuf {
            let path = self
                .0
                .join("codex/2026/09")
                .join(day)
                .join(format!("rollout-{name}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!("{header}\nbody that discovery must not parse"),
            )
            .unwrap();
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root(id: &str, cwd: &Path) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":{},"source":"cli"}}}}"#,
            serde_json::to_string(cwd).unwrap()
        )
    }

    fn child(id: &str, parent: &str, cwd: &Path) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":{},"source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}","agent_path":"agent/{id}"}}}}}}}}}}"#,
            serde_json::to_string(cwd).unwrap()
        )
    }

    #[test]
    fn discovery_filters_by_cwd_and_root_eligibility() {
        let tree = TempTree::new("eligibility");
        let cwd = tree.0.join("work");
        fs::create_dir_all(&cwd).unwrap();
        tree.rollout("01", "root", &root("root", &cwd));
        tree.rollout("01", "other", &root("other", &tree.0.join("elsewhere")));
        tree.rollout("01", "child", &child("child", "root", &cwd));
        tree.rollout("01", "helper", &format!(r#"{{"type":"session_meta","payload":{{"id":"helper","cwd":{},"source":{{"subagent":{{"other":"guardian"}}}}}}}}"#, serde_json::to_string(&cwd).unwrap()));
        tree.rollout("01", "malformed", "not json");
        tree.rollout("01", "empty-cwd", &root("empty-cwd", Path::new("")));
        let noise = tree.0.join("codex/2026/09/01/rollout-directory.jsonl");
        fs::create_dir_all(&noise).unwrap();

        let mut catalog = SessionCatalog::new(tree.roots());
        let candidates = catalog.candidates_for_cwd(&cwd);
        assert_eq!(
            candidates
                .iter()
                .map(|item| item.key.id.as_str())
                .collect::<Vec<_>>(),
            ["root"]
        );
    }

    #[test]
    fn discovery_preserves_claude_and_chooses_newest_across_providers() {
        let tree = TempTree::new("mixed");
        let cwd = tree.0.join("work");
        fs::create_dir_all(&cwd).unwrap();
        let normalized_cwd = comparable_path(&cwd);
        let claude_dir = tree
            .roots()
            .claude_projects
            .join(transcript::sanitize_cwd(&normalized_cwd));
        fs::create_dir_all(&claude_dir).unwrap();
        let claude = claude_dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        fs::write(&claude, "{}\n").unwrap();
        let codex = tree.rollout("01", "root", &root("codex", &cwd));
        File::options()
            .write(true)
            .open(&claude)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2)),
            )
            .unwrap();
        File::options()
            .write(true)
            .open(&codex)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2)),
            )
            .unwrap();

        let mut catalog = SessionCatalog::new(tree.roots());
        let candidates = catalog.candidates_for_cwd(&cwd);
        assert_eq!(candidates.len(), 2);
        assert_eq!(catalog.latest_for_cwd(&cwd).unwrap().key.id, "codex");
    }

    #[test]
    fn discovery_normalizes_a_relative_cwd_before_claude_lookup() {
        let tree = TempTree::new("relative-cwd");
        let cwd = comparable_path(Path::new("."));
        let claude_dir = tree
            .roots()
            .claude_projects
            .join(transcript::sanitize_cwd(&cwd));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("11111111-1111-1111-1111-111111111111.jsonl"),
            "{}\n",
        )
        .unwrap();

        let mut catalog = SessionCatalog::new(tree.roots());
        let chosen = catalog.latest_for_cwd(Path::new(".")).unwrap();
        assert_eq!(chosen.cwd, Some(cwd));
        assert_eq!(chosen.key.provider, Provider::Claude);
    }

    #[test]
    fn discovery_reads_no_more_than_the_header_limit_and_caches_unchanged_files() {
        let tree = TempTree::new("bounded");
        let cwd = tree.0.join("work");
        let path = tree.rollout("01", "oversized", &"x".repeat(1024));
        fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).unwrap();
        let mut catalog = SessionCatalog::with_header_limit(tree.roots(), 512);
        assert!(catalog.candidates_for_cwd(&cwd).is_empty());
        assert_eq!(
            catalog.last_refresh_stats(),
            RefreshStats {
                candidates_read: 1,
                bytes_read: 512
            }
        );
        assert!(catalog.candidates_for_cwd(&cwd).is_empty());
        #[cfg(unix)]
        assert_eq!(catalog.last_refresh_stats(), RefreshStats::default());
        #[cfg(not(unix))]
        assert_eq!(
            catalog.last_refresh_stats(),
            RefreshStats {
                candidates_read: 1,
                bytes_read: 512
            }
        );
        fs::write(&path, format!("{}\n", root("now-valid", &cwd))).unwrap();
        assert_eq!(catalog.candidates_for_cwd(&cwd)[0].key.id, "now-valid");
        assert!(catalog.last_refresh_stats().bytes_read <= 512);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_invalidates_a_same_size_same_mtime_replacement() {
        let tree = TempTree::new("replacement");
        let cwd = tree.0.join("work");
        let other = tree.0.join("away");
        let path = tree.rollout("01", "root", &root("root", &cwd));
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(11);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
        let mut catalog = SessionCatalog::new(tree.roots());
        assert_eq!(catalog.candidates_for_cwd(&cwd).len(), 1);

        let replacement = path.with_extension("replacement");
        let old = fs::read_to_string(&path).unwrap();
        let replaced = old.replace(cwd.to_str().unwrap(), other.to_str().unwrap());
        assert_eq!(old.len(), replaced.len());
        fs::write(&replacement, replaced).unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(FileTimes::new().set_modified(time))
            .unwrap();
        fs::rename(replacement, path).unwrap();

        assert!(catalog.candidates_for_cwd(&cwd).is_empty());
        assert_eq!(catalog.candidates_for_cwd(&other).len(), 1);
    }

    #[test]
    fn equal_mtime_choice_is_deterministic() {
        let tree = TempTree::new("tie");
        let cwd = tree.0.join("work");
        let a = tree.rollout("01", "a", &root("a", &cwd));
        let b = tree.rollout("01", "b", &root("b", &cwd));
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(7);
        for path in [&a, &b] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
        }
        let mut catalog = SessionCatalog::new(tree.roots());
        assert_eq!(
            catalog.latest_for_cwd(&cwd).unwrap().path,
            comparable_path(&b)
        );
    }

    #[test]
    fn descendant_activity_keeps_its_root_family_current() {
        let tree = TempTree::new("family-freshness");
        let cwd = tree.0.join("work");
        let active_root = tree.rollout("01", "active-root", &root("active-root", &cwd));
        let child_path = tree.rollout("01", "child", &child("child", "active-root", &cwd));
        let quiet_root = tree.rollout("01", "quiet-root", &root("quiet-root", &cwd));
        for (path, seconds) in [(&active_root, 1), (&quiet_root, 2), (&child_path, 3)] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        }

        let mut catalog = SessionCatalog::new(tree.roots());
        assert_eq!(catalog.latest_for_cwd(&cwd).unwrap().key.id, "active-root");
    }

    #[test]
    fn manifest_closes_over_nested_children_and_pins_an_explicit_child() {
        let tree = TempTree::new("closure");
        let cwd = tree.0.join("work");
        tree.rollout("01", "grandchild", &child("grandchild", "child", &cwd));
        let root_path = tree.rollout("03", "root", &root("root", &cwd));
        let child_path = tree.rollout("02", "child", &child("child", "root", &cwd));
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(root_path)).unwrap();
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|file| file.session.as_ref().unwrap().id.as_str())
                .collect::<Vec<_>>(),
            ["root", "child", "grandchild"]
        );
        assert_eq!(
            manifest
                .metadata
                .iter()
                .map(|metadata| metadata.child.id.as_str())
                .collect::<Vec<_>>(),
            ["child", "grandchild"]
        );

        let pinned = catalog.manifest(&WatchTarget::File(child_path)).unwrap();
        assert_eq!(pinned.root.key.id, "child");
        assert_eq!(
            pinned
                .files
                .iter()
                .map(|file| file.session.as_ref().unwrap().id.as_str())
                .collect::<Vec<_>>(),
            ["child", "grandchild"]
        );
    }

    #[test]
    fn claude_manifest_carries_subagent_and_workflow_identity() {
        let tree = TempTree::new("claude-manifest");
        let root_path = tree.0.join("11111111-1111-1111-1111-111111111111.jsonl");
        fs::write(&root_path, "{}\n").unwrap();
        let subagents = transcript::subagents_dir(&root_path).unwrap();
        let direct = subagents.join("agent-direct.jsonl");
        let direct_meta = subagents.join("agent-direct.meta.json");
        let workflow_dir = subagents.join("workflows/wf-one");
        fs::create_dir_all(&workflow_dir).unwrap();
        fs::write(&direct, "{}\n").unwrap();
        fs::write(&direct_meta, "{}").unwrap();
        fs::write(workflow_dir.join("journal.jsonl"), "{}\n").unwrap();
        fs::write(workflow_dir.join("agent-nested.jsonl"), "{}\n").unwrap();
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(root_path)).unwrap();
        assert!(manifest.files.iter().any(|file| {
            file.role
                == (ManifestFileRole::ClaudeSubagent {
                    agent_id: "direct".into(),
                    workflow: None,
                })
        }));
        assert!(manifest.files.iter().any(|file| {
            file.role
                == (ManifestFileRole::ClaudeSubagentMetadata {
                    agent_id: "direct".into(),
                    workflow: None,
                })
        }));
        assert!(manifest.files.iter().any(|file| {
            file.role
                == (ManifestFileRole::ClaudeWorkflowJournal {
                    workflow: "wf-one".into(),
                })
        }));
        assert!(manifest.files.iter().any(|file| {
            file.role
                == (ManifestFileRole::ClaudeSubagent {
                    agent_id: "nested".into(),
                    workflow: Some("wf-one".into()),
                })
        }));
    }

    #[test]
    fn explicit_codex_file_outside_discovery_root_is_pinned() {
        let tree = TempTree::new("external-file");
        let cwd = tree.0.join("work");
        let path = tree.0.join("saved-rollout.jsonl");
        fs::write(
            &path,
            format!("{}\n", child("saved-child", "missing", &cwd)),
        )
        .unwrap();
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(path.clone())).unwrap();
        assert_eq!(manifest.root.key.id, "saved-child");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, comparable_path(&path));
    }

    #[test]
    fn explicit_claude_file_named_like_a_rollout_is_content_sniffed() {
        let tree = TempTree::new("claude-rollout-name");
        let path = tree.0.join("rollout-copy.jsonl");
        fs::write(&path, r#"{"type":"user","message":{"content":"hello"}}"#).unwrap();
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(path)).unwrap();
        assert_eq!(manifest.root.key.provider, Provider::Claude);
        assert_eq!(manifest.root.cwd, None);
    }

    #[test]
    fn explicit_alias_does_not_duplicate_a_catalogued_root() {
        let tree = TempTree::new("path-alias");
        let cwd = tree.0.join("work");
        let path = tree.rollout("01", "root", &root("root", &cwd));
        let alias = path.parent().unwrap().join("../01/rollout-root.jsonl");
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(alias)).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, comparable_path(&path));
    }

    #[test]
    fn refresh_discovers_a_new_day_bucket() {
        let tree = TempTree::new("refresh");
        let cwd = tree.0.join("work");
        tree.rollout("01", "first", &root("first", &cwd));
        let mut catalog = SessionCatalog::new(tree.roots());
        assert_eq!(catalog.candidates_for_cwd(&cwd).len(), 1);
        tree.rollout("02", "second", &root("second", &cwd));
        assert_eq!(catalog.candidates_for_cwd(&cwd).len(), 2);
    }
}
