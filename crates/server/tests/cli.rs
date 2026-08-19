use std::process::Command;

#[test]
fn version_matches_the_cargo_package() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusty-dlna"))
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run rusty-dlna --version");
    assert!(output.status.success(), "status: {}", output.status);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).expect("version is UTF-8"),
        format!("rusty-dlna {}\n", env!("CARGO_PKG_VERSION"))
    );
}
