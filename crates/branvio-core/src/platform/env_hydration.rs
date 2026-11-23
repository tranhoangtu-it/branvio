//! Cross-platform Git, SSH Agent, GPG, and PATH environment hydration.
//!
//! Never mutates the global process environment (`std::env::set_var` is unsafe in Rust 2024);
//! instead produces an immutable [`HydratedGitEnv`] map passed explicitly to subprocesses.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::process::Command;

/// Detected SSH authentication agent backing Git network and signing operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAgentKind {
    /// Standard Unix domain socket (`SSH_AUTH_SOCK` — OpenSSH, 1Password, Secretive, gpg-agent).
    UnixSocket {
        /// Path to the agent UNIX domain socket.
        socket_path: PathBuf,
    },
    /// Windows native OpenSSH Authentication Agent Named Pipe (`\\.\pipe\openssh-ssh-agent`).
    WindowsOpenSshPipe {
        /// Named pipe path.
        pipe_path: String,
    },
    /// PuTTY Pageant detected on Windows.
    WindowsPageant,
    /// No active SSH agent detected (will rely on SSH config / askpass / GCM).
    NoneDetected,
}

/// Per-invocation options configuring a child `git` CLI subprocess environment.
#[derive(Clone, Copy, Default)]
pub struct SubprocessEnvOptions<'a> {
    /// Sets `GIT_OPTIONAL_LOCKS=0` so read-only probes never create `.git/index.lock`.
    pub read_only_no_locks: bool,
    /// Forces `LC_ALL=C` and `LANG=C` for deterministic machine parsing of CLI porcelain/diagnostics.
    pub force_c_locale: bool,
    /// Path to current Branvio executable when enabling AskPass or Sequence Editor helpers.
    pub self_helper_exe: Option<&'a Path>,
    /// Ephemeral token passed to `--branvio-git-askpass`.
    pub askpass_token: Option<&'a str>,
    /// Path to staged JSON plan passed to `--branvio-git-sequence-editor`.
    pub sequence_plan_path: Option<&'a Path>,
}

impl std::fmt::Debug for SubprocessEnvOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubprocessEnvOptions")
            .field("read_only_no_locks", &self.read_only_no_locks)
            .field("force_c_locale", &self.force_c_locale)
            .field("self_helper_exe", &self.self_helper_exe)
            .field("askpass_token", &self.askpass_token.map(|_| "<redacted>"))
            .field("sequence_plan_path", &self.sequence_plan_path)
            .finish()
    }
}

/// Snapshot of resolved environment variables and tool paths for native `git` execution.
///
/// Built asynchronously once at startup (with timeout guard) and cached immutably.
#[derive(Debug, Clone)]
pub struct HydratedGitEnv {
    /// Complete key-value environment table applied to child `git` processes.
    vars: BTreeMap<OsString, OsString>,
    /// Detected SSH agent transport.
    pub ssh_agent: SshAgentKind,
    /// Absolute path to the resolved native `git` executable, if discovered.
    pub git_executable: Option<PathBuf>,
    /// Absolute path to the resolved `git-lfs` binary, if installed.
    pub git_lfs_executable: Option<PathBuf>,
    /// Whether environment variables were supplemented from a macOS/Unix login shell.
    pub hydrated_from_login_shell: bool,
}

impl HydratedGitEnv {
    /// Captures the current process environment without invoking a login shell.
    /// Useful as an instant zero-latency baseline during cold-start before background hydration completes.
    pub fn from_current_process() -> Self {
        let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        ensure_baseline_path(&mut vars);
        let ssh_agent = detect_ssh_agent(&vars);
        let git_executable = find_binary_in_path(&vars, git_binary_name());
        let git_lfs_executable = find_binary_in_path(&vars, git_lfs_binary_name());
        Self {
            vars,
            ssh_agent,
            git_executable,
            git_lfs_executable,
            hydrated_from_login_shell: false,
        }
    }

    /// Parses a NUL-separated (`env -0`) buffer produced by a login shell and merges
    /// critical developer environment variables into `vars`.
    pub fn merge_nul_separated_env(base: &mut BTreeMap<OsString, OsString>, raw_bytes: &[u8]) {
        const LOGIN_SHELL_KEYS: &[&str] = &[
            "PATH",
            "SSH_AUTH_SOCK",
            "SSH_AGENT_PID",
            "GPG_TTY",
            "GNUPGHOME",
            "GIT_CONFIG_GLOBAL",
            "GIT_EXEC_PATH",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "HOMEBREW_PREFIX",
            "LANG",
            "LC_ALL",
        ];

        for record in raw_bytes.split(|&b| b == 0) {
            if record.is_empty() {
                continue;
            }
            let Some(eq_idx) = record.iter().position(|&b| b == b'=') else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(&record[..eq_idx]) else {
                continue;
            };
            if !LOGIN_SHELL_KEYS.contains(&key) {
                continue;
            }
            let val = String::from_utf8_lossy(&record[eq_idx + 1..]);
            if !val.is_empty() {
                base.insert(OsString::from(key), OsString::from(val.as_ref()));
            }
        }
    }

    /// Produces a per-invocation environment map for running a native `git` subprocess.
    ///
    /// - Always injects `GIT_TERMINAL_PROMPT=0` so Git never hangs waiting on an invisible TTY.
    /// - Sets `GIT_OPTIONAL_LOCKS=0` on read-only probes so Branvio never creates `.git/index.lock`
    ///   conflicts against user commands running in external terminals.
    /// - Sets `LC_ALL=C` / `LANG=C` when [`SubprocessEnvOptions::force_c_locale`] is enabled.
    pub fn build_subprocess_env(
        &self,
        options: SubprocessEnvOptions<'_>,
    ) -> BTreeMap<OsString, OsString> {
        let mut env = self.vars.clone();
        env.insert(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0"));

        if options.read_only_no_locks {
            env.insert(OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0"));
        }

        if options.force_c_locale {
            env.insert(OsString::from("LC_ALL"), OsString::from("C"));
            env.insert(OsString::from("LANG"), OsString::from("C"));
        }

        if let Some(exe) = options.self_helper_exe {
            let exe_str = exe.to_string_lossy();
            if let Some(token) = options.askpass_token {
                let askpass_cmd = format!("\"{exe_str}\" --branvio-git-askpass");
                env.insert(OsString::from("GIT_ASKPASS"), OsString::from(&askpass_cmd));
                env.insert(OsString::from("SSH_ASKPASS"), OsString::from(&askpass_cmd));
                env.insert(
                    OsString::from("SSH_ASKPASS_REQUIRE"),
                    OsString::from("prefer"),
                );
                env.insert(
                    OsString::from("BRANVIO_ASKPASS_TOKEN"),
                    OsString::from(token),
                );
            }
            if let Some(plan_path) = options.sequence_plan_path {
                let seq_cmd = format!("\"{exe_str}\" --branvio-git-sequence-editor");
                let editor_cmd = format!("\"{exe_str}\" --branvio-git-editor");
                env.insert(
                    OsString::from("GIT_SEQUENCE_EDITOR"),
                    OsString::from(seq_cmd),
                );
                env.insert(OsString::from("GIT_EDITOR"), OsString::from(editor_cmd));
                env.insert(
                    OsString::from("BRANVIO_REBASE_PLAN_FILE"),
                    plan_path.as_os_str().to_os_string(),
                );
            }
        }

        env
    }

    /// Returns a read-only view of a hydrated environment variable.
    pub fn get(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.vars.get(key.as_ref()).map(OsString::as_os_str)
    }
}

/// Hydrates Git, SSH, GPG, and Homebrew/WSL PATH variables asynchronously off the UI thread.
///
/// On macOS/Unix GUI launches (e.g. launched via Finder/Dock rather than a terminal),
/// spawns the user's login shell (`$SHELL -lic "env -0"`) with a strict 800ms timeout
/// so 1Password SSH Agent, Secretive, `gpg-agent`, and Homebrew tools are discovered reliably
/// without delaying cold start.
pub async fn hydrate_git_environment() -> HydratedGitEnv {
    let mut base = HydratedGitEnv::from_current_process();

    #[cfg(unix)]
    {
        let needs_hydration = base.get("SSH_AUTH_SOCK").is_none() || base.git_executable.is_none();
        if needs_hydration {
            let shell = base
                .get("SHELL")
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| {
                    #[cfg(target_os = "macos")]
                    {
                        "/bin/zsh".to_string()
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        "/bin/sh".to_string()
                    }
                });

            let child_future = Command::new(&shell)
                .args(["-lic", "command env -0 || /usr/bin/env -0"])
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();

            if let Ok(Ok(output)) =
                tokio::time::timeout(Duration::from_millis(800), child_future).await
                && output.status.success()
            {
                HydratedGitEnv::merge_nul_separated_env(&mut base.vars, &output.stdout);
                base.hydrated_from_login_shell = true;
            }
        }
    }

    ensure_baseline_path(&mut base.vars);
    base.ssh_agent = detect_ssh_agent(&base.vars);
    base.git_executable = find_binary_in_path(&base.vars, git_binary_name());
    base.git_lfs_executable = find_binary_in_path(&base.vars, git_lfs_binary_name());
    base
}

fn ensure_baseline_path(vars: &mut BTreeMap<OsString, OsString>) {
    #[cfg(target_os = "macos")]
    {
        let current = vars
            .get(OsStr::new("PATH"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let extra = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
        let mut parts: Vec<String> = std::env::split_paths(&current)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        for dir in extra {
            if !parts.iter().any(|existing| existing == dir) {
                parts.push(dir.to_string());
            }
        }
        if let Ok(joined) = std::env::join_paths(parts) {
            vars.insert(OsString::from("PATH"), joined);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = vars;
    }
}

fn detect_ssh_agent(vars: &BTreeMap<OsString, OsString>) -> SshAgentKind {
    if let Some(sock) = vars.get(OsStr::new("SSH_AUTH_SOCK")) {
        let sock_str = sock.to_string_lossy();
        if !sock_str.is_empty() {
            #[cfg(windows)]
            if sock_str.starts_with(r"\\.\pipe\") {
                return SshAgentKind::WindowsOpenSshPipe {
                    pipe_path: sock_str.into_owned(),
                };
            }
            return SshAgentKind::UnixSocket {
                socket_path: PathBuf::from(sock),
            };
        }
    }

    #[cfg(windows)]
    {
        let pipe = r"\\.\pipe\openssh-ssh-agent";
        if std::path::Path::new(pipe).exists() {
            return SshAgentKind::WindowsOpenSshPipe {
                pipe_path: pipe.to_string(),
            };
        }
    }

    SshAgentKind::NoneDetected
}

#[inline]
const fn git_binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "git.exe"
    }
    #[cfg(not(windows))]
    {
        "git"
    }
}

#[inline]
const fn git_lfs_binary_name() -> &'static str {
    #[cfg(windows)]
    {
        "git-lfs.exe"
    }
    #[cfg(not(windows))]
    {
        "git-lfs"
    }
}

fn find_binary_in_path(vars: &BTreeMap<OsString, OsString>, binary: &str) -> Option<PathBuf> {
    let path_var = vars.get(OsStr::new("PATH"))?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_env_options_debug_redacts_askpass_token() {
        let opts = SubprocessEnvOptions {
            askpass_token: Some("super-secret-askpass-token"),
            ..SubprocessEnvOptions::default()
        };
        let dbg = format!("{opts:?}");
        assert!(!dbg.contains("super-secret-askpass-token"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn merges_nul_separated_login_env_safely() {
        let mut vars = BTreeMap::new();
        let payload = b"SSH_AUTH_SOCK=/tmp/1password.sock\0SECRET_TOKEN=ignored\0PATH=/opt/homebrew/bin:/usr/bin\0";
        HydratedGitEnv::merge_nul_separated_env(&mut vars, payload);

        assert_eq!(
            vars.get(OsStr::new("SSH_AUTH_SOCK")).unwrap(),
            "/tmp/1password.sock"
        );
        assert!(!vars.contains_key(OsStr::new("SECRET_TOKEN")));
        assert_eq!(
            vars.get(OsStr::new("PATH")).unwrap(),
            "/opt/homebrew/bin:/usr/bin"
        );
    }

    #[test]
    fn builds_non_locking_and_c_locale_subprocess_env() {
        let base = HydratedGitEnv::from_current_process();
        let env = base.build_subprocess_env(SubprocessEnvOptions {
            read_only_no_locks: true,
            force_c_locale: true,
            ..SubprocessEnvOptions::default()
        });
        assert_eq!(env.get(OsStr::new("GIT_TERMINAL_PROMPT")).unwrap(), "0");
        assert_eq!(env.get(OsStr::new("GIT_OPTIONAL_LOCKS")).unwrap(), "0");
        assert_eq!(env.get(OsStr::new("LC_ALL")).unwrap(), "C");
        assert_eq!(env.get(OsStr::new("LANG")).unwrap(), "C");
    }

    #[test]
    fn builds_rebase_sequence_and_commit_editor_env() {
        let base = HydratedGitEnv::from_current_process();
        let exe = Path::new("/usr/local/bin/branvio");
        let plan = Path::new("/tmp/rebase-plan.json");
        let env = base.build_subprocess_env(SubprocessEnvOptions {
            self_helper_exe: Some(exe),
            sequence_plan_path: Some(plan),
            ..SubprocessEnvOptions::default()
        });
        assert_eq!(
            env.get(OsStr::new("GIT_SEQUENCE_EDITOR")).unwrap(),
            "\"/usr/local/bin/branvio\" --branvio-git-sequence-editor"
        );
        assert_eq!(
            env.get(OsStr::new("GIT_EDITOR")).unwrap(),
            "\"/usr/local/bin/branvio\" --branvio-git-editor"
        );
        assert_eq!(
            env.get(OsStr::new("BRANVIO_REBASE_PLAN_FILE")).unwrap(),
            "/tmp/rebase-plan.json"
        );
    }
}
