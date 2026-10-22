use std::process::Command;

#[test]
fn version_flag_prints_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_prime-radiant"))
        .arg("--version")
        .output()
        .expect("the built binary runs");

    assert!(output.status.success(), "--version exits 0");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert_eq!(
        stdout.trim(),
        format!("prime-radiant {}", env!("CARGO_PKG_VERSION")),
        "--version prints the binary name and crate version"
    );
}
