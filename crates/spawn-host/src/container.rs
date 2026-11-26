//! Container runtime detection and `bwrap` capability probing.
//!
//! Used by [`crate::engine_view`] and `spawn doctor` to distinguish a bare host
//! where `bwrap` is missing or misconfigured from a container environment where
//! nested user namespaces are blocked by the outer container runtime.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;

use crate::engine_view::{BwrapView, ViewConfig};

/// Result of inspecting whether the current process runs inside a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEnv {
    /// `true` when at least one container runtime signal was observed.
    pub in_container: bool,
    /// Best-effort container runtime name (`"docker"`, `"podman"`,
    /// `"containerd"`, `"kubernetes"`, `"lxc"`, etc.) when known.
    pub runtime: Option<String>,
    /// Human-readable evidence signals found during inspection.
    pub signals: Vec<String>,
}

/// Result of probing whether `bwrap` can create a sandboxed filesystem view
/// in the current environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BwrapCapability {
    /// `bwrap` works cleanly (`BwrapView::new` self-check passed).
    Ready,
    /// Running inside a container that blocks nested user/mount namespaces.
    BlockedInContainer(String),
    /// Unavailable for non-container reasons (not installed, misconfigured, etc.).
    Unavailable(String),
}

/// Detect whether the current process runs inside a container.
#[must_use]
pub fn detect_container() -> ContainerEnv {
    detect_container_with(Path::new("/"), &|name| std::env::var(name).ok())
}

/// Testable implementation of [`detect_container`] inspecting `root` and
/// environment variable lookup `env_lookup`.
#[must_use]
fn detect_container_with(root: &Path, env_lookup: &dyn Fn(&str) -> Option<String>) -> ContainerEnv {
    let mut signals = Vec::new();
    let mut runtime: Option<String> = None;

    if root.join(".dockerenv").exists() {
        signals.push("/.dockerenv".to_string());
        runtime.get_or_insert_with(|| "docker".to_string());
    }
    if root.join("run/.containerenv").exists() {
        signals.push("/run/.containerenv".to_string());
        runtime.get_or_insert_with(|| "podman".to_string());
    }
    if let Ok(cgroup) = std::fs::read_to_string(root.join("proc/1/cgroup")) {
        let lower = cgroup.to_ascii_lowercase();
        for (needle, name) in [
            ("docker", "docker"),
            ("containerd", "containerd"),
            ("kubepods", "kubernetes"),
            ("libpod", "podman"),
            ("lxc", "lxc"),
        ] {
            if lower.contains(needle) {
                let tag = format!("proc/1/cgroup:{needle}");
                if !signals.contains(&tag) {
                    signals.push(tag);
                }
                runtime.get_or_insert_with(|| name.to_string());
            }
        }
    }
    if let Some(val) = env_lookup("container") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            signals.push(format!("env:container={trimmed}"));
            runtime.get_or_insert_with(|| trimmed.to_string());
        }
    }

    ContainerEnv {
        in_container: !signals.is_empty(),
        runtime,
        signals,
    }
}

/// Whether `error` from `BwrapView::new` indicates that nested namespaces are
/// blocked by the outer container policy rather than a missing binary or bad config.
#[must_use]
pub fn is_container_namespace_block(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "operation not permitted",
        "eperm",
        "no permissions to create a new namespace",
        "no permissions to create new namespace",
        "setting up uid map",
        "writing to uid_map",
        "failed rtm_newaddr",
        "unprivileged_userns_clone",
        "permission denied",
        "creating new namespace failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Probe `bwrap` with `config` and classify container namespace blocks cleanly.
#[must_use]
pub fn probe_bwrap(program: &str, config: &ViewConfig) -> BwrapCapability {
    probe_bwrap_with(
        || BwrapView::new(program, config.clone()).map(|_| ()),
        &detect_container,
    )
}

/// Testable helper behind [`probe_bwrap`].
fn probe_bwrap_with(
    check: impl FnOnce() -> Result<(), String>,
    detect: &dyn Fn() -> ContainerEnv,
) -> BwrapCapability {
    match check() {
        Ok(()) => BwrapCapability::Ready,
        Err(error) => {
            let env = detect();
            if env.in_container && is_container_namespace_block(&error) {
                BwrapCapability::BlockedInContainer(error)
            } else {
                BwrapCapability::Unavailable(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{BwrapCapability, ContainerEnv, detect_container_with, probe_bwrap_with};

    #[test]
    fn detect_container_identifies_docker_podman_cgroup_and_env() {
        let scratch = crate::Scratch::new("spawn-container-detect");
        let bare = detect_container_with(&scratch, &|_| None);
        assert!(!bare.in_container);
        assert_eq!(bare.runtime, None);
        assert!(bare.signals.is_empty());

        let docker_root = scratch.join("docker");
        std::fs::create_dir_all(&docker_root).expect("docker dir");
        std::fs::write(docker_root.join(".dockerenv"), "").expect("dockerenv");
        let docker = detect_container_with(&docker_root, &|_| None);
        assert!(docker.in_container);
        assert_eq!(docker.runtime.as_deref(), Some("docker"));
        assert_eq!(docker.signals, vec!["/.dockerenv"]);

        let podman_root = scratch.join("podman");
        std::fs::create_dir_all(podman_root.join("run")).expect("podman run");
        std::fs::write(podman_root.join("run/.containerenv"), "engine=\"podman\"\n")
            .expect("containerenv");
        let podman = detect_container_with(&podman_root, &|_| None);
        assert!(podman.in_container);
        assert_eq!(podman.runtime.as_deref(), Some("podman"));
        assert_eq!(podman.signals, vec!["/run/.containerenv"]);

        let cg_root = scratch.join("cg");
        std::fs::create_dir_all(cg_root.join("proc/1")).expect("proc");
        std::fs::write(
            cg_root.join("proc/1/cgroup"),
            "0::/system.slice/containerd.service\n",
        )
        .expect("cgroup");
        let cg = detect_container_with(&cg_root, &|_| None);
        assert!(cg.in_container);
        assert_eq!(cg.runtime.as_deref(), Some("containerd"));
        assert!(cg.signals.iter().any(|s| s.contains("containerd")));

        let env_only = detect_container_with(&scratch, &|k| {
            (k == "container").then(|| "podman".to_string())
        });
        assert!(env_only.in_container);
        assert_eq!(env_only.runtime.as_deref(), Some("podman"));
        assert_eq!(env_only.signals, vec!["env:container=podman"]);
    }

    #[test]
    fn probe_bwrap_distinguishes_container_block_from_bare_host_failure() {
        let in_docker = || ContainerEnv {
            in_container: true,
            runtime: Some("docker".to_string()),
            signals: vec!["/.dockerenv".to_string()],
        };
        let bare = || ContainerEnv {
            in_container: false,
            runtime: None,
            signals: Vec::new(),
        };

        assert_eq!(probe_bwrap_with(|| Ok(()), &bare), BwrapCapability::Ready);
        let perm_err = "/usr/bin/bwrap does not work on this host (exit status: 1): bwrap: No permissions to create a new namespace";
        assert_eq!(
            probe_bwrap_with(|| Err(perm_err.to_string()), &in_docker),
            BwrapCapability::BlockedInContainer(perm_err.to_string())
        );
        assert_eq!(
            probe_bwrap_with(|| Err(perm_err.to_string()), &bare),
            BwrapCapability::Unavailable(perm_err.to_string())
        );
        let missing_err = "bwrap is not installed (not found on PATH)";
        assert_eq!(
            probe_bwrap_with(|| Err(missing_err.to_string()), &in_docker),
            BwrapCapability::Unavailable(missing_err.to_string())
        );
        let _ = PathBuf::from("/");
    }
}
