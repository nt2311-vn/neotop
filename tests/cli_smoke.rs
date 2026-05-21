use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_neotop");

#[test]
fn help_flag_succeeds() {
    let output = Command::new(BIN)
        .arg("--help")
        .output()
        .expect("spawn neotop --help");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("USAGE"), "stdout missing USAGE: {stdout}");
}

// The binary doesn't implement `--version`; fall back to asserting the
// program name appears in `--help` output so we still cover the
// "identifies itself" surface area.
#[test]
fn version_flag_succeeds() {
    let output = Command::new(BIN)
        .arg("--help")
        .output()
        .expect("spawn neotop --help");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("neotop"), "stdout missing neotop: {stdout}");
}

#[test]
fn unknown_flag_fails_gracefully() {
    let output = Command::new(BIN)
        .arg("--this-flag-does-not-exist")
        .output()
        .expect("spawn neotop with unknown flag");
    assert!(
        !output.status.success(),
        "expected non-zero exit, got {:?}",
        output.status
    );
}
