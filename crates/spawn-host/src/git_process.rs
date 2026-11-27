//! Shared `git` child-process invocation, locale pinning, and plumbing helpers.
//!
//! Keeps locale-neutral `git` process construction (`LC_ALL=C`, no
//! `LANGUAGE`) and test `git` configuration isolation outside `git.rs` so
//! non-workspace boundary adapters (`git_border`, `d6_cutover`, `cow`,
//! `overlay`, and CLI `sources`) invoke `git` without coupling to the Git
//! workspace store.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Output};

use spawn_core::port::WorkspaceError;

/// The locale every git Spawn runs gets (`LC_ALL`): git's own, untranslated
/// messages.
const GIT_LOCALE: &str = "C";

/// A command that runs `program` (the git to run) the way Spawn runs every
/// git: in the C locale, `LC_ALL=C` and no `LANGUAGE`.
/// Every git of workspace adapters and of the controller (`spawn-cli`'s
/// `controller_git`) is built here, so none can be left to the host's
/// locale.
///
/// Spawn reads what git answers: a push's summary, a merge's conflict
/// kinds, a listing, an exit code, and the error it quotes in a record or a
/// note. Under a translated locale (`LANG=zh_CN.UTF-8`) git, and the
/// `git-receive-pack` or hook it starts, writes its messages in that
/// language; nothing Spawn decides may change with the host's language.
/// `LC_ALL` wins over `LANG` and every `LC_*`; `LANGUAGE`, which gettext
/// would otherwise read first, is removed. Paths, listings and commit
/// messages are bytes to git in any locale.
pub fn git_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", GIT_LOCALE).env_remove("LANGUAGE");
    command
}

/// A command that runs git the way a test runs its own (a seed's `init`,
/// `add` and `commit`, a look at a ref or an index): [`git_command`],
/// without the user's or the system's git configuration.
///
/// `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` leave out
/// `~/.gitconfig`, `$XDG_CONFIG_HOME/git/config` and `/etc/gitconfig`, and
/// configuration inherited through the environment (`GIT_CONFIG_COUNT`,
/// `GIT_CONFIG_PARAMETERS`) is removed. So a host whose configuration signs
/// every commit (`commit.gpgsign` with a key no agent holds: `ssh-keygen -Y
/// sign` waits forever), runs hooks (`core.hooksPath`) or refuses to guess
/// an identity (`user.useConfigOnly`) neither hangs nor fails a test, and
/// a configuration a test sets on the command itself still counts. It
/// leaves out every setting, not just signing: no setting of the host's
/// can reach a test's own git. Standard input is empty; the identity
/// (`spawn-test`) is one a test may override with its own `-c`.
///
/// Never Spawn's own git: what Spawn runs must hold under the host's
/// configuration, and a test runs it that way (`spawn-cli`'s
/// `spawns_own_commits_ignore_forced_signing`).
#[cfg(any(test, feature = "testing"))]
pub fn test_git_command() -> Command {
    let mut command = git_command("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(std::process::Stdio::null())
        .args([
            "-c",
            "user.name=spawn-test",
            "-c",
            "user.email=spawn@localhost",
        ]);
    command
}

/// Stdout of a git command exactly as written, bytes and all, or the
/// failure as an error. A `-z` listing is read this way: a path may begin
/// or end with whitespace, which trimming would cut off the first or the
/// last path, and its bytes need not be UTF-8 ([`crate::tree_path::TreePath`]).
pub(crate) fn report_listing(
    output: std::io::Result<Output>,
    word: &str,
) -> Result<Vec<u8>, WorkspaceError> {
    let output =
        output.map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
    if !output.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git {word}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Trimmed stdout of a git command that prints one value (a commit, a
/// tree), or the failure as an error. Never for a listing of paths.
pub(crate) fn report(
    output: std::io::Result<Output>,
    word: &str,
) -> Result<String, WorkspaceError> {
    report_listing(output, word).map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
}

/// git arguments joined for an error message.
pub(crate) fn shown_arguments<S: AsRef<OsStr>>(arguments: &[S]) -> String {
    arguments
        .iter()
        .map(|argument| argument.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Bytes of pathspecs one `git ls-tree` call carries on its command line:
/// far under Linux's 2 MiB for arguments and environment together, and
/// over any single path, so every batch holds at least one.
const PATHSPEC_BATCH_BYTES: usize = 128 << 10;

/// `specs` in order, cut into runs of at most [`PATHSPEC_BATCH_BYTES`]
/// (a longer spec gets a run of its own).
pub(crate) fn pathspec_batches(specs: Vec<OsString>) -> Vec<Vec<OsString>> {
    let mut batches: Vec<Vec<OsString>> = Vec::new();
    let mut bytes = 0;
    for spec in specs {
        let length = spec.len() + 1;
        match batches.last_mut() {
            Some(batch) if bytes + length <= PATHSPEC_BATCH_BYTES => {
                bytes += length;
                batch.push(spec);
            }
            _ => {
                bytes = length;
                batches.push(vec![spec]);
            }
        }
    }
    batches
}

/// `specs` NUL-terminated, for `--pathspec-from-file=- --pathspec-file-nul`.
pub(crate) fn nul_terminated(specs: &[OsString]) -> Vec<u8> {
    let mut input = Vec::new();
    for spec in specs {
        input.extend_from_slice(spec.as_bytes());
        input.push(0);
    }
    input
}
