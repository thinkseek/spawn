use thiserror::Error;

use spawn_types::engine::Engine;

/// Errors in engine launch-parameter construction.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// A required launch input was empty.
    #[error("missing launch input: {0}")]
    MissingInput(&'static str),
}

/// Engine process parameters independent of the terminal implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineLaunch {
    /// Executable invoked inside the terminal.
    pub program: String,
    /// Arguments passed verbatim.
    pub arguments: Vec<String>,
    /// Environment variables required for hook routing or JSON streaming.
    pub environment: Vec<(String, String)>,
}

/// Build the launch parameters for an engine execution.
///
/// `hook_forwarder` is the Spawn CLI command that Claude hooks must invoke.
/// Codex streams JSON on standard output and therefore needs no hook config.
///
/// # Errors
/// Fails when a required parameter such as the working mount path is empty.
pub fn build_launch(
    engine: Engine,
    mount_path: &str,
    prompt: &str,
    resume_native_session: Option<&str>,
    hook_forwarder: &str,
) -> Result<EngineLaunch, LaunchError> {
    if mount_path.trim().is_empty() {
        return Err(LaunchError::MissingInput("mount_path"));
    }
    match engine {
        Engine::ClaudeCode => {
            let mut arguments = vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                mount_path.to_string(),
            ];
            if let Some(session_id) = resume_native_session {
                arguments.push("--resume".to_string());
                arguments.push(session_id.to_string());
            }
            if !prompt.is_empty() {
                arguments.push(prompt.to_string());
            }
            Ok(EngineLaunch {
                program: "claude".to_string(),
                arguments,
                environment: vec![(
                    "SPAWN_HOOK_FORWARDER".to_string(),
                    hook_forwarder.to_string(),
                )],
            })
        }
        Engine::Codex => {
            let mut arguments = vec![
                "exec".to_string(),
                "--json".to_string(),
                "--cd".to_string(),
                mount_path.to_string(),
            ];
            if let Some(session_id) = resume_native_session {
                arguments.push("--resume".to_string());
                arguments.push(session_id.to_string());
            }
            if !prompt.is_empty() {
                arguments.push(prompt.to_string());
            }
            Ok(EngineLaunch {
                program: "codex".to_string(),
                arguments,
                environment: Vec::new(),
            })
        }
    }
}
