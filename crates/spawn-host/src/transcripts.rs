//! The transcript archive in a local directory (docs/storage.md,
//! "Transcript archive"): `<root>/<agent>/<native>.jsonl` is copy 0 of a
//! native session's transcript, `<root>/<agent>/<native>.<n>.jsonl` copy
//! `n`. The Bridge's root is `<workspace root>/.transcripts`.
//!
//! The progress records ([`spawn_core::transcripts::ArchiveProgress`]) are
//! beside it, not in it: `<root>-progress/<agent>/<native>.json`
//! (`<workspace root>/.transcripts-progress`), one JSON object each,
//! replaced whole (a temporary file renamed over it), so the archive's own
//! files stay the transcripts' bytes alone.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use spawn_core::id::AgentId;
use spawn_core::port::{ArchiveError, TranscriptArchive};
use spawn_core::transcripts::ArchiveProgress;

/// The directory of the workspace root the archive lives in.
pub const TRANSCRIPT_DIRECTORY: &str = ".transcripts";

/// Transcript archive in a directory of this host.
#[derive(Debug, Clone)]
pub struct LocalTranscriptArchive {
    root: PathBuf,
    progress: PathBuf,
}

impl LocalTranscriptArchive {
    /// Archive under `root`, created as copies are written; its progress
    /// records under `<root>-progress`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut name = root
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push("-progress");
        let progress = root.with_file_name(name);
        Self { root, progress }
    }

    /// The progress record of `native`.
    fn progress_file(&self, agent: &AgentId, native: &str) -> Result<PathBuf, ArchiveError> {
        plain_names(agent, native)?;
        Ok(self
            .progress
            .join(agent.as_str())
            .join(format!("{native}.json")))
    }

    /// The file of copy `copy` of `native` ([`plain_names`]).
    fn file(&self, agent: &AgentId, native: &str, copy: usize) -> Result<PathBuf, ArchiveError> {
        plain_names(agent, native)?;
        let name = if copy == 0 {
            format!("{native}.jsonl")
        } else {
            format!("{native}.{copy}.jsonl")
        };
        Ok(self.root.join(agent.as_str()).join(name))
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
            let path = self.file(agent, native, lengths.len())?;
            match std::fs::metadata(&path) {
                Ok(metadata) => lengths.push(metadata.len()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(lengths),
                Err(error) => return Err(failed("stat", &path, &error)),
            }
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
        let path = self.file(agent, native, copy)?;
        let mut file = File::open(&path).map_err(|error| failed("open", &path, &error))?;
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| {
                file.take(u64::try_from(limit).unwrap_or(u64::MAX))
                    .read_to_end(&mut bytes)
            })
            .map_err(|error| failed("read", &path, &error))?;
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
        let path = self.file(agent, native, copy)?;
        if copy > 0 {
            let before = self.file(agent, native, copy - 1)?;
            if !before.exists() {
                return Err(ArchiveError(format!(
                    "copy {copy} of {native} would be past the next"
                )));
            }
        }
        let directory = path.parent().unwrap_or(&self.root).to_path_buf();
        let fresh = !directory.exists();
        std::fs::create_dir_all(&directory)
            .map_err(|error| failed("create", &directory, &error))?;
        let created = !path.exists();
        // Only a copy's first append creates it: a refused one leaves no
        // empty copy behind.
        let mut file = OpenOptions::new()
            .create(at == 0)
            .append(true)
            .open(&path)
            .map_err(|error| failed("open", &path, &error))?;
        // Two writers of one copy take turns; each checks the length under
        // the lock, so neither writes over the other.
        file.lock().map_err(|error| failed("lock", &path, &error))?;
        let length = file
            .metadata()
            .map_err(|error| failed("stat", &path, &error))?
            .len();
        if length != at {
            return Err(ArchiveError(format!(
                "copy {copy} of {native} holds {length} bytes, not {at}"
            )));
        }
        file.write_all(bytes)
            .and_then(|()| file.sync_data())
            .map_err(|error| failed("write", &path, &error))?;
        // A new file, or a new directory, is durable once its directory is.
        let sync = |directory: &Path| {
            File::open(directory)
                .and_then(|handle| handle.sync_all())
                .map_err(|error| failed("sync", directory, &error))
        };
        if created {
            sync(&directory)?;
        }
        if fresh && let Some(above) = directory.parent() {
            sync(above)?;
        }
        Ok(())
    }
    fn put_progress(&self, progress: &ArchiveProgress, durable: bool) -> Result<(), ArchiveError> {
        let path = self.progress_file(&progress.agent, &progress.native)?;
        let directory = path.parent().unwrap_or(&self.progress).to_path_buf();
        std::fs::create_dir_all(&directory)
            .map_err(|error| failed("create", &directory, &error))?;
        let temporary = directory.join(format!(
            ".{}.{}.tmp",
            progress.native,
            std::process::id()
        ));
        let written = File::create(&temporary).and_then(|mut file| {
            file.write_all(progress.to_json().as_bytes())?;
            file.write_all(b"\n")?;
            if durable {
                file.sync_data()?;
            }
            Ok(())
        });
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(failed("write", &temporary, &error));
        }
        std::fs::rename(&temporary, &path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            failed("rename", &path, &error)
        })?;
        if durable {
            File::open(&directory)
                .and_then(|handle| handle.sync_all())
                .map_err(|error| failed("sync", &directory, &error))?;
        }
        Ok(())
    }

    fn progress(&self) -> Result<Vec<ArchiveProgress>, ArchiveError> {
        read_progress(&self.progress)
    }
}

/// Every progress record under `directory` (`<agent>/<native>.json`); a
/// directory that does not exist holds none, and a file that is not a
/// record is left out.
fn read_progress(directory: &Path) -> Result<Vec<ArchiveProgress>, ArchiveError> {
    let agents = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(failed("read", directory, &error)),
    };
    let mut records = Vec::new();
    for agent in agents.filter_map(Result::ok) {
        if !agent.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = agent.path();
        let files = std::fs::read_dir(&path).map_err(|error| failed("read", &path, &error))?;
        for file in files.filter_map(Result::ok) {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let path = file.path();
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(failed("read", &path, &error)),
            };
            records.extend(ArchiveProgress::from_json(&bytes));
        }
    }
    records.sort_by(|a, b| (&a.agent, &a.native).cmp(&(&b.agent, &b.native)));
    Ok(records)
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
            base.join(".transcripts-progress/agent_local/0b7c1d2e-0000-4000-8000-000000000001.json")
                .is_file()
        );
        assert!(!base.join(".transcripts").exists(), "the archive holds no record");
        // Something that is not a record is left out.
        std::fs::write(base.join(".transcripts-progress/agent_local/x.json"), b"{").expect("write");
        assert_eq!(archive.progress().expect("read"), vec![record]);
        std::fs::remove_dir_all(&base).expect("clean up");
    }
}
