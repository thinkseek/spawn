//! The transcript archive in a local directory (docs/storage.md,
//! "Transcript archive"): `<root>/<agent>/<native>.jsonl` is copy 0 of a
//! native session's transcript, `<root>/<agent>/<native>.<n>.jsonl` copy
//! `n`. The Bridge's root is `<workspace root>/.transcripts`.
//!
//! The progress records ([`spawn_core::transcripts::ArchiveProgress`]) are
//! beside it, not in it: `<root>-progress/<agent>/<native>.json`
//! (`<workspace root>/.transcripts-progress`), one JSON object each,
//! replaced whole (a temporary file renamed over it), so the archive's own
//! files stay the transcripts' bytes alone. So are the copies' records
//! (objective 175, [`spawn_core::transcripts::CopySource`]):
//! `<root>-sources/<agent>/<native>.<copy>.json`, through the same writer,
//! durably.
//!
//! Every file is reached from the directory above the archive (the
//! workspace root) through [`crate::beneath`] (objective 175 R7): a link
//! below it (`.transcripts`, an agent's directory, a copy, a record) is
//! never followed, and a file with a second hard link is never read or
//! written; each is an error, and nothing is copied.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use spawn_core::id::AgentId;
use spawn_core::port::{ArchiveError, TranscriptArchive};
use spawn_core::transcripts::{ArchiveProgress, CopySource};

use crate::beneath::Beneath;

/// The directory of the workspace root the archive lives in.
pub const TRANSCRIPT_DIRECTORY: &str = ".transcripts";

/// Transcript archive in a directory of this host.
#[derive(Debug, Clone)]
pub struct LocalTranscriptArchive {
    /// The directory above the archive, every file reached from it.
    below: Beneath,
    /// The archive's own directory name (`.transcripts`).
    name: String,
}

impl LocalTranscriptArchive {
    /// Archive under `root`, created as copies are written; its progress
    /// records under `<root>-progress`, its copies' records under
    /// `<root>-sources`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let above = root.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::below(Beneath::new(above), &root)
    }

    /// The loop's archive, `<workspace root>/.transcripts`, reached from
    /// `workspace_root` as given: pinned when the loop's mask is a
    /// stand-in's ([`Beneath::pinned`]), so a root swapped for a link after
    /// the pin writes nothing through it.
    #[must_use]
    pub fn for_loop(workspace_root: Beneath) -> Self {
        let root = workspace_root.root().join(TRANSCRIPT_DIRECTORY);
        Self::below(workspace_root, &root)
    }

    fn below(below: Beneath, root: &Path) -> Self {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self { below, name }
    }

    /// `<name><suffix>/<agent>`, relative to the directory above.
    fn agent_dir(&self, suffix: &str, agent: &AgentId) -> PathBuf {
        PathBuf::from(format!("{}{suffix}", self.name)).join(agent.as_str())
    }

    /// The file name of copy `copy` of `native` ([`plain_names`]).
    fn copy_name(agent: &AgentId, native: &str, copy: usize) -> Result<String, ArchiveError> {
        plain_names(agent, native)?;
        Ok(if copy == 0 {
            format!("{native}.jsonl")
        } else {
            format!("{native}.{copy}.jsonl")
        })
    }

    /// Copy `copy` of `native`, relative to the directory above.
    fn copy_path(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
    ) -> Result<PathBuf, ArchiveError> {
        Ok(self
            .agent_dir("", agent)
            .join(Self::copy_name(agent, native, copy)?))
    }

    /// Whether copy `copy` of `native` exists.
    fn made(&self, agent: &AgentId, native: &str, copy: usize) -> Result<bool, ArchiveError> {
        Ok(self
            .below
            .open_read(&self.copy_path(agent, native, copy)?)
            .map_err(ArchiveError)?
            .is_some())
    }
}

/// A native id names a file, so it is held to letters, digits, `-` and `_`
/// (Claude Code's are UUIDs), and so is the agent.
fn plain_names(agent: &AgentId, native: &str) -> Result<(), ArchiveError> {
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    if !plain(native) || !plain(agent.as_str()) {
        return Err(ArchiveError(format!(
            "{agent}/{native} does not name an archive file"
        )));
    }
    Ok(())
}

fn failed(what: &str, path: &Path, error: &std::io::Error) -> ArchiveError {
    ArchiveError(format!("{what} {}: {error}", path.display()))
}

impl TranscriptArchive for LocalTranscriptArchive {
    fn copies(&self, agent: &AgentId, native: &str) -> Result<Vec<u64>, ArchiveError> {
        let mut lengths = Vec::new();
        loop {
            let path = self.copy_path(agent, native, lengths.len())?;
            let Some(file) = self.below.open_read(&path).map_err(ArchiveError)? else {
                return Ok(lengths);
            };
            let length = file
                .metadata()
                .map_err(|error| failed("stat", &self.below.root().join(&path), &error))?
                .len();
            lengths.push(length);
        }
    }

    fn read(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, ArchiveError> {
        let path = self.copy_path(agent, native, copy)?;
        let shown = self.below.root().join(&path);
        let mut file = self
            .below
            .open_read(&path)
            .map_err(ArchiveError)?
            .ok_or_else(|| ArchiveError(format!("open {}: no such copy", shown.display())))?;
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| {
                file.take(u64::try_from(limit).unwrap_or(u64::MAX))
                    .read_to_end(&mut bytes)
            })
            .map_err(|error| failed("read", &shown, &error))?;
        Ok(bytes)
    }

    fn append(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        at: u64,
        bytes: &[u8],
    ) -> Result<(), ArchiveError> {
        let name = Self::copy_name(agent, native, copy)?;
        if copy > 0 && !self.made(agent, native, copy - 1)? {
            return Err(ArchiveError(format!(
                "copy {copy} of {native} would be past the next"
            )));
        }
        let directory = self
            .below
            .make_dir(&self.agent_dir("", agent))
            .map_err(ArchiveError)?;
        let shown = self
            .below
            .root()
            .join(self.agent_dir("", agent))
            .join(&name);
        // Only a copy's first append creates it: a refused one leaves no
        // empty copy behind.
        let Some((mut file, created)) = self
            .below
            .open_append(&directory, &name, at == 0)
            .map_err(ArchiveError)?
        else {
            return Err(ArchiveError(format!(
                "copy {copy} of {native} holds 0 bytes, not {at}"
            )));
        };
        // Two writers of one copy take turns; each checks the length under
        // the lock, so neither writes over the other.
        file.lock()
            .map_err(|error| failed("lock", &shown, &error))?;
        let length = file
            .metadata()
            .map_err(|error| failed("stat", &shown, &error))?
            .len();
        if length != at {
            return Err(ArchiveError(format!(
                "copy {copy} of {native} holds {length} bytes, not {at}"
            )));
        }
        file.write_all(bytes)
            .and_then(|()| file.sync_data())
            .map_err(|error| failed("write", &shown, &error))?;
        // A new file is durable once its directory is.
        if created {
            directory.sync().map_err(ArchiveError)?;
        }
        Ok(())
    }

    fn put_progress(&self, progress: &ArchiveProgress, durable: bool) -> Result<(), ArchiveError> {
        plain_names(&progress.agent, &progress.native)?;
        let directory = self
            .below
            .make_dir(&self.agent_dir("-progress", &progress.agent))
            .map_err(ArchiveError)?;
        let mut line = progress.to_json();
        line.push('\n');
        self.below
            .replace_whole(
                &directory,
                &format!("{}.json", progress.native),
                line.as_bytes(),
                durable,
            )
            .map_err(ArchiveError)
    }

    fn progress(&self) -> Result<Vec<ArchiveProgress>, ArchiveError> {
        let root = PathBuf::from(format!("{}-progress", self.name));
        let mut records = Vec::new();
        for (agent, is_dir) in self.below.list(&root).map_err(ArchiveError)? {
            if !is_dir {
                continue;
            }
            let directory = root.join(&agent);
            for (name, is_dir) in self.below.list(&directory).map_err(ArchiveError)? {
                if is_dir || name.starts_with('.') || !name.ends_with(".json") {
                    continue;
                }
                if let Some(bytes) = self
                    .below
                    .read(&directory.join(&name))
                    .map_err(ArchiveError)?
                {
                    records.extend(ArchiveProgress::from_json(&bytes));
                }
            }
        }
        records.sort_by(|a, b| (&a.agent, &a.native).cmp(&(&b.agent, &b.native)));
        Ok(records)
    }

    fn source(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
    ) -> Result<Option<CopySource>, ArchiveError> {
        plain_names(agent, native)?;
        let path = self
            .agent_dir("-sources", agent)
            .join(format!("{native}.{copy}.json"));
        Ok(self
            .below
            .read(&path)
            .map_err(ArchiveError)?
            .and_then(|bytes| CopySource::from_json(&bytes)))
    }

    fn put_source(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        source: &CopySource,
    ) -> Result<(), ArchiveError> {
        if !self.made(agent, native, copy)? {
            return Err(ArchiveError(format!(
                "no copy {copy} of {native} to record"
            )));
        }
        let directory = self
            .below
            .make_dir(&self.agent_dir("-sources", agent))
            .map_err(ArchiveError)?;
        let mut line = source.to_json();
        line.push('\n');
        self.below
            .replace_whole(
                &directory,
                &format!("{native}.{copy}.json"),
                line.as_bytes(),
                true,
            )
            .map_err(ArchiveError)
    }
}

#[cfg(test)]
mod tests {
    use super::LocalTranscriptArchive;
    use spawn_core::id::{AgentId, ExecutionId, SessionId, WorkspaceId};
    use spawn_core::port::TranscriptArchive;
    use spawn_core::transcripts::ArchiveProgress;

    #[test]
    fn the_local_archive_passes_the_shared_contract() {
        let root = std::env::temp_dir().join(format!("spawn-transcripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let archive = LocalTranscriptArchive::new(&root);
        spawn_core::contract::transcript_archive("local", &archive);
        assert!(
            root.join("agent_local")
                .join("0b7c1d2e-0000-4000-8000-000000000001.1.jsonl")
                .is_file(),
            "copy 1 is its own file"
        );
        // A native id that is not a plain name never becomes a path.
        let agent = AgentId::new("agent_local").expect("valid agent");
        assert!(archive.append(&agent, "../escape", 0, 0, b"x").is_err());
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    /// The progress records live beside the archive, not in it, and are
    /// read back as written; one record per native session, the latest.
    #[test]
    fn progress_records_are_kept_beside_the_archive() {
        let base = std::env::temp_dir().join(format!("spawn-progress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let archive = LocalTranscriptArchive::new(base.join(".transcripts"));
        assert!(archive.progress().expect("none yet").is_empty());
        let mut record = ArchiveProgress {
            agent: AgentId::new("agent_local").expect("valid agent"),
            native: "0b7c1d2e-0000-4000-8000-000000000001".to_string(),
            session: SessionId::new("session_local").expect("valid session"),
            execution: ExecutionId::new("exec_1").expect("valid execution"),
            workspace: WorkspaceId::new("w:/w").expect("valid workspace"),
            live: true,
            known_ms: 10,
            archived: None,
            behind_ms: Some(5),
        };
        archive.put_progress(&record, false).expect("put");
        record.live = false;
        record.behind_ms = None;
        archive.put_progress(&record, true).expect("put again");
        assert_eq!(archive.progress().expect("read"), vec![record.clone()]);
        assert!(
            base.join(
                ".transcripts-progress/agent_local/0b7c1d2e-0000-4000-8000-000000000001.json"
            )
            .is_file()
        );
        assert!(
            !base.join(".transcripts").exists(),
            "the archive holds no record"
        );
        // Something that is not a record is left out.
        std::fs::write(base.join(".transcripts-progress/agent_local/x.json"), b"{").expect("write");
        assert_eq!(archive.progress().expect("read"), vec![record]);
        std::fs::remove_dir_all(&base).expect("clean up");
    }
}
