use std::sync::{Arc, Mutex};

use spawn_process::{CommandLine, ProcessError, ProcessOutput, ProcessRunner};
use spawn_terminal::{LaunchSpec, TerminalError, TerminalHost, TmuxHost};
use spawn_types::identifiers::TerminalName;

#[derive(Default)]
struct ScriptedRunner {
    calls: Arc<Mutex<Vec<CommandLine>>>,
    next_status: Mutex<Option<i32>>,
    capture_text: String,
}

impl ScriptedRunner {
    #[allow(clippy::unnecessary_wraps)]
    fn run(&self, command: &CommandLine) -> Result<ProcessOutput, ProcessError> {
        self.calls.lock().unwrap().push(command.clone());
        let exit_code = self.next_status.lock().unwrap().take().unwrap_or(0);
        Ok(ProcessOutput {
            exit_code: Some(exit_code),
            stdout: self.capture_text.clone(),
            stderr: if exit_code == 0 {
                String::new()
            } else {
                "can't find session".to_string()
            },
        })
    }
}

fn spec(name: &str) -> LaunchSpec {
    LaunchSpec {
        name: TerminalName::new(name).unwrap(),
        program: "claude".to_string(),
        arguments: vec!["--resume".to_string(), "native-1".to_string()],
        working_directory: "/workspace/agent".to_string(),
        command_prefix: Vec::new(),
        environment: vec![("SPAWN".to_string(), "1".to_string())],
    }
}

#[test]
fn start_builds_detached_named_session() {
    let runner = Arc::new(ScriptedRunner::default());
    let host = TmuxHost::with_runner("tmux", ScriptedRunnerShim(runner.clone()));
    host.start(&spec("spawn-exec-1")).unwrap();

    let calls = runner.calls.lock().unwrap();
    let command = calls.first().expect("one tmux invocation");
    assert_eq!(command.program, "tmux");
    assert_eq!(command.arguments[0], "new-session");
    assert!(command.arguments.contains(&"-d".to_string()));
    assert!(command.arguments.contains(&"spawn-exec-1".to_string()));
    // The shell command changes into the mount and exports variables.
    let shell = command.arguments.last().unwrap();
    assert!(shell.contains("cd "));
    assert!(shell.contains("export SPAWN=1"));
    assert!(shell.contains("claude"));
}

#[test]
fn lifecycle_maps_tmux_subcommands() {
    let runner = Arc::new(ScriptedRunner {
        capture_text: "pane-line".to_string(),
        ..Default::default()
    });
    let host = TmuxHost::with_runner("tmux", ScriptedRunnerShim(runner.clone()));
    let name = TerminalName::new("spawn-exec-2").unwrap();

    assert!(host.is_alive(&name).unwrap());
    let capture = host.capture(&name).unwrap();
    assert_eq!(capture.text, "pane-line");
    host.send_keys(&name, "go", true).unwrap();
    host.terminate(&name).unwrap();

    let verbs: Vec<String> = runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.arguments[0].clone())
        .collect();
    assert_eq!(
        verbs,
        vec!["has-session", "capture-pane", "send-keys", "kill-session"]
    );
}

#[test]
fn non_zero_exit_becomes_rejected_error() {
    let runner = Arc::new(ScriptedRunner {
        next_status: Mutex::new(Some(1)),
        ..Default::default()
    });
    let host = TmuxHost::with_runner("tmux", ScriptedRunnerShim(runner));
    let result = host.terminate(&TerminalName::new("missing").unwrap());
    assert!(matches!(result, Err(TerminalError::Rejected(_))));
}

/// Thin wrapper so the shared Arc state can implement the runner trait.
struct ScriptedRunnerShim(Arc<ScriptedRunner>);

impl ProcessRunner for ScriptedRunnerShim {
    fn run(&self, command: &CommandLine) -> Result<ProcessOutput, ProcessError> {
        self.0.run(command)
    }
}
