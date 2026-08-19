use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SANDBOX: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);

impl Sandbox {
    fn new(label: &str) -> Self {
        let sequence = NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusty-dlna-cli-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create CLI sandbox");
        Self(path)
    }
}

impl AsRef<Path> for Sandbox {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_in(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rusty-dlna"))
        .args(args)
        .current_dir(sandbox)
        .env("RUSTY_DLNA_HTTP_PORT", "18200")
        .env("RUSTY_DLNA_SSDP_PORT", "11900")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run rusty-dlna CLI")
}

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

#[test]
fn help_and_invalid_arguments_are_actionable() {
    let sandbox = Sandbox::new("arguments");
    let help = run_in(&sandbox, &["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).expect("help is UTF-8");
    assert!(help.contains("Multithreaded DLNA server"));
    assert!(help.contains("--database-check"));

    let invalid = run_in(&sandbox, &["--not-a-real-option"]);
    assert!(!invalid.status.success());
    let invalid = String::from_utf8(invalid.stderr).expect("diagnostic is UTF-8");
    assert!(invalid.contains("unexpected argument '--not-a-real-option'"));
}

#[test]
fn non_serving_cli_modes_cover_startup_and_database_lifecycle() {
    let sandbox = Sandbox::new("modes");

    let effective = run_in(&sandbox, &["--print-effective-config"]);
    assert!(effective.status.success(), "{effective:?}");
    let effective = String::from_utf8(effective.stdout).expect("effective config is UTF-8");
    assert!(effective.contains("http_port = 18200"));
    assert!(effective.contains("ssdp_port = 11900"));
    assert!(effective.contains("files.db"));

    let check = run_in(&sandbox, &["--check"]);
    assert!(check.status.success(), "{check:?}");
    assert!(String::from_utf8(check.stdout)
        .expect("check output is UTF-8")
        .contains("rustyDLNA check OK"));

    let database = run_in(&sandbox, &["--database-check"]);
    assert!(database.status.success(), "{database:?}");
    assert!(String::from_utf8(database.stdout)
        .expect("database output is UTF-8")
        .contains("database OK:"));

    let rescan = run_in(&sandbox, &["--rescan"]);
    assert!(rescan.status.success(), "{rescan:?}");
    assert!(String::from_utf8(rescan.stdout)
        .expect("rescan output is UTF-8")
        .contains("rescan complete:"));

    let rebuild = run_in(&sandbox, &["--rebuild-database"]);
    assert!(rebuild.status.success(), "{rebuild:?}");
    assert!(String::from_utf8(rebuild.stdout)
        .expect("rebuild output is UTF-8")
        .contains("database rebuilt:"));
}

#[test]
fn config_path_errors_retain_validation_context() {
    let sandbox = Sandbox::new("invalid-config");
    let config = sandbox.as_ref().join("invalid.toml");
    std::fs::write(&config, "thumbnail_quality = 1\n").expect("write invalid config");
    let output = run_in(
        &sandbox,
        &["--config", config.to_str().expect("UTF-8 path"), "--check"],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("diagnostic is UTF-8");
    assert!(stderr.contains("thumbnail_quality must be between 2 and 31"));
}
