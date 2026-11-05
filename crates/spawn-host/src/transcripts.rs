//! The transcript archive in a local directory (docs/storage.md,
//! "Transcript archive"): `<root>/<agent>/<native>.jsonl` is copy 0 of a
//! native session's transcript, `<root>/<agent>/<native>.<n>.jsonl` copy
//! `n`. The Bridge's root is `<workspace root>/.transcripts`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use spawn_core::id::AgentId;
use spawn_core::port::{ArchiveError, TranscriptArchive};

/// The directory of the workspace root the archive lives in.
pub const TRANSCRIPT_DIRECTORY: &str = ".transcripts";

/// Transcript archive in a directory of this host.
#[derive(Debug, Clone)]
pub struct LocalTranscriptArchive {
    root: PathBuf,
}

impl LocalTranscriptArchive {
    /// Archive under `root`, created as copies are written.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The file of copy `copy` of `native`. A native id names a file, so it
    /// is held to letters, digits, `-` and `_` (Claude Code's are UUIDs).
    fn file(&self, agent: &AgentId, native: &str, copy: usize) -> Result<PathBuf, ArchiveError> {
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
        let name = if copy == 0 {
            format!("{native}.jsonl")
        } else {
            format!("{native}.{copy}.jsonl")
        };
        Ok(self.root.join(agent.as_str()).join(name))
    }
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
}

#[cfg(test)]
mod tests {
    use super::LocalTranscriptArchive;
    use spawn_core::id::AgentId;
    use spawn_core::port::TranscriptArchive;

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
}
