use squad::eval_optimize::{run_optimize, OptimizeConfig};
use std::fs;

fn setup() -> (tempfile::TempDir, OptimizeConfig) {
    let t = tempfile::tempdir().unwrap();
    let harness = t.path().join("harness.md");
    fs::write(&harness, "zero\n").unwrap();
    let suite = t.path().join("suite.yaml");
    fs::write(&suite, "tasks:\n  - id: a\n    cmd: 'grep -q a \"$SQUAD_EVAL_HARNESS\"'\n  - id: b\n    cmd: 'grep -q b \"$SQUAD_EVAL_HARNESS\"'\n").unwrap();
    let cfg = OptimizeConfig {
        suite,
        harness,
        proposer_cmd: "echo a > \"$SQUAD_OPT_HARNESS_OUT\"".into(),
        composer_cmd: None,
        candidates: 1,
        rounds: 1,
        trials: Some(1),
        max_probes: None,
        out_dir: t.path().join("out"),
        stage_timeout_secs: 5,
    };
    (t, cfg)
}
#[test]
fn compose_and_preserve_source() {
    let (_t, mut c) = setup();
    c.candidates = 2;
    c.proposer_cmd = "if [ \"$SQUAD_OPT_CANDIDATE_INDEX\" = 1 ]; then echo a; else echo b; fi > \"$SQUAD_OPT_HARNESS_OUT\"".into();
    c.composer_cmd = Some("printf 'a\\nb\\n' > \"$SQUAD_OPT_HARNESS_OUT\"".into());
    let s = run_optimize(&c).unwrap();
    assert_eq!(s.winner_label, "r1-composed");
    assert_eq!(fs::read_to_string(&c.harness).unwrap(), "zero\n");
    assert!(fs::metadata(c.out_dir.join("summary.json")).is_ok());
    assert!(!fs::read(c.out_dir.join("winner.diff")).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(c.out_dir.join("winner/harness")).unwrap(),
        "a\nb\n"
    );
}
#[test]
fn gates_budget_and_keep_base() {
    for cmd in [
        ": > \"$SQUAD_OPT_HARNESS_OUT\"",
        "head -c 40000 /dev/zero > \"$SQUAD_OPT_HARNESS_OUT\"",
    ] {
        let (_t, mut c) = setup();
        c.proposer_cmd = cmd.into();
        let s = run_optimize(&c).unwrap();
        assert_eq!(s.probes_used, 1);
        assert!(s.candidates[1].rejected);
    }
    let (_t, mut c) = setup();
    c.max_probes = Some(1);
    let s = run_optimize(&c).unwrap();
    assert_eq!(s.probes_used, 1);
    assert!(s.kept_base);
    assert!(fs::read(c.out_dir.join("winner.diff")).unwrap().is_empty());
}
#[test]
fn carries_winner_and_prefers_small_harness() {
    let (_t, mut c) = setup();
    c.rounds = 2;
    c.proposer_cmd="if [ \"$SQUAD_OPT_ROUND\" = 1 ]; then printf 'a\\nlong\\n' > \"$SQUAD_OPT_HARNESS_OUT\"; else grep -q long \"$SQUAD_OPT_HARNESS_IN\" && echo a > \"$SQUAD_OPT_HARNESS_OUT\"; fi".into();
    let s = run_optimize(&c).unwrap();
    assert_eq!(s.winner_label, "r2-cand1");
}
#[cfg(unix)]
#[test]
fn directory_and_symlink() {
    let (t, mut c) = setup();
    let d = t.path().join("dir");
    fs::create_dir(&d).unwrap();
    fs::write(d.join("role.md"), "zero\n").unwrap();
    c.harness = d.clone();
    c.proposer_cmd = "echo a > \"$SQUAD_OPT_HARNESS_OUT/role.md\"".into();
    fs::write(
        &c.suite,
        "tasks:\n  - id: a\n    cmd: 'grep -q a \"$SQUAD_EVAL_HARNESS/role.md\"'\n",
    )
    .unwrap();
    assert!(run_optimize(&c).unwrap().improved);
    std::os::unix::fs::symlink("role.md", d.join("link")).unwrap();
    c.out_dir = t.path().join("out2");
    assert!(run_optimize(&c).is_err());
}

#[test]
fn regression_keeps_base() {
    let (_t, mut c) = setup();
    fs::write(&c.harness, "a\nb\n").unwrap();
    c.proposer_cmd = "echo a > \"$SQUAD_OPT_HARNESS_OUT\"".into();
    let result = run_optimize(&c).unwrap();
    assert!(result.candidates[1].regressed);
    assert!(result.kept_base);
    assert!(fs::read(c.out_dir.join("winner.diff")).unwrap().is_empty());
}

#[test]
fn overlapping_output_is_not_created() {
    let (t, mut c) = setup();
    let harness = t.path().join("roles");
    fs::create_dir(&harness).unwrap();
    fs::write(harness.join("role.md"), "zero\n").unwrap();
    c.harness = harness.clone();
    c.out_dir = harness.join("results");
    assert!(run_optimize(&c).is_err());
    assert!(!c.out_dir.exists());
}

#[test]
fn winner_diff_applies() {
    let (t, c) = setup();
    run_optimize(&c).unwrap();
    let diff = fs::read_to_string(c.out_dir.join("winner.diff")).unwrap();
    assert!(diff.contains("a/") && diff.contains("b/") && diff.contains("harness.md"));
    assert!(!diff.contains("base/harness") && !diff.contains("winner/harness"));
    let apply_dir = t.path().join("apply");
    fs::create_dir(&apply_dir).unwrap();
    fs::copy(&c.harness, apply_dir.join("harness.md")).unwrap();
    assert!(std::process::Command::new("git")
        .arg("apply")
        .arg(c.out_dir.join("winner.diff"))
        .current_dir(&apply_dir)
        .status()
        .unwrap()
        .success());
    assert_eq!(
        fs::read_to_string(apply_dir.join("harness.md")).unwrap(),
        "a\n"
    );
}

#[test]
fn resolved_output_does_not_create_intermediate_harness_directory() {
    let (t, mut c) = setup();
    let roles = t.path().join("roles");
    fs::create_dir(&roles).unwrap();
    fs::write(roles.join("role.md"), "zero\n").unwrap();
    c.harness = roles.clone();
    c.out_dir = roles.join("new/../../results");
    fs::write(
        &c.suite,
        "tasks:\n  - id: a\n    cmd: 'grep -q a \"$SQUAD_EVAL_HARNESS/role.md\"'\n",
    )
    .unwrap();
    c.proposer_cmd = "echo a > \"$SQUAD_OPT_HARNESS_OUT/role.md\"".into();
    run_optimize(&c).unwrap();
    assert!(t.path().join("results/summary.json").exists());
    assert!(!roles.join("new").exists());
}

#[test]
fn directory_diff_normalizes_added_empty_file() {
    let (t, mut c) = setup();
    let roles = t.path().join("roles");
    fs::create_dir(&roles).unwrap();
    fs::write(roles.join("role.md"), "zero\n").unwrap();
    c.harness = roles;
    fs::write(
        &c.suite,
        "tasks:\n  - id: a\n    cmd: 'grep -q a \"$SQUAD_EVAL_HARNESS/role.md\"'\n",
    )
    .unwrap();
    c.proposer_cmd =
        "echo a > \"$SQUAD_OPT_HARNESS_OUT/role.md\"; touch \"$SQUAD_OPT_HARNESS_OUT/new\"".into();
    assert!(run_optimize(&c).unwrap().improved);
    let diff = fs::read_to_string(c.out_dir.join("winner.diff")).unwrap();
    assert!(
        diff.contains("diff --git a/roles/new b/roles/new"),
        "{diff}"
    );
    assert!(
        !diff.contains("base/harness") && !diff.contains("winner/harness"),
        "{diff}"
    );
}

#[test]
fn winner_diff_preserves_crlf_and_applies() {
    let (t, mut c) = setup();
    fs::write(&c.harness, b"zero\r\n").unwrap();
    c.proposer_cmd = "printf 'a\\r\\n' > \"$SQUAD_OPT_HARNESS_OUT\"".into();
    run_optimize(&c).unwrap();
    let diff = fs::read(c.out_dir.join("winner.diff")).unwrap();
    assert!(
        diff.windows(7).any(|w| w == b"-zero\r\n"),
        "{:?}",
        String::from_utf8_lossy(&diff)
    );
    assert!(diff.windows(4).any(|w| w == b"+a\r\n"));
    let apply_dir = t.path().join("apply");
    fs::create_dir(&apply_dir).unwrap();
    fs::copy(&c.harness, apply_dir.join("harness.md")).unwrap();
    assert!(std::process::Command::new("git")
        .arg("apply")
        .arg(c.out_dir.join("winner.diff"))
        .current_dir(&apply_dir)
        .status()
        .unwrap()
        .success());
    assert_eq!(fs::read(apply_dir.join("harness.md")).unwrap(), b"a\r\n");
}

#[cfg(unix)]
#[test]
fn probe_cannot_corrupt_authoritative_harness() {
    let (_t, c) = setup();
    fs::write(
        &c.suite,
        "tasks:\n  - id: a\n    cmd: 'echo corrupted > \"$SQUAD_EVAL_HARNESS\"; true'\n",
    )
    .unwrap();
    let _ = run_optimize(&c);
    assert_eq!(
        fs::read_to_string(c.out_dir.join("base/harness")).unwrap(),
        "zero\n"
    );
}

#[cfg(unix)]
#[test]
fn background_processes_are_stopped() {
    let (t, mut c) = setup();
    let trial_marker = t.path().join("trial-leak");
    let stage_marker = t.path().join("stage-leak");
    fs::write(
        &c.suite,
        format!(
            "tasks:\n  - id: a\n    cmd: '(sleep 1; touch {}) & exit 0'\n",
            trial_marker.display()
        ),
    )
    .unwrap();
    c.proposer_cmd = format!("(sleep 1; touch {}) & exit 0", stage_marker.display());
    run_optimize(&c).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!trial_marker.exists());
    assert!(!stage_marker.exists());
}
