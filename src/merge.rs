//! Transactional working-tree merges; snapshots remain recoverable until verification succeeds.
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
};

#[derive(Serialize, Deserialize)]
struct Snapshot {
    state: String,
    repo: PathBuf,
    created_at: String,
    tracked_list: Vec<PathBuf>,
    untracked_list: Vec<PathBuf>,
    #[serde(default)]
    ignored_paths: Vec<PathBuf>,
    existing: Vec<PathBuf>,
    missing: Vec<PathBuf>,
}
fn git(repo: &Path, args: &[&str], index: Option<&Path>) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo).args(args);
    if let Some(index) = index {
        cmd.env(
            "GIT_INDEX_FILE",
            crate::platform::shell_path(index).as_ref(),
        );
    }
    Ok(cmd.output()?)
}
fn checked(repo: &Path, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>> {
    let out = git(repo, args, index)?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.stdout)
}
fn is_protected_repo_path(path: &Path) -> bool {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return false;
    };
    first.eq_ignore_ascii_case(".git")
        || (first.eq_ignore_ascii_case(".squad")
            && matches!(components.next(), Some(Component::Normal(second))
                if second.eq_ignore_ascii_case("merge-snapshots")))
}

fn validate_repo_relative_path(path: &Path) -> Result<()> {
    validate_repo_relative_path_for(path, cfg!(windows))
}

fn validate_repo_relative_path_for(path: &Path, windows_rules: bool) -> Result<()> {
    let raw = path.as_os_str().as_encoded_bytes();
    // Path::components normalizes interior "." components before iteration.
    if raw.is_empty()
        || raw.contains(&0)
        || raw
            .split(|&b| b == b'/' || b == b'\\')
            .any(|part| part == b"." || part == b"..")
    {
        bail!("unsafe repository path: {}", path.display());
    }
    // Git emits slash-separated paths on all platforms; reject ambiguous backslashes.
    if (raw.contains(&b'\\') && !cfg!(windows)) || is_protected_repo_path(path) {
        bail!("unsafe repository path: {}", path.display());
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            bail!("unsafe repository path: {}", path.display());
        };
        if !windows_rules {
            continue;
        }
        let name = name.to_string_lossy();
        let base = name
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ');
        let reserved = ["CON", "PRN", "AUX", "NUL"]
            .iter()
            .any(|device| base.eq_ignore_ascii_case(device))
            || (base.len() == 4
                && matches!(base.as_bytes()[3], b'1'..=b'9')
                && (base.as_bytes()[..3].eq_ignore_ascii_case(b"COM")
                    || base.as_bytes()[..3].eq_ignore_ascii_case(b"LPT")));
        if name.contains(':') || name.ends_with(['.', ' ']) || reserved {
            bail!("unsafe repository path: {}", path.display());
        }
    }
    Ok(())
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.is_symlink()
}

fn ensure_no_symlink_ancestors(repo: &Path, rel: &Path) -> Result<()> {
    let mut path = repo.to_path_buf();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(m) if is_link_or_reparse(&m) => {
                    bail!("unsafe repository path ancestor: {}", path.display());
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn paths(repo: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let paths = repo_relative_paths(repo, args)?;
    for path in &paths {
        ensure_no_symlink_ancestors(repo, path)?;
    }
    Ok(paths)
}

fn repo_relative_paths(repo: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    let output = checked(repo, args, None)?;
    let mut paths = Vec::new();
    for entry in output.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        #[cfg(unix)]
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(entry));
        #[cfg(not(unix))]
        let path = PathBuf::from(
            std::str::from_utf8(entry).context("non-UTF-8 git path on this platform")?,
        );
        if is_protected_repo_path(&path) {
            continue;
        }
        validate_repo_relative_path(&path)?;
        paths.push(path);
    }
    Ok(paths)
}
fn all_paths(repo: &Path) -> Result<BTreeSet<PathBuf>> {
    Ok(repo_relative_paths(
        repo,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?
    .into_iter()
    .collect())
}
fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        #[cfg(windows)]
        Ok(m)
            if {
                use std::os::windows::fs::FileTypeExt;
                (m.is_dir() || m.file_type().is_symlink_dir()) && is_link_or_reparse(&m)
            } =>
        {
            fs::remove_dir(path)?
        }
        Ok(m) if m.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn copy(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(source)?;
    if meta.is_symlink() {
        let target = fs::read_link(source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dest)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTypeExt;
            let directory = fs::metadata(source).map_or_else(
                |_| meta.file_type().is_symlink_dir(),
                |target| target.is_dir(),
            );
            let result = if directory {
                std::os::windows::fs::symlink_dir(target, dest)
            } else {
                std::os::windows::fs::symlink_file(target, dest)
            };
            result.context(
                "recreate snapshot symlink (Windows requires Developer Mode or symlink privileges)",
            )?;
        }
        #[cfg(not(any(unix, windows)))]
        bail!("cannot copy symlink on this platform: {}", target.display());
    } else if meta.is_file() && !is_link_or_reparse(&meta) {
        fs::copy(source, dest)?;
    } else {
        bail!("unsupported snapshot entry: {}", source.display());
    }
    Ok(())
}
fn index_path(repo: &Path) -> Result<PathBuf> {
    let bytes = checked(repo, &["rev-parse", "--git-path", "index"], None)?;
    let path = PathBuf::from(String::from_utf8(bytes)?.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}
fn snapshot_root(repo: &Path) -> Result<PathBuf> {
    let out = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
    )?;
    // Older Git may echo the unsupported option instead of failing.
    let bytes = if out.status.success() && !out.stdout.starts_with(b"--path-format=") {
        out.stdout
    } else {
        checked(repo, &["rev-parse", "--git-common-dir"], None)?
    };
    let path = PathBuf::from(String::from_utf8(bytes)?.trim());
    let common = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    Ok(common.canonicalize()?.join("squad-merge-snapshots"))
}
fn restore(dir: &Path, s: &Snapshot) -> Result<()> {
    let manifest_paths = || {
        s.tracked_list
            .iter()
            .chain(&s.untracked_list)
            .chain(&s.ignored_paths)
            .chain(&s.existing)
            .chain(&s.missing)
    };
    for path in manifest_paths() {
        validate_repo_relative_path(path).context("unsafe snapshot path")?;
    }
    let files = dir.join("files");
    let existing: BTreeSet<_> = s.existing.iter().cloned().collect();
    let ignored_overlap = |path: &Path| {
        s.ignored_paths
            .iter()
            .any(|p| p.starts_with(path) || path.starts_with(p))
    };
    for path in s
        .tracked_list
        .iter()
        .chain(&s.untracked_list)
        .chain(&s.missing)
    {
        anyhow::ensure!(existing.contains(path), "inconsistent snapshot path");
        if !s.missing.contains(path) {
            ensure_no_symlink_ancestors(dir, &Path::new("files").join(path))?;
        }
    }

    let remove_added = |path: &Path| -> Result<()> {
        remove(&s.repo.join(path))?;
        let mut parent = path.parent();
        while let Some(dir) = parent.filter(|p| !p.as_os_str().is_empty()) {
            if existing
                .iter()
                .chain(&s.ignored_paths)
                .any(|p| p.starts_with(dir))
            {
                break;
            }
            ensure_no_symlink_ancestors(&s.repo, dir)?;
            match fs::remove_dir(s.repo.join(dir)) {
                Ok(()) => (),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e.into()),
            }
            parent = dir.parent();
        }
        Ok(())
    };

    // The live index can still list children underneath a link added by the patch.
    for path in all_paths(&s.repo)?.difference(&existing) {
        if ignored_overlap(path) {
            continue;
        }
        if ensure_no_symlink_ancestors(&s.repo, path).is_ok() {
            let dest = s.repo.join(path);
            match fs::symlink_metadata(&dest) {
                Ok(metadata) if is_link_or_reparse(&metadata) => remove_added(path)?,
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    for path in &s.missing {
        ensure_no_symlink_ancestors(&s.repo, path)?;
        remove(&s.repo.join(path))?;
    }
    for path in manifest_paths() {
        ensure_no_symlink_ancestors(&s.repo, path)?;
    }
    for path in s.tracked_list.iter().chain(&s.untracked_list) {
        if !s.missing.contains(path) {
            ensure_no_symlink_ancestors(&s.repo, path)?;
            let dest = s.repo.join(path);
            remove(&dest)?;
            copy(&files.join(path), &dest)?;
        }
    }
    let index = index_path(&s.repo)?;
    if dir.join("no_index").exists() {
        remove(&index)?;
    } else {
        fs::copy(dir.join("index"), index)?;
    }
    let current = all_paths(&s.repo)?;
    for path in current.difference(&existing) {
        if ignored_overlap(path) {
            continue;
        }
        ensure_no_symlink_ancestors(&s.repo, path)?;
        remove_added(path)?;
    }
    Ok(())
}
fn recover(root: &Path, repo: &Path, prefix: &str) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let dir = entry?.path();
        if !entry_name_matches(&dir, prefix) {
            continue;
        }
        let metadata = fs::symlink_metadata(&dir)?;
        if is_link_or_reparse(&metadata) {
            bail!("unsafe snapshot directory: {}", dir.display());
        }
        if !metadata.is_dir() {
            continue;
        }
        if !dir.join("manifest.json").exists() {
            fs::remove_dir_all(dir)?;
            continue;
        }
        let s: Snapshot = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
        if s.repo == repo {
            if s.state == "in_progress" {
                restore(&dir, &s).context("snapshot recovery failed; snapshot retained")?;
            }
            fs::remove_dir_all(dir)?;
        }
    }
    Ok(())
}
fn entry_name_matches(path: &Path, prefix: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(prefix))
}
fn report(json: bool, state: &str, detail: serde_json::Value) {
    if json {
        println!(
            "{}",
            serde_json::json!({"merge_state":state,"details":detail})
        );
    } else {
        println!("merge_state={state}\n{detail}");
    }
}
fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(8192)..]).into_owned()
}

pub fn command(args: &[String]) -> Result<i32> {
    if args == ["--help"] || args.is_empty() {
        println!("squad merge apply --repo REPO --patch PATCH [--workspace WS] [--from-harness] [--post-merge-gate CMD]... [--json]\nsquad merge recover --repo REPO [--workspace WS] [--json]");
        return Ok(0);
    }
    let action = &args[0];
    if action != "apply" && action != "recover" {
        bail!("unknown merge action: {action}");
    }
    let (mut repo, mut patch) = (None, None);
    let (mut json, mut harness) = (false, false);
    let mut gates = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--from-harness" => harness = true,
            flag @ ("--repo" | "--patch" | "--workspace" | "--post-merge-gate") => {
                i += 1;
                let value = args
                    .get(i)
                    .with_context(|| format!("{flag} requires a value"))?
                    .clone();
                match flag {
                    "--repo" => repo = Some(PathBuf::from(value)),
                    "--patch" => patch = Some(PathBuf::from(value)),
                    "--workspace" => (), // Accepted for CLI compatibility; coordination is repository-scoped.
                    _ => gates.push(value),
                }
            }
            flag => bail!("unknown merge flag: {flag}"),
        }
        i += 1;
    }
    let repo = repo.context("--repo required")?.canonicalize()?;
    let top = checked(&repo, &["rev-parse", "--show-toplevel"], None)?;
    if Path::new(String::from_utf8(top)?.trim()).canonicalize()? != repo {
        bail!("--repo must be the repository root");
    }
    let patch = if action == "apply" {
        Some(patch.context("--patch required")?.canonicalize()?)
    } else {
        None
    };
    if harness {
        if let Some(config) = crate::harness_config::load_harness_config(&repo)? {
            gates.splice(0..0, config.gates.post_merge);
        }
    }
    let root = snapshot_root(&repo)?;
    if let Ok(meta) = fs::symlink_metadata(&root) {
        if is_link_or_reparse(&meta) {
            bail!("unsafe snapshot directory: {}", root.display());
        }
    }
    fs::create_dir_all(&root)?;
    // Stable FNV-1a groups snapshots by target worktree within the common directory.
    let hash = repo
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("merge.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            report(json, "locked", serde_json::json!({}));
            return Ok(4);
        }
        Err(e) => return Err(e.into()),
    }
    let prefix = format!("{hash:x}-");
    recover(&root, &repo, &prefix)?;
    let Some(patch) = patch else {
        report(json, "recovered", serde_json::json!({}));
        return Ok(0);
    };
    let dir = root.join(format!("{prefix}{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir)?;
    let mut s = Snapshot {
        state: "in_progress".into(),
        repo: repo.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        tracked_list: paths(&repo, &["ls-files", "-z", "--cached"])?,
        untracked_list: paths(&repo, &["ls-files", "-z", "--others", "--exclude-standard"])?,
        ignored_paths: paths(
            &repo,
            &[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
            ],
        )?,
        existing: all_paths(&repo)?.into_iter().collect(),
        missing: Vec::new(),
    };
    s.existing.extend(paths(
        &repo,
        &[
            "ls-files",
            "-z",
            "--others",
            "--directory",
            "--exclude-standard",
        ],
    )?);
    s.tracked_list.sort();
    s.tracked_list.dedup();
    let index = index_path(&repo)?;
    if index.exists() {
        fs::copy(&index, dir.join("index"))?;
    } else {
        fs::write(dir.join("no_index"), "")?;
    }
    for path in s.tracked_list.iter().chain(&s.untracked_list) {
        ensure_no_symlink_ancestors(&repo, path)?;
        ensure_no_symlink_ancestors(&dir, &Path::new("files").join(path))?;
        match fs::symlink_metadata(repo.join(path)) {
            Ok(_) => copy(&repo.join(path), &dir.join("files").join(path))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => s.missing.push(path.clone()),
            Err(e) => return Err(e.into()),
        }
    }
    fs::write(dir.join("manifest.tmp"), serde_json::to_vec(&s)?)?;
    fs::rename(dir.join("manifest.tmp"), dir.join("manifest.json"))?;
    let result = (|| -> Result<(i32, &'static str, serde_json::Value)> {
        let temp_index = dir.join("apply.index");
        if index.exists() {
            fs::copy(&index, &temp_index)?;
        }
        let stage_paths = paths(
            &repo,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let mut stage = Command::new("git")
            .current_dir(&repo)
            .env(
                "GIT_INDEX_FILE",
                crate::platform::shell_path(&temp_index).as_ref(),
            )
            .args(["update-index", "--add", "--remove", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        {
            let mut input = stage.stdin.take().context("missing git stdin")?;
            for path in stage_paths {
                input.write_all(path.as_os_str().as_encoded_bytes())?;
                input.write_all(&[0])?;
            }
        }
        let staged = stage.wait_with_output()?;
        if !staged.status.success() {
            bail!("temporary index update failed: {}", tail(&staged.stderr));
        }
        patch.to_str().context("patch path must be UTF-8")?;
        let patch = crate::platform::shell_path(&patch);
        let out = git(&repo, &["apply", "--3way", &patch], Some(&temp_index))?;
        if !out.status.success() {
            let conflicts = paths_from_index(&repo, &temp_index)?;
            return Ok((
                1,
                "rolled_back_conflict",
                serde_json::json!({"conflicts":conflicts,"output_tail":tail(&out.stderr)}),
            ));
        }
        for gate in &gates {
            let log_path = dir.join("gate.log");
            let log = fs::File::create(&log_path)?;
            let status = crate::platform::bash_command(gate)
                .current_dir(&repo)
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .status()?;
            if !status.success() {
                let mut log = fs::File::open(log_path)?;
                let len = log.metadata()?.len();
                log.seek(SeekFrom::Start(len.saturating_sub(8192)))?;
                let mut output = Vec::new();
                log.take(8192).read_to_end(&mut output)?;
                return Ok((
                    1,
                    "rolled_back_verify_fail",
                    serde_json::json!({"gate":gate,"exit_code":status.code(),"output_tail":tail(&output)}),
                ));
            }
        }
        Ok((0, "merged", serde_json::json!({})))
    })();
    if !matches!(&result, Ok((0, _, _))) {
        restore(&dir, &s).context("rollback failed; snapshot retained for recovery")?;
    }
    // Mark committed before cleanup so a cleanup error cannot cause later rollback.
    s.state = "complete".into();
    fs::write(dir.join("manifest.tmp"), serde_json::to_vec(&s)?)?;
    fs::rename(dir.join("manifest.tmp"), dir.join("manifest.json"))?;
    fs::remove_dir_all(&dir)?;
    let (code, state, detail) = result?;
    report(json, state, detail);
    Ok(code)
}
fn paths_from_index(repo: &Path, index: &Path) -> Result<Vec<String>> {
    let out = checked(
        repo,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
        Some(index),
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_repo_paths() {
        for path in [
            "",
            ".",
            "./file",
            "a/./b",
            "..",
            "a/../b",
            "/file",
            "C:/file",
            ".GIT/config",
            ".Squad/Merge-Snapshots/state",
            "foo:bar",
            "aux.log",
            "CON",
            "PRN.data",
            "nul",
            "Com9.json",
            "lPt1",
            "file.",
            "file ",
        ] {
            assert!(
                validate_repo_relative_path_for(Path::new(path), true).is_err(),
                "{path}"
            );
        }
        assert!(validate_repo_relative_path(Path::new(".squad/workers/status")).is_ok());
        assert!(validate_repo_relative_path(Path::new("dir/COM10.txt")).is_ok());
    }

    #[test]
    fn accepts_unix_filenames() {
        for path in ["notes:2026.md", "aux.log"] {
            assert!(
                validate_repo_relative_path_for(Path::new(path), false).is_ok(),
                "{path}"
            );
        }
        for path in ["..", ".GIT/config", ".Squad/Merge-Snapshots/x"] {
            assert!(
                validate_repo_relative_path_for(Path::new(path), false).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn rejects_link_ancestors_from_git_and_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&outside).unwrap();
        checked(&repo, &["init"], None).unwrap();
        fs::create_dir(repo.join("dir")).unwrap();
        fs::write(repo.join("dir/file"), "tracked").unwrap();
        checked(&repo, &["add", "dir/file"], None).unwrap();
        fs::remove_dir_all(repo.join("dir")).unwrap();
        fs::write(outside.join("file"), "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();
        #[cfg(windows)]
        assert!(Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(repo.join("dir"))
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success());

        let path = PathBuf::from("dir/file");
        assert!(paths(&repo, &["ls-files", "-z", "--cached"]).is_err());
        let snapshot = Snapshot {
            state: "in_progress".into(),
            repo,
            created_at: String::new(),
            tracked_list: vec![path.clone()],
            untracked_list: vec![],
            ignored_paths: vec![],
            existing: vec![PathBuf::from("dir"), path.clone()],
            missing: vec![path],
        };
        let error = restore(temp.path(), &snapshot).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsafe repository path ancestor"));
        let mut snapshot = snapshot;
        snapshot.missing.clear();
        for path in [".GIT/config", ".squad/merge-snapshots/leftover"] {
            snapshot.tracked_list = vec![path.into()];
            let error = restore(temp.path(), &snapshot).unwrap_err();
            assert_eq!(error.to_string(), "unsafe snapshot path");
        }
        assert_eq!(fs::read_to_string(outside.join("file")).unwrap(), "outside");
    }
}
