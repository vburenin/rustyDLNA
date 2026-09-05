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

fn write_config(sandbox: &Sandbox, contents: &str) -> PathBuf {
    let path = sandbox.as_ref().join("rusty-dlna.toml");
    std::fs::write(&path, contents).expect("write CLI test configuration");
    path
}

fn seed_stale_cache_artifact(sandbox: &Sandbox) -> PathBuf {
    let cache = sandbox.as_ref().join("cache");
    std::fs::create_dir_all(&cache).expect("create test cache");
    let output = rusty_dlna_transcode::cache_dest_for_key(
        &cache,
        1,
        rusty_dlna_transcode::RecodeAction::Browser,
        &"a".repeat(64),
    );
    let part = rusty_dlna_transcode::cache_part(&output);
    std::fs::write(&part, b"diagnostics must not run cache maintenance")
        .expect("seed stale cache artifact");
    part
}

fn assert_diagnostic_storage_neutral(sandbox: &Sandbox, stale_part: &Path) {
    assert!(
        stale_part.is_file(),
        "diagnostic removed a stale cache artifact"
    );
    assert!(
        !sandbox.as_ref().join("database/files.db").exists(),
        "diagnostic created the catalog database"
    );
    assert!(
        !sandbox.as_ref().join("cache/uuid").exists(),
        "diagnostic created a persistent UUID"
    );
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
fn port_environment_overrides_are_strict_and_actionable() {
    let sandbox = Sandbox::new("invalid-port-environment");
    for (name, value) in [
        ("RUSTY_DLNA_HTTP_PORT", "0"),
        ("RUSTY_DLNA_HTTP_PORT", "not-a-port"),
        ("RUSTY_DLNA_HTTP_PORT", "65536"),
        ("RUSTY_DLNA_SSDP_PORT", "0"),
        ("RUSTY_DLNA_SSDP_PORT", "not-a-port"),
        ("RUSTY_DLNA_SSDP_PORT", "65536"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rusty-dlna"))
            .args(["--port", "18333", "--print-effective-config"])
            .current_dir(&sandbox)
            .env("RUSTY_DLNA_HTTP_PORT", "18200")
            .env("RUSTY_DLNA_SSDP_PORT", "11900")
            .env(name, value)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run rusty-dlna with invalid port environment override");
        assert!(!output.status.success(), "{name}={value} was accepted");
        let stderr = String::from_utf8(output.stderr).expect("diagnostic is UTF-8");
        assert!(stderr.contains(name), "unexpected diagnostic: {stderr}");
        assert!(
            stderr.contains("between 1 and 65535"),
            "unexpected diagnostic: {stderr}"
        );
    }
}

#[test]
fn non_utf8_port_environment_overrides_are_rejected() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let sandbox = Sandbox::new("non-utf8-port-environment");
    for name in ["RUSTY_DLNA_HTTP_PORT", "RUSTY_DLNA_SSDP_PORT"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rusty-dlna"))
            .arg("--print-effective-config")
            .current_dir(&sandbox)
            .env("RUSTY_DLNA_HTTP_PORT", "18200")
            .env("RUSTY_DLNA_SSDP_PORT", "11900")
            .env(name, OsString::from_vec(vec![b'8', 0xff]))
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run rusty-dlna with non-UTF port environment override");
        assert!(!output.status.success(), "{name} accepted non-UTF input");
        let stderr = String::from_utf8(output.stderr).expect("diagnostic is UTF-8");
        assert!(stderr.contains(name), "unexpected diagnostic: {stderr}");
        assert!(
            stderr.contains("between 1 and 65535"),
            "unexpected diagnostic: {stderr}"
        );
    }
}

#[test]
fn valid_port_environment_override_keeps_precedence_over_cli_port() {
    let sandbox = Sandbox::new("port-environment-precedence");
    let output = Command::new(env!("CARGO_BIN_EXE_rusty-dlna"))
        .args(["--port", "18333", "--print-effective-config"])
        .current_dir(&sandbox)
        .env("RUSTY_DLNA_HTTP_PORT", "18200")
        .env("RUSTY_DLNA_SSDP_PORT", "11900")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run rusty-dlna with port environment overrides");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("effective config is UTF-8");
    assert!(stdout.contains("http_port = 18200"), "{stdout}");
    assert!(stdout.contains("ssdp_port = 11900"), "{stdout}");
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

    let rescan = run_in(&sandbox, &["--rescan"]);
    assert!(rescan.status.success(), "{rescan:?}");
    assert!(String::from_utf8(rescan.stdout)
        .expect("rescan output is UTF-8")
        .contains("rescan complete:"));

    let database = run_in(&sandbox, &["--database-check"]);
    assert!(database.status.success(), "{database:?}");
    assert!(String::from_utf8(database.stdout)
        .expect("database output is UTF-8")
        .contains("database OK:"));

    let rebuild = run_in(&sandbox, &["--rebuild-database"]);
    assert!(rebuild.status.success(), "{rebuild:?}");
    assert!(String::from_utf8(rebuild.stdout)
        .expect("rebuild output is UTF-8")
        .contains("database rebuilt:"));
}

#[test]
fn diagnostic_modes_validate_without_starting_or_mutating_runtime_storage() {
    for mode in ["--print-effective-config", "--check"] {
        let sandbox = Sandbox::new(mode.trim_start_matches('-'));
        let config = write_config(
            &sandbox,
            r#"
friendly_name = "diagnostic-only"
cache_dir = "cache"
db_dir = "database"
rescan_secs = 0

[web]
enable = false

[[remap]]
name = "cli-remap-sentinel"
client = "Kodi"
hdr = "dv-p7"
action = "original"
"#,
        );
        let stale_part = seed_stale_cache_artifact(&sandbox);
        let output = run_in(
            &sandbox,
            &[
                "--config",
                config.to_str().expect("UTF-8 config path"),
                mode,
            ],
        );
        assert!(output.status.success(), "{mode}: {output:?}");
        assert_diagnostic_storage_neutral(&sandbox, &stale_part);

        let stdout = String::from_utf8(output.stdout).expect("diagnostic output is UTF-8");
        if mode == "--print-effective-config" {
            assert!(stdout.contains("cli-remap-sentinel"), "{stdout}");
            assert!(stdout.contains("Kodi"), "{stdout}");
            assert!(stdout.contains("DolbyVisionProfile7"), "{stdout}");
            assert!(stdout.contains("Original"), "{stdout}");
        } else {
            assert!(stdout.contains("rustyDLNA check OK"), "{stdout}");
            assert!(stdout.contains("remaps        1"), "{stdout}");
        }
    }
}

#[test]
fn combined_non_serving_modes_follow_documented_precedence() {
    let print_sandbox = Sandbox::new("print-mode-precedence");
    let print_config = write_config(
        &print_sandbox,
        "cache_dir = \"cache\"\ndb_dir = \"database\"\nrescan_secs = 0\n[web]\nenable = false\n",
    );
    let stale_part = seed_stale_cache_artifact(&print_sandbox);
    let printed = run_in(
        &print_sandbox,
        &[
            "--config",
            print_config.to_str().expect("UTF-8 config path"),
            "--print-effective-config",
            "--database-check",
            "--rebuild-database",
            "--rescan",
            "--check",
        ],
    );
    assert!(printed.status.success(), "{printed:?}");
    let stdout = String::from_utf8(printed.stdout).expect("effective config is UTF-8");
    assert!(stdout.contains("database = "), "{stdout}");
    assert!(!stdout.contains("database OK:"), "{stdout}");
    assert!(!stdout.contains("database rebuilt:"), "{stdout}");
    assert!(!stdout.contains("rescan complete:"), "{stdout}");
    assert!(!stdout.contains("rustyDLNA check OK"), "{stdout}");
    assert_diagnostic_storage_neutral(&print_sandbox, &stale_part);

    for (label, flags, expected) in [
        (
            "database-mode-precedence",
            vec![
                "--database-check",
                "--rebuild-database",
                "--rescan",
                "--check",
            ],
            "database OK:",
        ),
        (
            "rebuild-mode-precedence",
            vec!["--rebuild-database", "--rescan", "--check"],
            "database rebuilt:",
        ),
        (
            "rescan-mode-precedence",
            vec!["--rescan", "--check"],
            "rescan complete:",
        ),
    ] {
        let sandbox = Sandbox::new(label);
        let config = write_config(
            &sandbox,
            "cache_dir = \"cache\"\ndb_dir = \"database\"\nrescan_secs = 0\n[web]\nenable = false\n",
        );
        let mut args = vec!["--config", config.to_str().expect("UTF-8 config path")];
        args.extend(flags);
        let output = run_in(&sandbox, &args);
        assert!(output.status.success(), "{label}: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("mode output is UTF-8");
        assert!(stdout.contains(expected), "{label}: {stdout}");
        for lower_mode in [
            "database rebuilt:",
            "rescan complete:",
            "rustyDLNA check OK",
        ] {
            if lower_mode != expected {
                assert!(!stdout.contains(lower_mode), "{label}: {stdout}");
            }
        }
    }
}

#[test]
fn diagnostic_tool_and_persisted_identity_failures_remain_storage_neutral() {
    let tool_sandbox = Sandbox::new("tool-validation-neutral");
    let tool_config = write_config(
        &tool_sandbox,
        r#"
cache_dir = "cache"
db_dir = "database"
rescan_secs = 0

[web]
enable = false

[transcode]
enable = true
encoder = "libx264"

[[remap]]
name = "missing-diagnostic-encoder"
hdr = "dv-p7"
action = "hdr10"
encoder = "rusty_encoder_that_does_not_exist"
"#,
    );
    let stale_part = seed_stale_cache_artifact(&tool_sandbox);
    let failed = run_in(
        &tool_sandbox,
        &[
            "--config",
            tool_config.to_str().expect("UTF-8 config path"),
            "--check",
        ],
    );
    assert!(
        !failed.status.success(),
        "missing encoder unexpectedly passed"
    );
    let stderr = String::from_utf8(failed.stderr).expect("tool diagnostic is UTF-8");
    assert!(
        stderr.contains("rusty_encoder_that_does_not_exist")
            || stderr.contains("required executable \"ffmpeg\" is unavailable"),
        "{stderr}"
    );
    assert_diagnostic_storage_neutral(&tool_sandbox, &stale_part);

    let uuid_sandbox = Sandbox::new("persisted-uuid-neutral");
    let uuid_config = write_config(
        &uuid_sandbox,
        "cache_dir = \"cache\"\ndb_dir = \"database\"\nrescan_secs = 0\n[web]\nenable = false\n",
    );
    std::fs::create_dir_all(uuid_sandbox.as_ref().join("cache")).unwrap();
    std::fs::write(uuid_sandbox.as_ref().join("cache/uuid"), "not-a-uuid\n").unwrap();
    let failed = run_in(
        &uuid_sandbox,
        &[
            "--config",
            uuid_config.to_str().expect("UTF-8 config path"),
            "--check",
        ],
    );
    assert!(
        !failed.status.success(),
        "invalid persisted UUID was accepted"
    );
    let stderr = String::from_utf8(failed.stderr).expect("UUID diagnostic is UTF-8");
    assert!(stderr.contains("invalid persisted UUID"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(uuid_sandbox.as_ref().join("cache/uuid")).unwrap(),
        "not-a-uuid\n"
    );
    assert!(!uuid_sandbox.as_ref().join("database/files.db").exists());
}

#[test]
fn diagnostic_check_reuses_a_persisted_uuid_without_rewriting_it() {
    let sandbox = Sandbox::new("persisted-uuid-read-only");
    let config = write_config(
        &sandbox,
        "cache_dir = \"cache\"\ndb_dir = \"database\"\nrescan_secs = 0\n[web]\nenable = false\n",
    );
    let stale_part = seed_stale_cache_artifact(&sandbox);
    let persisted = b"  4D696E69-444C-164E-9D41-98B7852028D3  \n";
    std::fs::write(sandbox.as_ref().join("cache/uuid"), persisted).unwrap();

    let checked = run_in(
        &sandbox,
        &[
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--check",
        ],
    );
    assert!(checked.status.success(), "{checked:?}");
    let stdout = String::from_utf8(checked.stdout).expect("check output is UTF-8");
    assert!(
        stdout.contains("uuid:4d696e69-444c-164e-9d41-98b7852028d3"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(sandbox.as_ref().join("cache/uuid")).unwrap(),
        persisted
    );
    assert!(stale_part.is_file());
    assert!(!sandbox.as_ref().join("database/files.db").exists());
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

#[test]
fn rescan_skips_fifo_media_aliases_and_sidecars_without_hanging() {
    use rusty_dlna_helper::{
        CaptureConfig, CaptureRetention, SupervisedCommand, SupervisedOutcome,
    };
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    let sandbox = Sandbox::new("fifo-scan");
    let root = sandbox.as_ref().join("library");
    std::fs::create_dir(&root).unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/library/video/tagged.mp4");
    std::fs::copy(source, root.join("movie.mp4")).unwrap();
    for name in ["pipe", "movie.srt", "playlist.m3u"] {
        let path = std::ffi::CString::new(root.join(name).as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a live C string inside the owned temporary library.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    std::os::unix::fs::symlink("pipe", root.join("alias.mp4")).unwrap();
    let config = write_config(
        &sandbox,
        "media_dir = [\"library\"]\ncache_dir = \"cache\"\ndb_dir = \"database\"\nrescan_secs = 0\n",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusty-dlna"));
    command
        .args(["--config"])
        .arg(config)
        .arg("--rescan")
        .current_dir(&sandbox)
        .env("RUSTY_DLNA_HTTP_PORT", "18200")
        .env("RUSTY_DLNA_SSDP_PORT", "11900");
    let outcome = SupervisedCommand::new(&mut command)
        .capture_stdout(CaptureConfig::new(16 * 1024, CaptureRetention::Tail))
        .capture_stderr(CaptureConfig::new(16 * 1024, CaptureRetention::Tail))
        .run_until(
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(10),
            || std::ops::ControlFlow::<()>::Continue(()),
        )
        .unwrap();
    let SupervisedOutcome::Exited(output) = outcome else {
        panic!("scan hung on a FIFO: {outcome:?}");
    };
    assert!(output.status.success(), "scan failed: {output:?}");
    let db =
        rusty_dlna_scan::LibraryDb::open_read_only(&sandbox.as_ref().join("database/files.db"))
            .unwrap();
    assert!(db
        .find_detail_by_path(&rusty_dlna_scan::path_to_db(&root.join("movie.mp4")))
        .unwrap()
        .is_some());
    assert!(db
        .find_detail_by_path(&rusty_dlna_scan::path_to_db(&root.join("alias.mp4")))
        .unwrap()
        .is_none());
}
