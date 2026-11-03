use spawn_process::{CommandLine, ProcessOutput};

#[test]
fn builder_keeps_arguments_verbatim() {
    let command = CommandLine::new("layerfs")
        .arg("--json")
        .args(["branch", "fork"])
        .working_directory("/workspace")
        .env("SPAWN_MODE", "test");

    assert_eq!(command.program, "layerfs");
    assert_eq!(command.arguments, vec!["--json", "branch", "fork"]);
    assert_eq!(
        command.working_directory.as_deref(),
        Some(std::path::Path::new("/workspace"))
    );
    assert_eq!(
        command.environment.get("SPAWN_MODE").map(String::as_str),
        Some("test")
    );
}

#[test]
fn success_requires_zero_exit_code() {
    let ok = ProcessOutput {
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    };
    let failed = ProcessOutput {
        exit_code: Some(125),
        stdout: String::new(),
        stderr: "refused".to_string(),
    };
    let signaled = ProcessOutput {
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
    };
    assert!(ok.succeeded());
    assert!(!failed.succeeded());
    assert!(!signaled.succeeded());
}
