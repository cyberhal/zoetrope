//! Native, provider-neutral session discovery.
//!
//! Automatic discovery reads only bounded rollout headers. Explicit paths and
//! replacement readiness stream records until positive provider evidence is
//! found; provider decoders retain ownership of transcript meaning.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::event::{Provider, SessionKey, SessionOrigin};
use crate::formats::{SessionProbe, SessionProber, probe_session_bytes};
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
    /// Once a pinned/replacement path required record streaming, later file
    /// growth must preserve that policy instead of silently demoting it to the
    /// automatic 64 KiB budget.
    streaming: bool,
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
        read_streaming_probe(path, false)
            .ok()
            .and_then(|read| read.probe)
            .is_some()
    }

    /// Resolve a replaced root without scanning the Codex history until the
    /// watched handle has a complete, valid header again.
    #[cfg(test)]
    pub(crate) fn replacement_manifest(&mut self, path: &Path) -> Option<SessionManifest> {
        self.replacement_manifest_with_overlays(path, std::iter::empty::<&PathBuf>())
    }

    /// Rebuild a root family while preserving positively resolved changed
    /// members that exceed the automatic discovery budget.
    pub(crate) fn replacement_manifest_with_overlays<'a>(
        &mut self,
        path: &Path,
        overlays: impl IntoIterator<Item = &'a PathBuf>,
    ) -> Option<SessionManifest> {
        let path = comparable_path(path);
        let root_read = read_explicit_session_detail(&path)?;
        let root = root_read.session.clone();
        if root.key.provider == Provider::Codex {
            self.refresh_codex();
            self.cache_explicit_codex(root_read);
            for overlay in overlays {
                if let Some(read) = read_explicit_session_detail(overlay)
                    && read.session.key.provider == Provider::Codex
                {
                    self.cache_explicit_codex(read);
                }
            }
            Some(self.manifest_for(root))
        } else {
            Some(claude_manifest(root))
        }
    }

    fn cache_explicit_codex(&mut self, read: ExplicitSession) {
        let path = read.session.path.clone();
        let identity = file_identity(&read.metadata);
        self.codex.insert(
            path,
            CachedHeader {
                len: read.metadata.len(),
                modified: read.metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                identity,
                session: Some(read.session),
                streaming: true,
            },
        );
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
                let root = self
                    .codex
                    .get(&path)
                    .and_then(|cached| cached.session.clone())
                    .or_else(|| read_explicit_session(&path))?;
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
                        .is_some_and(|cwd| cwd.is_absolute() && comparable_path(cwd) == wanted)
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
            let stream_known_path = self.codex.get(&path).is_some_and(|cached| cached.streaming);
            let read = if stream_known_path {
                read_streaming_probe(&path, false).ok()
            } else {
                read_bounded_probe(&path, self.header_bytes).ok()
            };
            let (len, modified, identity, session) = if let Some(read) = read {
                self.last_stats.bytes_read += read.bytes_read;
                let len = read.metadata.len();
                let modified = read.metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let identity = file_identity(&read.metadata);
                let session = read
                    .probe
                    .and_then(|probe| codex_ref_from_probe(&path, modified, probe));
                (len, modified, identity, session)
            } else {
                (metadata.len(), modified, identity, None)
            };
            self.codex.insert(
                path,
                CachedHeader {
                    len,
                    modified,
                    identity,
                    session,
                    streaming: stream_known_path,
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

struct BoundedProbe {
    metadata: fs::Metadata,
    probe: Option<SessionProbe>,
    bytes_read: usize,
}

fn read_bounded_probe(path: &Path, limit: usize) -> std::io::Result<BoundedProbe> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit as u64).read_to_end(&mut bytes)?;
    let bytes_read = bytes.len();
    let at_eof = metadata.len() <= bytes_read as u64;
    Ok(BoundedProbe {
        metadata,
        probe: probe_session_bytes(&bytes, at_eof),
        bytes_read,
    })
}

/// Probe a pinned file without imposing the automatic discovery budget.
/// Fixed-size chunks and [`SessionProber`]'s per-record cap keep memory bounded
/// even when positive evidence follows a long preamble or malformed record.
fn read_streaming_probe(
    path: &Path,
    continue_until_claude_id: bool,
) -> std::io::Result<BoundedProbe> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let mut prober = SessionProber::default();
    let mut chunk = [0_u8; 64 * 1024];
    let mut bytes_read = 0;
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes_read += read;
        prober.push_bytes(&chunk[..read]);
        match prober.probe() {
            Some(SessionProbe::Codex(_)) => break,
            Some(SessionProbe::Claude(identity))
                if !continue_until_claude_id || identity.first_session_id().is_some() =>
            {
                break;
            }
            Some(SessionProbe::Claude(_)) | None => {}
        }
    }
    Ok(BoundedProbe {
        metadata,
        probe: prober.finish(),
        bytes_read,
    })
}

fn codex_ref_from_probe(
    path: &Path,
    modified: SystemTime,
    probe: SessionProbe,
) -> Option<SessionRef> {
    let SessionProbe::Codex(metadata) = probe else {
        return None;
    };
    let cwd = metadata.cwd.map(PathBuf::from);
    let (kind, parent_thread_id, agent_path) = match metadata.origin {
        SessionOrigin::TopLevel => (SessionKind::Root, None, None),
        SessionOrigin::ThreadSpawn {
            parent_thread_id,
            agent_path,
            ..
        } => (SessionKind::Spawned, Some(parent_thread_id), agent_path),
        SessionOrigin::Auxiliary | SessionOrigin::Unknown => (SessionKind::Auxiliary, None, None),
    };
    Some(SessionRef {
        key: metadata.session,
        path: path.to_owned(),
        cwd,
        kind,
        parent_thread_id,
        agent_path,
        modified,
    })
}

struct ExplicitSession {
    session: SessionRef,
    metadata: fs::Metadata,
}

fn read_explicit_session_detail(path: &Path) -> Option<ExplicitSession> {
    let read = read_streaming_probe(path, true).ok()?;
    let modified = read.metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let session = match read.probe {
        Some(probe @ SessionProbe::Codex(_)) => codex_ref_from_probe(path, modified, probe),
        Some(SessionProbe::Claude(identity)) => {
            let id = identity
                .first_session_id()
                .map(str::to_owned)
                .or_else(|| path.file_stem()?.to_str().map(str::to_owned))?;
            Some(SessionRef {
                key: SessionKey::new(Provider::Claude, id),
                path: path.to_owned(),
                cwd: None,
                kind: SessionKind::Root,
                parent_thread_id: None,
                agent_path: None,
                modified,
            })
        }
        None if transcript::is_session_file(path) => Some(SessionRef {
            key: SessionKey::new(Provider::Claude, path.file_stem()?.to_str()?.to_owned()),
            path: path.to_owned(),
            cwd: None,
            kind: SessionKind::Root,
            parent_thread_id: None,
            agent_path: None,
            modified,
        }),
        None => None,
    }?;
    Some(ExplicitSession {
        session,
        metadata: read.metadata,
    })
}

fn read_explicit_session(path: &Path) -> Option<SessionRef> {
    read_explicit_session_detail(path).map(|read| read.session)
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
        tree.rollout(
            "01",
            "relative-cwd",
            &root("relative-cwd", Path::new("work")),
        );
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
    fn explicit_provider_detection_skips_wrong_shaped_noise_before_codex_header() {
        let tree = TempTree::new("provider-collision");
        let cwd = tree.0.join("work");
        let path = tree.0.join("recording.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                r#"{"type":"user"}"#,
                root("actual-thread", &cwd)
            ),
        )
        .unwrap();
        let mut catalog = SessionCatalog::new(tree.roots());

        let manifest = catalog.manifest(&WatchTarget::File(path)).unwrap();
        assert_eq!(
            manifest.root.key,
            SessionKey::new(Provider::Codex, "actual-thread")
        );
    }

    #[test]
    fn whitespace_codex_identity_is_not_provider_evidence() {
        let tree = TempTree::new("whitespace-id");
        let cwd = tree.0.join("work");
        let path = tree.rollout(
            "01",
            "blank",
            &format!(
                r#"{{"type":"session_meta","payload":{{"id":"  ","cwd":{},"source":"cli"}}}}"#,
                serde_json::to_string(&cwd).unwrap()
            ),
        );
        let mut catalog = SessionCatalog::new(tree.roots());

        assert!(catalog.candidates_for_cwd(&cwd).is_empty());
        assert!(catalog.manifest(&WatchTarget::File(path)).is_none());
    }

    #[test]
    fn replacement_readiness_uses_the_shared_positive_probe() {
        let tree = TempTree::new("replacement-probe");
        let cwd = tree.0.join("work");
        let path = tree.0.join("recording.jsonl");
        let catalog = SessionCatalog::new(tree.roots());

        fs::write(&path, r#"{"type":"user"}"#).unwrap();
        assert!(!catalog.replacement_ready(&path));

        fs::write(
            &path,
            format!("{}\n{}", r#"{"type":"user"}"#, root("ready", &cwd)),
        )
        .unwrap();
        assert!(catalog.replacement_ready(&path));

        fs::write(
            &path,
            r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#,
        )
        .unwrap();
        assert!(catalog.replacement_ready(&path));
    }

    #[test]
    fn explicit_and_replacement_probe_past_the_automatic_discovery_budget() {
        let tree = TempTree::new("explicit-large-record-scan");
        let cwd = tree.0.join("work");
        let mut text = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": "late-header",
                "cwd": cwd,
                "source": "cli",
                "padding": "x".repeat(DEFAULT_HEADER_BYTES + 4096),
            }
        })
        .to_string();
        assert!(text.len() > DEFAULT_HEADER_BYTES);
        text.push('\n');
        let path = tree.rollout("01", "late-header", &text);
        let mut catalog = SessionCatalog::new(tree.roots());

        assert!(
            catalog.candidates_for_cwd(&cwd).is_empty(),
            "automatic discovery stays within its 64 KiB budget"
        );
        let manifest = catalog
            .manifest(&WatchTarget::File(path.clone()))
            .expect("a pinned path scans complete records past that budget");
        assert_eq!(
            manifest.root.key,
            SessionKey::new(Provider::Codex, "late-header")
        );
        assert!(catalog.replacement_ready(&path));
        assert_eq!(
            catalog.replacement_manifest(&path).unwrap().root.key,
            SessionKey::new(Provider::Codex, "late-header")
        );
        assert_eq!(
            crate::formats::detect_session(&text),
            (Provider::Codex, Some("late-header".into())),
            "portable and native probes share classification semantics"
        );
    }

    #[test]
    fn streaming_probe_stops_after_a_decisive_header_chunk() {
        let tree = TempTree::new("streaming-probe-early-stop");
        let cwd = tree.0.join("work");
        let cases = [
            (
                "codex.jsonl",
                format!("{}\n", root("codex-early", &cwd)),
                Provider::Codex,
            ),
            (
                "claude.jsonl",
                concat!(
                    r#"{"type":"user","sessionId":"claude-early","message":{"role":"user","content":"start"}}"#,
                    "\n"
                )
                .to_owned(),
                Provider::Claude,
            ),
        ];
        for (name, header, expected_provider) in cases {
            let path = tree.0.join(name);
            let mut contents = header.into_bytes();
            contents.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
            fs::write(&path, &contents).unwrap();

            let read = read_streaming_probe(&path, true).unwrap();
            let provider = match read.probe.unwrap() {
                SessionProbe::Codex(_) => Provider::Codex,
                SessionProbe::Claude(_) => Provider::Claude,
            };
            assert_eq!(provider, expected_provider);
            assert!(read.bytes_read <= 64 * 1024);
            assert!(read.bytes_read < read.metadata.len() as usize);
        }
    }

    #[test]
    fn explicit_claude_file_named_like_a_rollout_is_content_sniffed() {
        let tree = TempTree::new("claude-rollout-name");
        let path = tree.0.join("rollout-copy.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
        )
        .unwrap();
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
