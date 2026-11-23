//! Fast-path internal CLI helper mode for `GIT_SEQUENCE_EDITOR`, `GIT_EDITOR`, and `GIT_ASKPASS`.
//!
//! When Branvio spawns `git rebase -i` in Tier 3, it sets `GIT_SEQUENCE_EDITOR="<self_exe> --branvio-git-sequence-editor"`
//! and `BRANVIO_REBASE_PLAN_FILE="<tmp_plan.json>"`. Git invokes our own binary with the todo file path;
//! this module intercepts those flags in `<2ms` before initializing any GPU/windowing stack, applies the
//! structured plan deterministically, and exits cleanly.

use git_engine::{RebaseAction, RebasePlanPayload};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Errors produced during headless Git CLI helper invocation.
#[derive(Debug, Error)]
pub enum CliHelperError {
    /// Missing expected file argument from Git.
    #[error("missing file path argument for {mode}")]
    MissingTargetArg {
        /// Helper flag invoked by Git.
        mode: &'static str,
    },

    /// Missing required environment variable linking the staged Branvio plan or token.
    #[error("missing required environment variable '{var}'")]
    MissingEnvVar {
        /// Environment variable name.
        var: &'static str,
    },

    /// Filesystem read/write failure on Git's todo or message file.
    #[error("I/O failure in helper mode: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to decode staged JSON rebase plan.
    #[error("invalid staged rebase plan JSON: {0}")]
    InvalidPlanJson(#[from] serde_json::Error),
}

/// Checks process arguments for internal Branvio Git helper modes.
///
/// Returns:
/// - `Ok(None)` if this is a normal GUI launch.
/// - `Ok(Some(0))` if an internal helper command ran and succeeded.
/// - `Err(err)` if helper mode failed (caller should print to stderr and exit non-zero).
pub fn try_handle_internal_git_helper(args: &[OsString]) -> Result<Option<i32>, CliHelperError> {
    let Some(flag) = args.get(1).and_then(|s| s.to_str()) else {
        return Ok(None);
    };

    match flag {
        "--branvio-git-sequence-editor" => {
            let todo_arg = args.get(2).ok_or(CliHelperError::MissingTargetArg {
                mode: "--branvio-git-sequence-editor",
            })?;
            let plan_path = std::env::var_os("BRANVIO_REBASE_PLAN_FILE").ok_or(
                CliHelperError::MissingEnvVar {
                    var: "BRANVIO_REBASE_PLAN_FILE",
                },
            )?;
            apply_rebase_sequence_file(Path::new(todo_arg), Path::new(&plan_path))?;
            Ok(Some(0))
        }
        "--branvio-git-editor" => {
            let msg_arg = args.get(2).ok_or(CliHelperError::MissingTargetArg {
                mode: "--branvio-git-editor",
            })?;
            if let Some(staged_msg_path) = std::env::var_os("BRANVIO_COMMIT_MSG_FILE") {
                let new_msg = fs::read_to_string(Path::new(&staged_msg_path))?;
                fs::write(Path::new(msg_arg), new_msg)?;
                Ok(Some(0))
            } else if let Some(plan_path) = std::env::var_os("BRANVIO_REBASE_PLAN_FILE") {
                apply_rebase_editor_message(Path::new(msg_arg), Path::new(&plan_path))?;
                Ok(Some(0))
            } else {
                Err(CliHelperError::MissingEnvVar {
                    var: "BRANVIO_COMMIT_MSG_FILE",
                })
            }
        }
        "--branvio-git-askpass" => {
            let token = std::env::var("BRANVIO_ASKPASS_TOKEN").map_err(|_| {
                CliHelperError::MissingEnvVar {
                    var: "BRANVIO_ASKPASS_TOKEN",
                }
            })?;
            println!("{token}");
            Ok(Some(0))
        }
        _ => Ok(None),
    }
}

/// Writes a structured [`RebasePlanPayload`] commit message override when Git invokes
/// `GIT_EDITOR` during `git rebase -i` (for `reword` or `squash` steps).
pub fn apply_rebase_editor_message(
    msg_path: &Path,
    plan_path: &Path,
) -> Result<(), CliHelperError> {
    let raw_plan = fs::read_to_string(plan_path)?;
    let plan: RebasePlanPayload = serde_json::from_str(&raw_plan)?;

    let Some(rebase_dir) = find_rebase_merge_dir(msg_path) else {
        return Ok(());
    };
    let Some(target_sha_lc) = resolve_active_rebase_sha(&rebase_dir)? else {
        return Ok(());
    };

    if let Some(entry) = plan.entries.iter().find(|entry| {
        let entry_hex = entry.oid.to_hex();
        entry_hex.starts_with(&target_sha_lc) || target_sha_lc.starts_with(&entry_hex)
    }) && let Some(updated) = &entry.updated_message
    {
        fs::write(msg_path, updated)?;
    }
    Ok(())
}

fn find_rebase_merge_dir(msg_path: &Path) -> Option<std::path::PathBuf> {
    let git_dir = msg_path.parent()?;
    let direct = git_dir.join("rebase-merge");
    if direct.is_dir() {
        return Some(direct);
    }
    None
}

fn resolve_active_rebase_sha(rebase_dir: &Path) -> Result<Option<String>, CliHelperError> {
    let stopped_path = rebase_dir.join("stopped-sha");
    if stopped_path.is_file() {
        let stopped_raw = fs::read_to_string(stopped_path)?;
        let stopped_lc = stopped_raw.trim().to_ascii_lowercase();
        if !stopped_lc.is_empty() {
            return Ok(Some(stopped_lc));
        }
    }

    // Git 2.x omits `rebase-merge/stopped-sha` during `reword` and `squash` editor invocations;
    // the active step is recorded as the last non-comment command line in `rebase-merge/done`.
    let done_path = rebase_dir.join("done");
    if !done_path.is_file() {
        return Ok(None);
    }
    let done_raw = fs::read_to_string(done_path)?;
    let last_line = done_raw.lines().rev().find(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    });
    let Some(line) = last_line else {
        return Ok(None);
    };
    let sha_token = line
        .split_whitespace()
        .skip(1)
        .take_while(|tok| !tok.starts_with('#'))
        .find(|tok| tok.len() >= 7 && tok.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(sha_token.map(str::to_ascii_lowercase))
}

/// Rewrites Git's `git-rebase-todo` file according to the structured [`RebasePlanPayload`].
///
/// Preserves existing short/full OIDs, trailing commit subjects, and Git's comment lines,
/// while reordering commits and changing actions (`pick`, `reword`, `edit`, `squash`, `fixup`, `drop`).
pub fn apply_rebase_sequence_file(
    todo_path: &Path,
    plan_path: &Path,
) -> Result<(), CliHelperError> {
    let raw_todo = fs::read_to_string(todo_path)?;
    let raw_plan = fs::read_to_string(plan_path)?;
    let plan: RebasePlanPayload = serde_json::from_str(&raw_plan)?;

    let rewritten = rewrite_rebase_todo_content(&raw_todo, &plan);
    fs::write(todo_path, rewritten)?;
    Ok(())
}

/// Pure function transforming raw `git-rebase-todo` text using a [`RebasePlanPayload`].
pub fn rewrite_rebase_todo_content(raw_todo: &str, plan: &RebasePlanPayload) -> String {
    let mut parsed_lines_by_oid: HashMap<String, (String, String)> = HashMap::new();
    let mut preserved_directives: Vec<&str> = Vec::new();

    for line in raw_todo.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Standard git-rebase-todo lines start with `<cmd> <sha> [subject...]`.
        let mut parts = trimmed.splitn(3, char::is_whitespace);
        let Some(cmd) = parts.next() else {
            continue;
        };
        let Some(sha) = parts.next() else {
            preserved_directives.push(trimmed);
            continue;
        };
        if matches!(
            cmd,
            "pick"
                | "p"
                | "reword"
                | "r"
                | "edit"
                | "e"
                | "squash"
                | "s"
                | "fixup"
                | "f"
                | "drop"
                | "d"
        ) {
            let rest = parts.next().unwrap_or("").trim().to_string();
            parsed_lines_by_oid.insert(sha.to_ascii_lowercase(), (sha.to_string(), rest));
        } else {
            preserved_directives.push(trimmed);
        }
    }

    let mut out = String::with_capacity(raw_todo.len() + 128);
    for entry in &plan.entries {
        let full_hex = entry.oid.to_hex();
        let matched = parsed_lines_by_oid.iter().find(|(sha_key, _)| {
            full_hex.starts_with(sha_key.as_str()) || sha_key.starts_with(full_hex.as_str())
        });

        let (emit_sha, subject) = match matched {
            Some((_, (orig_sha, subj))) => (orig_sha.as_str(), subj.as_str()),
            None => (full_hex.as_str(), ""),
        };

        if entry.action == RebaseAction::Drop {
            out.push_str(&format!("drop {emit_sha} {subject}\n"));
            continue;
        }

        out.push_str(entry.action.todo_keyword());
        out.push(' ');
        out.push_str(emit_sha);
        if !subject.is_empty() {
            out.push(' ');
            out.push_str(subject);
        }
        out.push('\n');
    }

    for directive in preserved_directives {
        out.push_str(directive);
        out.push('\n');
    }

    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use branvio_core::ids::GitOid;
    use compact_str::CompactString;
    use git_engine::RebasePlanEntry;

    #[test]
    fn rewrites_rebase_todo_reordered_with_squash_and_drop() {
        let oid_a = GitOid::from_hex("1111111111111111111111111111111111111111").unwrap();
        let oid_b = GitOid::from_hex("2222222222222222222222222222222222222222").unwrap();
        let oid_c = GitOid::from_hex("3333333333333333333333333333333333333333").unwrap();

        let original_todo = "\
pick 1111111 feat: initial parser
pick 2222222 fix: typo in parser
pick 3333333 debug: temp println
# Rebase 0000000..3333333 onto 0000000
";

        let plan = RebasePlanPayload {
            onto_rev: CompactString::const_new("main"),
            entries: vec![
                RebasePlanEntry {
                    oid: oid_a,
                    action: RebaseAction::Reword,
                    updated_message: Some("feat(parser): initial core".to_string()),
                },
                RebasePlanEntry {
                    oid: oid_c,
                    action: RebaseAction::Drop,
                    updated_message: None,
                },
                RebasePlanEntry {
                    oid: oid_b,
                    action: RebaseAction::Fixup,
                    updated_message: None,
                },
            ],
        };

        let rewritten = rewrite_rebase_todo_content(original_todo, &plan);
        assert_eq!(
            rewritten,
            "\
reword 1111111 feat: initial parser
drop 3333333 debug: temp println
fixup 2222222 fix: typo in parser
"
        );
    }

    #[test]
    fn applies_rebase_editor_message_for_stopped_sha() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        let rebase_dir = git_dir.join("rebase-merge");
        fs::create_dir_all(&rebase_dir).unwrap();

        let oid_a = GitOid::from_hex("1111111111111111111111111111111111111111").unwrap();
        let oid_b = GitOid::from_hex("2222222222222222222222222222222222222222").unwrap();
        let msg_path = git_dir.join("COMMIT_EDITMSG");
        fs::write(&msg_path, "original message\n").unwrap();

        let plan_path = dir.path().join("plan.json");
        let plan = RebasePlanPayload {
            onto_rev: CompactString::const_new("main"),
            entries: vec![
                RebasePlanEntry {
                    oid: oid_a,
                    action: RebaseAction::Reword,
                    updated_message: Some("reworded commit A\n".to_string()),
                },
                RebasePlanEntry {
                    oid: oid_b,
                    action: RebaseAction::Squash,
                    updated_message: None,
                },
            ],
        };
        fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        fs::write(
            rebase_dir.join("stopped-sha"),
            format!("{}\n", oid_a.to_hex()),
        )
        .unwrap();
        apply_rebase_editor_message(&msg_path, &plan_path).unwrap();
        assert_eq!(
            fs::read_to_string(&msg_path).unwrap(),
            "reworded commit A\n"
        );

        fs::write(&msg_path, "squash default\n").unwrap();
        fs::write(rebase_dir.join("stopped-sha"), "2222222\n").unwrap();
        apply_rebase_editor_message(&msg_path, &plan_path).unwrap();
        assert_eq!(fs::read_to_string(&msg_path).unwrap(), "squash default\n");
    }

    #[test]
    fn applies_rebase_editor_message_via_rebase_merge_done_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        let rebase_dir = git_dir.join("rebase-merge");
        fs::create_dir_all(&rebase_dir).unwrap();

        let oid_a = GitOid::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let oid_b = GitOid::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let msg_path = git_dir.join("COMMIT_EDITMSG");

        let plan_path = dir.path().join("plan.json");
        let plan = RebasePlanPayload {
            onto_rev: CompactString::const_new("main"),
            entries: vec![
                RebasePlanEntry {
                    oid: oid_a,
                    action: RebaseAction::Reword,
                    updated_message: Some("feat: reworded via done log\n".to_string()),
                },
                RebasePlanEntry {
                    oid: oid_b,
                    action: RebaseAction::Squash,
                    updated_message: Some("feat: combined squash message\n".to_string()),
                },
            ],
        };
        fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).unwrap();

        // Simulate Git 2.x `reword`: no `stopped-sha` file, `done` ends with `reword <sha>`.
        fs::write(&msg_path, "old subject A\n").unwrap();
        fs::write(
            rebase_dir.join("done"),
            format!("reword {} # old subject A\n# comment\n\n", oid_a.to_hex()),
        )
        .unwrap();
        apply_rebase_editor_message(&msg_path, &plan_path).unwrap();
        assert_eq!(
            fs::read_to_string(&msg_path).unwrap(),
            "feat: reworded via done log\n"
        );

        // Simulate Git 2.x `squash`: `done` ends with `squash <sha>`.
        fs::write(&msg_path, "# combination message\n").unwrap();
        fs::write(
            rebase_dir.join("done"),
            format!(
                "pick {} # old subject A\nsquash {} # old subject B\n",
                oid_a.to_hex(),
                oid_b.to_hex()
            ),
        )
        .unwrap();
        apply_rebase_editor_message(&msg_path, &plan_path).unwrap();
        assert_eq!(
            fs::read_to_string(&msg_path).unwrap(),
            "feat: combined squash message\n"
        );
    }
}
