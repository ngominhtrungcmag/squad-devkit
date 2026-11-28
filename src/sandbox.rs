//! Native filesystem isolation for worker commands and gates.
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Default)]
struct Options {
    worktree: PathBuf,
    tmp: Option<PathBuf>,
    git: Option<PathBuf>,
    ro: Vec<PathBuf>,
    readonly: bool,
    network: bool,
    strict: bool,
}

fn canonical(value: &str) -> Result<PathBuf> {
    let path = Path::new(value)
        .canonicalize()
        .context("sandbox path must exist")?;
    path.to_str().context("sandbox paths must be UTF-8")?;
    if !path.is_dir() || path.parent().is_none() {
        bail!("sandbox paths must be directories other than root");
    }
    Ok(path)
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("path string")
}

fn requires_isolation(options: &Options) -> bool {
    options.strict || options.readonly || !options.network
}

fn backend(options: &Options, allow_network: bool) -> Option<Command> {
    if cfg!(target_os = "macos") {
        let mut policy = if options.strict {
            "(version 1)(deny default)(allow process*)(allow sysctl-read)(allow mach-lookup)(allow file-read* (literal \"/\"))"
                .to_string()
        } else {
            "(version 1)(allow default)(deny file-write*)".to_string()
        };
        if options.strict {
            for path in [
                "/bin",
                "/usr/bin",
                "/usr/lib",
                "/System/Library",
                "/Library/Developer/CommandLineTools",
                "/opt/homebrew",
                "/usr/local",
                "/dev/null",
                "/dev/urandom",
                "/dev/random",
                "/private/var/select/sh",
            ] {
                policy.push_str(&format!(
                    "(allow file-read* (subpath {}))",
                    quoted(Path::new(path))
                ));
            }
            for path in std::iter::once(&options.worktree)
                .chain(options.ro.iter())
                .chain(options.git.iter())
            {
                policy.push_str(&format!("(allow file-read* (subpath {}))", quoted(path)));
            }
        }
        policy.push_str("(allow file-write* (literal \"/dev/null\"))");
        if !options.readonly {
            policy.push_str(&format!(
                "(allow file-write* (subpath {}))",
                quoted(&options.worktree)
            ));
        }
        if let Some(tmp) = &options.tmp {
            policy.push_str(&format!(
                "(allow file-read* file-write* (subpath {}))",
                quoted(tmp)
            ));
        }
        if !options.strict {
            if let Some(git) = &options.git {
                policy.push_str(&format!("(allow file-write* (subpath {}))", quoted(git)));
            }
        }
        if !allow_network {
            policy.push_str("(deny network*)");
        } else if options.strict {
            policy.push_str("(allow network*)");
        }
        let mut cmd = Command::new("sandbox-exec");
        cmd.args(["-p", &policy]);
        Some(cmd)
    } else if cfg!(target_os = "linux") {
        let mut cmd = Command::new("bwrap");
        cmd.args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
        ]);
        if allow_network {
            cmd.arg("--share-net");
        }
        if options.strict {
            for path in ["/usr", "/bin", "/lib", "/lib64"] {
                if Path::new(path).exists() {
                    if let Ok(metadata) = std::fs::symlink_metadata(path) {
                        if metadata.file_type().is_symlink() {
                            if let Ok(target) = std::fs::read_link(path) {
                                cmd.args(["--symlink"]).arg(target).arg(path);
                                continue;
                            }
                        }
                    }
                    cmd.args(["--ro-bind", path, path]);
                }
            }
        } else {
            cmd.args(["--ro-bind", "/", "/"]);
        }
        cmd.args(["--proc", "/proc", "--dev", "/dev"]);
        for path in options.ro.iter().chain(options.git.iter()) {
            cmd.arg(if !options.strict && options.git.as_ref() == Some(path) {
                "--bind"
            } else {
                "--ro-bind"
            })
            .arg(path)
            .arg(path);
        }
        cmd.arg(if options.readonly {
            "--ro-bind"
        } else {
            "--bind"
        })
        .arg(&options.worktree)
        .arg(&options.worktree);
        if let Some(tmp) = &options.tmp {
            cmd.arg("--bind").arg(tmp).arg(tmp);
        }
        Some(cmd)
    } else {
        None
    }
}

pub fn command(args: &[String]) -> Result<i32> {
    if args.first().map(String::as_str) != Some("exec") {
        bail!(
            "Usage: squad sandbox exec --worktree DIR [--profile strict|workspace|off] [--readonly] [--network allow|deny] -- CMD..."
        );
    }
    let mut options = Options {
        strict: true,
        ..Options::default()
    };
    let mut off = false;
    let mut network = None;
    let mut no_network_requested = false;
    let mut strict_requested = false;
    let mut i = 1;
    while i < args.len() && args[i] != "--" {
        let flag = args[i].as_str();
        if flag == "--readonly" {
            options.readonly = true;
            i += 1;
            continue;
        }
        if flag == "--no-network" || flag == "--strict" {
            if flag == "--no-network" {
                no_network_requested = true;
            } else {
                strict_requested = true;
            }
            i += 1;
            continue;
        }
        let value = args.get(i + 1).context("missing sandbox option value")?;
        match flag {
            "--worktree" => options.worktree = canonical(value)?,
            "--run-tmp" => options.tmp = Some(canonical(value)?),
            "--git-common-dir" => options.git = Some(canonical(value)?),
            "--ro-path" => options.ro.push(canonical(value)?),
            "--profile" => match value.as_str() {
                "strict" => {
                    options.strict = true;
                    off = false;
                }
                "workspace" => {
                    options.strict = false;
                    off = false;
                }
                "off" => {
                    options.strict = false;
                    off = true;
                }
                _ => bail!("invalid sandbox profile"),
            },
            "--network" => match value.as_str() {
                "allow" => network = Some(true),
                "deny" => network = Some(false),
                _ => bail!("invalid sandbox network policy"),
            },
            _ => bail!("unknown sandbox option: {flag}"),
        }
        i += 2;
    }
    let child = args
        .get(i + 1..)
        .filter(|v| !v.is_empty())
        .context("sandbox requires -- CMD")?;
    if options.worktree.as_os_str().is_empty() {
        bail!("--worktree is required");
    }
    // Strict requires network isolation by default; workspace and off require it on request.
    options.strict |= strict_requested;
    if no_network_requested {
        network = Some(false);
    }
    options.network = network.unwrap_or(!options.strict);
    if off && requires_isolation(&options) {
        bail!("sandbox profile off cannot enforce --strict, --readonly, or --no-network");
    }
    if options.readonly {
        for path in options
            .tmp
            .iter()
            .chain(options.git.iter().filter(|_| !options.strict))
        {
            if path.starts_with(&options.worktree) || options.worktree.starts_with(path) {
                bail!("sandbox --readonly conflicts with a writable path overlapping the worktree");
            }
        }
    }
    let mut isolated = None;
    if !off {
        // Keep network denied by default whenever the backend is available.
        let allow_network = network.unwrap_or(false);
        let backend_error = if let Some(mut probe_command) = backend(&options, allow_network) {
            if preflight(&options, &mut probe_command)? {
                isolated = backend(&options, allow_network);
            }
            if cfg!(target_os = "macos") {
                "sandbox-exec missing or sandbox preflight failed"
            } else {
                "bwrap missing or sandbox preflight failed; install bubblewrap and enable user namespaces"
            }
        } else if cfg!(windows) {
            "OS-level sandboxing for --strict, --readonly, and --no-network is unavailable on native Windows; use WSL2 with bwrap"
        } else {
            "OS-level sandboxing is unavailable on this platform"
        };
        if isolated.is_none() {
            if requires_isolation(&options) {
                bail!("{backend_error}");
            }
            eprintln!("WARNING: {backend_error}; profile=workspace backend=none enforcement=none; running directly");
        }
    }
    let mut cmd = if let Some(mut cmd) = isolated {
        cmd.arg(&child[0]);
        cmd
    } else {
        #[cfg(windows)]
        let program = if matches!(child[0].as_str(), "bash" | "sh") {
            crate::platform::posix_shell(&[&child[0]]).unwrap_or_else(|| PathBuf::from(&child[0]))
        } else {
            PathBuf::from(&child[0])
        };
        #[cfg(not(windows))]
        let program = &child[0];
        Command::new(program)
    };
    cmd.args(&child[1..]).current_dir(&options.worktree);
    if let Some(tmp) = options.tmp {
        #[cfg(windows)]
        let tmp = crate::platform::shell_path(&tmp).into_owned();
        cmd.env("TMPDIR", tmp);
    }
    Ok(cmd
        .status()
        .context("execute sandbox child")?
        .code()
        .unwrap_or(1))
}

fn preflight(options: &Options, command: &mut Command) -> Result<bool> {
    // Probe before the actual child so a failing child is never rerun outside isolation.
    let Some(shell) = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
    else {
        return Ok(false);
    };
    let cat = ["/bin/cat", "/usr/bin/cat"]
        .into_iter()
        .find(|path| Path::new(path).is_file());
    if options.strict && cat.is_none() {
        return Ok(false);
    }
    let nonce = uuid::Uuid::new_v4();
    let probe = std::env::temp_dir().join(format!("squad-sandbox-{nonce}"));
    std::fs::create_dir(&probe).context("create sandbox preflight fixture")?;
    let worktree_probe = options.worktree.join(format!(".squad-preflight-{nonce}"));
    let result = (|| {
        let dir = probe.canonicalize()?;
        let secret = dir.join("secret");
        std::fs::write(&secret, b"preflight")?;
        Ok(command
            .args([
                shell,
                "-c",
                r#"
if [ "$1" = strict ] && "$2" "$3" >/dev/null 2>&1; then exit 1; fi
if (echo denied >"$4") 2>/dev/null; then exit 1; fi
if [ "$5" = readonly ] && (set -C; echo denied >"$6") 2>/dev/null; then exit 1; fi
printf sandbox-preflight-ok
"#,
                "sh",
                if options.strict {
                    "strict"
                } else {
                    "workspace"
                },
                cat.unwrap_or(""),
            ])
            .arg(secret)
            .arg(dir.join("outside"))
            .arg(if options.readonly {
                "readonly"
            } else {
                "write"
            })
            .arg(&worktree_probe)
            .current_dir(&options.worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|out| out.status.success() && out.stdout == b"sandbox-preflight-ok"))
    })();
    let _ = std::fs::remove_file(worktree_probe);
    let _ = std::fs::remove_dir_all(probe);
    result
}
