//! The model relay's self-test (`scripts/model-relay-test.py`), run with the
//! workspace's tests so the gate judges the relay too. It needs `python3`
//! and loopback only: fake upstreams, no keys, no network.

use std::path::Path;
use std::process::Command;

#[test]
fn the_model_relay_passes_its_self_test() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/model-relay-test.py");
    let output = match Command::new("python3").arg("-I").arg(&script).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("python3 is not installed; the relay self-test is skipped");
            return;
        }
        Err(error) => panic!("run {}: {error}", script.display()),
    };
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
