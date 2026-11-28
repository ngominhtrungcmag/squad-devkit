use std::process::Command;

#[test]
fn off_runs_and_invalid_profile_fails() {
    let root = std::env::temp_dir();
    let run = |profile: &str| {
        Command::new(env!("CARGO_BIN_EXE_squad"))
            .args([
                "sandbox",
                "exec",
                "--worktree",
                root.to_str().unwrap(),
                "--profile",
                profile,
                "--",
                env!("CARGO_BIN_EXE_squad"),
                "--help",
            ])
            .output()
            .unwrap()
            .status
    };
    assert!(run("off").success());
    assert!(!run("invalid").success());
}

#[cfg(unix)]
#[test]
fn isolation_smoke() {
    #[cfg(target_os = "linux")]
    if !Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("Skipping isolation smoke because bwrap is unavailable or unusable");
        return;
    }
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_squad"));
    let path = format!(
        "{}:{}",
        bin.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    assert!(Command::new("bash")
        .arg("tests/squad_sandbox_isolation_smoke.sh")
        .env("PATH", path)
        .status()
        .unwrap()
        .success());
}

#[test]
fn unavailable_or_disabled_backend_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let run = |options: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_squad"))
            .env("PATH", &bin)
            .env("HOME", root.path())
            .args([
                "sandbox",
                "exec",
                "--worktree",
                root.path().to_str().unwrap(),
            ])
            .args(options)
            .args(["--", env!("CARGO_BIN_EXE_squad"), "init", "--harness"])
            .output()
            .unwrap()
    };
    for options in [
        &["--profile", "strict"][..],
        &["--profile", "workspace", "--readonly"],
        &["--profile", "workspace", "--no-network"],
        &["--profile", "off", "--readonly"],
        &["--profile", "off", "--strict"],
        &["--profile", "off", "--no-network"],
        &["--profile", "off", "--no-network", "--network", "allow"],
        &["--profile", "off", "--network", "deny"],
    ] {
        let output = run(options);
        assert!(!output.status.success(), "{options:?}");
        assert!(
            !root.path().join(".squad/harness.toml").is_file(),
            "{options:?}"
        );
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("sandbox") && !error.contains("unknown sandbox option"),
            "{error}"
        );
    }
    let output = run(&["--profile", "workspace"]);
    assert!(output.status.success(), "{output:?}");
    assert!(root.path().join(".squad/harness.toml").is_file());
    let warning = String::from_utf8_lossy(&output.stderr);
    assert!(warning.contains("WARNING"));
    #[cfg(windows)]
    assert!(warning.contains("WSL2 with bwrap"));
}

#[cfg(unix)]
#[test]
fn ipc_uses_atomic_directory_exchange_without_workspace_database() {
    let dir = tempfile::tempdir().unwrap();
    let ipc = dir.path().join("ipc");
    std::fs::create_dir(&ipc).unwrap();
    let child = Command::new("bash")
        .arg("scripts/squad-ask.sh")
        .args([
            "--from",
            "fixture",
            "--workspace",
            "/does-not-exist",
            "--check",
            "--timeout",
            "3",
        ])
        .env("SQUAD_ASK_IPC_DIR", &ipc)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let request = std::fs::read_dir(&ipc)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".request.json"));
        if let Some(request) = request {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(request.path()).unwrap()).unwrap();
            assert_eq!(value["op"], "check");
            let response =
                serde_json::json!({"id": value["id"], "ok": true, "output": "fixture reply"});
            let pending = ipc.join("response.tmp");
            std::fs::write(&pending, response.to_string()).unwrap();
            std::fs::rename(
                pending,
                ipc.join(format!("{}.response.json", value["id"].as_str().unwrap())),
            )
            .unwrap();
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "fixture reply"
    );
}

#[test]
fn workspace_preserves_child_failure_without_rerun() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let (shell, flag, script) = ("cmd.exe", "/C", "echo once>>count & exit /b 7");
    #[cfg(not(windows))]
    let (shell, flag, script) = ("/bin/sh", "-c", "echo once >>count; exit 7");
    let output = Command::new(env!("CARGO_BIN_EXE_squad"))
        .args([
            "sandbox",
            "exec",
            "--worktree",
            root.path().to_str().unwrap(),
            "--profile",
            "workspace",
            "--",
            shell,
            flag,
            script,
        ])
        .status()
        .unwrap();
    assert_eq!(output.code(), Some(7));
    assert_eq!(
        std::fs::read_to_string(root.path().join("count"))
            .unwrap()
            .trim_end(),
        "once"
    );
}
