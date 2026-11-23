//! Detection of destructive `git` invocations typed into the embedded terminal's command box,
//! so the presenter can ask for confirmation before forwarding them to the shell.
//!
//! This is a safety net for the command-line input only: keystrokes typed directly into the PTY
//! canvas go straight to the shell and are not inspected.
//!
//! The detector is designed to **fail closed**:
//! - the line is tokenized with basic POSIX shell quoting (single/double quotes, backslash
//!   escapes, comments) and split into command segments on `;`, `&&`, `||`, `|`, `&`, newlines,
//!   `$(`, backticks and parentheses, so `$(git …)`, `(git …)` and `if …; then git …; fi` are all
//!   inspected;
//! - a `git` program token (or a path ending in `/git`) is looked for *anywhere* in a segment, so
//!   wrappers such as `sudo -u root`, `env`, `time`, `nohup` and `xargs` do not hide it;
//! - scripts passed to `sh -c '…'` / `bash -lc "…"`, `eval …` and double-quoted command
//!   substitutions are analysed recursively;
//! - if the line mentions `git` together with a destructive keyword but cannot be analysed
//!   confidently (unbalanced quotes, `alias`, functions forwarding `"$@"`, variables in command
//!   position, `-c alias.*`, excessive nesting), confirmation is required anyway.

/// Branch names treated as the repository's primary trunk.
pub const TRUNK_BRANCH_NAMES: [&str; 3] = ["main", "master", "trunk"];

/// Returns `true` when `branch` is one of [`TRUNK_BRANCH_NAMES`].
pub fn is_trunk_branch(branch: &str) -> bool {
    TRUNK_BRANCH_NAMES.contains(&branch)
}

/// A destructive `git` command detected in a terminal command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructiveGitCommand {
    /// The offending command segment (normalised), shown verbatim in the confirmation.
    pub command: String,
    /// Plain-language explanation of what will be lost.
    pub reason: String,
}

/// Maximum nesting of `sh -c` / `eval` / substitution scripts that is analysed.
const MAX_DEPTH: usize = 4;

/// Words that, together with `git`, make an unparsable line worth confirming.
const DESTRUCTIVE_KEYWORDS: &[&str] = &[
    "reset",
    "clean",
    "push",
    "checkout",
    "restore",
    "switch",
    "stash",
    "reflog",
    "update-ref",
    "worktree",
    "tag",
    "branch",
    "filter-branch",
    "--hard",
    "--force",
    "--delete",
    "-f",
    "-d",
    "-D",
];

/// Scans `line` for the first destructive `git` command.
///
/// `current_branch` is the checked-out branch (if any) and is used to flag deleting it.
pub fn detect_destructive_git_command(
    line: &str,
    current_branch: Option<&str>,
) -> Option<DestructiveGitCommand> {
    let mut analysis = Analysis::default();
    analyze_line(line, current_branch, 0, &mut analysis);
    if let Some(found) = analysis.found {
        return Some(found);
    }
    (analysis.unconfident && mentions_git_with_destructive_keyword(line)).then(|| {
        DestructiveGitCommand {
            command: line.trim().to_string(),
            reason: "This command line could not be checked reliably and may run a destructive \
                     git operation."
                .to_string(),
        }
    })
}

#[derive(Default)]
struct Analysis {
    found: Option<DestructiveGitCommand>,
    unconfident: bool,
}

fn analyze_line(line: &str, current_branch: Option<&str>, depth: usize, out: &mut Analysis) {
    if out.found.is_some() {
        return;
    }
    if depth > MAX_DEPTH {
        out.unconfident = true;
        return;
    }
    let segments = match tokenize(line) {
        Ok(segments) => segments,
        Err(()) => {
            out.unconfident = true;
            return;
        }
    };
    for segment in &segments {
        analyze_segment(segment, current_branch, depth, out);
        if out.found.is_some() {
            return;
        }
    }
}

fn analyze_segment(
    segment: &[Token],
    current_branch: Option<&str>,
    depth: usize,
    out: &mut Analysis,
) {
    // 1. Recurse into nested scripts first (they run regardless of the outer command word).
    for (i, tok) in segment.iter().enumerate() {
        let follows_dash_c = i > 0
            && segment
                .get(i - 1)
                .is_some_and(|p| is_dash_c_option(&p.text));
        if tok.has_substitution || follows_dash_c {
            analyze_line(&tok.text, current_branch, depth + 1, out);
            if out.found.is_some() {
                return;
            }
        }
        if tok.text == "eval" {
            let script = segment
                .get(i + 1..)
                .unwrap_or_default()
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            analyze_line(&script, current_branch, depth + 1, out);
            if out.found.is_some() {
                return;
            }
        }
        if matches!(tok.text.as_str(), "alias" | "function") {
            out.unconfident = true;
        }
    }

    // 2. Command word (first token that is not a `VAR=value` assignment).
    let Some(cmd_idx) = segment.iter().position(|t| !is_env_assignment(&t.text)) else {
        return;
    };
    let Some(cmd_word) = segment.get(cmd_idx) else {
        return;
    };
    if cmd_word.text.starts_with('$') {
        out.unconfident = true;
    }
    if matches!(cmd_word.text.as_str(), "echo" | "printf") {
        return;
    }

    // 3. Every `git` program token anywhere in the segment.
    let texts: Vec<&str> = segment.iter().map(|t| t.text.as_str()).collect();
    for (i, tok) in texts.iter().enumerate() {
        if !is_git_program(tok) {
            continue;
        }
        let args = texts.get(i + 1..).unwrap_or_default();
        match classify_git_invocation(args, current_branch) {
            Classification::Destructive(reason) => {
                out.found = Some(DestructiveGitCommand {
                    command: texts.join(" "),
                    reason,
                });
                return;
            }
            Classification::Unknown => out.unconfident = true,
            Classification::Safe => {}
        }
    }
}

fn is_env_assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !name.as_bytes().first().is_some_and(u8::is_ascii_digit)
        }
        None => false,
    }
}

/// `-c`, `-lc`, `-ic`, … (a short-option cluster containing `c`): the next argument is a script.
fn is_dash_c_option(tok: &str) -> bool {
    tok.strip_prefix('-').is_some_and(|rest| {
        !rest.starts_with('-')
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_alphabetic())
            && rest.contains('c')
    })
}

fn is_git_program(tok: &str) -> bool {
    let name = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    name == "git" || name.eq_ignore_ascii_case("git.exe")
}

fn mentions_git_with_destructive_keyword(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    if !lower.contains("git") {
        return false;
    }
    let words: Vec<&str> = line
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '\'' | '"' | '`' | '=')
        })
        .filter(|w| !w.is_empty())
        .collect();
    DESTRUCTIVE_KEYWORDS.iter().any(|k| words.contains(k))
}

// ---------------------------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Token {
    /// Unquoted token text.
    text: String,
    /// The token contained a `$(…)` or backtick substitution inside double quotes.
    has_substitution: bool,
}

/// Splits `line` into command segments of unquoted tokens. Returns `Err` on unbalanced quotes
/// or a trailing backslash.
fn tokenize(line: &str) -> Result<Vec<Vec<Token>>, ()> {
    let mut segments: Vec<Vec<Token>> = Vec::new();
    let mut segment: Vec<Token> = Vec::new();
    let mut current = Token::default();
    let mut in_token = false;
    let mut chars = line.chars().peekable();

    fn end_token(current: &mut Token, in_token: &mut bool, segment: &mut Vec<Token>) {
        if *in_token {
            segment.push(std::mem::take(current));
            *in_token = false;
        }
    }
    fn end_segment(segment: &mut Vec<Token>, segments: &mut Vec<Vec<Token>>) {
        if !segment.is_empty() {
            segments.push(std::mem::take(segment));
        }
    }

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => end_token(&mut current, &mut in_token, &mut segment),
            '\n' | ';' | '|' | '&' | '(' | ')' | '`' => {
                end_token(&mut current, &mut in_token, &mut segment);
                end_segment(&mut segment, &mut segments);
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                end_token(&mut current, &mut in_token, &mut segment);
                end_segment(&mut segment, &mut segments);
            }
            '#' if !in_token => {
                // Comment through end of line.
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                end_segment(&mut segment, &mut segments);
            }
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(n) => {
                    current.text.push(n);
                    in_token = true;
                }
                None => return Err(()),
            },
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(n) => current.text.push(n),
                        None => return Err(()),
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => current.text.push(n),
                            Some('\n') => {}
                            Some(n) => {
                                current.text.push('\\');
                                current.text.push(n);
                            }
                            None => return Err(()),
                        },
                        Some('$') if chars.peek() == Some(&'(') => {
                            current.has_substitution = true;
                            current.text.push('$');
                        }
                        Some('`') => {
                            current.has_substitution = true;
                            current.text.push('`');
                        }
                        Some(n) => current.text.push(n),
                        None => return Err(()),
                    }
                }
            }
            other => {
                current.text.push(other);
                in_token = true;
            }
        }
    }
    end_token(&mut current, &mut in_token, &mut segment);
    end_segment(&mut segment, &mut segments);
    Ok(segments)
}

// ---------------------------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------------------------

enum Classification {
    Safe,
    Destructive(String),
    /// Cannot tell (variable subcommand, `-c alias.*`, …).
    Unknown,
}

/// Global options that consume the following token when not written as `--opt=value`.
const GLOBAL_OPTS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--super-prefix",
    "--exec-path",
];

/// Classifies the arguments following a `git` program token.
fn classify_git_invocation(args: &[&str], current_branch: Option<&str>) -> Classification {
    let mut idx = 0;
    let mut unknown = false;
    while let Some(tok) = args.get(idx) {
        if GLOBAL_OPTS_WITH_VALUE.contains(tok) {
            if *tok == "-c"
                && args
                    .get(idx + 1)
                    .is_some_and(|v| v.to_ascii_lowercase().starts_with("alias."))
            {
                unknown = true;
            }
            idx += 2;
        } else if tok.starts_with('-') {
            if tok.starts_with("-c") && tok.to_ascii_lowercase().contains("alias.") {
                unknown = true;
            }
            idx += 1;
        } else {
            break;
        }
    }
    let Some(subcommand) = args.get(idx) else {
        return Classification::Safe;
    };
    if subcommand.contains('$') {
        return Classification::Unknown;
    }
    let sub_args = args.get(idx + 1..).unwrap_or_default();
    match classify_subcommand(subcommand, sub_args, current_branch) {
        Some(reason) => Classification::Destructive(reason),
        None if unknown => Classification::Unknown,
        None => Classification::Safe,
    }
}

fn classify_subcommand(
    subcommand: &str,
    args: &[&str],
    current_branch: Option<&str>,
) -> Option<String> {
    let first_positional = args.iter().find(|a| !a.starts_with('-')).copied();
    match subcommand {
        "reset" => has_long(args, "--hard")
            .then(|| "`reset --hard` permanently discards uncommitted changes.".to_string()),
        "clean" => (has_long(args, "--force") || has_short_flag(args, 'f'))
            .then(|| "`clean -f` permanently deletes untracked files.".to_string()),
        "push" => classify_push(args),
        "branch" => classify_branch(args, current_branch),
        "checkout" => classify_checkout(args),
        "switch" => (has_long(args, "--force")
            || has_long(args, "--discard-changes")
            || has_short_flag(args, 'f'))
        .then(|| "This switch discards local changes in your working tree.".to_string()),
        "restore" => {
            let staged_only = (has_long(args, "--staged") || has_short_flag(args, 'S'))
                && !(has_long(args, "--worktree") || has_short_flag(args, 'W'));
            (!staged_only)
                .then(|| "`restore` overwrites local changes in your working tree.".to_string())
        }
        "stash" => matches!(first_positional, Some("clear" | "drop")).then(|| {
            "This permanently deletes stashed changes (`stash clear` / `stash drop`).".to_string()
        }),
        "reflog" => matches!(first_positional, Some("expire" | "delete")).then(|| {
            "Expiring or deleting reflog entries removes the safety net for recovering commits."
                .to_string()
        }),
        "update-ref" => (has_long(args, "--delete") || has_short_flag(args, 'd'))
            .then(|| "`update-ref -d` deletes a ref without any safety checks.".to_string()),
        "worktree" => (first_positional == Some("remove")
            && (has_long(args, "--force") || has_short_flag(args, 'f')))
        .then(|| {
            "`worktree remove --force` deletes a worktree including its uncommitted changes."
                .to_string()
        }),
        "tag" => {
            if has_long(args, "--delete") || has_short_flag(args, 'd') {
                Some("`tag -d` deletes tags.".to_string())
            } else {
                (has_long(args, "--force") || has_short_flag(args, 'f'))
                    .then(|| "`tag -f` overwrites an existing tag.".to_string())
            }
        }
        "filter-branch" => {
            Some("`filter-branch` rewrites the history of your branches.".to_string())
        }
        _ => None,
    }
}

fn classify_push(args: &[&str]) -> Option<String> {
    let forced = has_short_flag(args, 'f')
        || args
            .iter()
            .any(|a| *a == "--force" || a.starts_with("--force-with-lease") || *a == "--mirror")
        || args.iter().any(|a| a.starts_with('+') && a.len() > 1);
    if forced {
        return Some(
            "Force push can remove commits from the remote that are not on your branch."
                .to_string(),
        );
    }
    let deletes = has_long(args, "--delete")
        || has_long(args, "--prune")
        || has_short_flag(args, 'd')
        || args.iter().any(|a| a.starts_with(':') && a.len() > 1);
    deletes.then(|| "This push deletes refs (branches or tags) on the remote.".to_string())
}

fn classify_checkout(args: &[&str]) -> Option<String> {
    let reason = || "This checkout overwrites local changes in your working tree.".to_string();
    let discards_paths = args
        .iter()
        .position(|a| *a == "--")
        .is_some_and(|pos| pos + 1 < args.len());
    let forced = has_long(args, "--force") || has_short_flag(args, 'f');
    if discards_paths || forced {
        return Some(reason());
    }
    let creates_branch =
        has_short_flag(args, 'b') || has_short_flag(args, 'B') || has_long(args, "--orphan");
    let positional: Vec<&str> = args
        .iter()
        .copied()
        .take_while(|a| *a != "--")
        .filter(|a| !a.starts_with('-'))
        .collect();
    let pathlike = positional.iter().any(|p| {
        matches!(*p, "." | "..")
            || p.starts_with("./")
            || p.starts_with("../")
            || p.starts_with(":/")
            || p.contains(['*', '?'])
    });
    // `git checkout <rev> <path>…` overwrites the paths from `<rev>`.
    let rev_and_paths = !creates_branch && positional.len() >= 2;
    (pathlike || rev_and_paths).then(reason)
}

fn classify_branch(args: &[&str], current_branch: Option<&str>) -> Option<String> {
    let force_delete = has_short_flag(args, 'D')
        || ((has_long(args, "--delete") || has_short_flag(args, 'd'))
            && (has_long(args, "--force") || has_short_flag(args, 'f')));
    let any_delete = force_delete || has_long(args, "--delete") || has_short_flag(args, 'd');
    if !any_delete {
        let force_move = has_short_flag(args, 'M')
            || has_short_flag(args, 'C')
            || has_short_flag(args, 'f')
            || has_long(args, "--force");
        return force_move.then(|| {
            "This force-moves or overwrites an existing branch, which can orphan its commits."
                .to_string()
        });
    }
    let targets: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect();
    if let Some(trunk) = targets.iter().find(|t| is_trunk_branch(t)) {
        return Some(format!(
            "This deletes '{trunk}', the repository's primary branch."
        ));
    }
    if let Some(current) = current_branch
        && targets.contains(&current)
    {
        return Some(format!(
            "This deletes '{current}', the branch you have checked out."
        ));
    }
    force_delete.then(|| {
        "`branch -D` deletes branches even if their commits are not merged anywhere.".to_string()
    })
}

fn has_long(args: &[&str], flag: &str) -> bool {
    args.iter().take_while(|a| **a != "--").any(|a| *a == flag)
}

/// Matches `-x` and short-flag clusters like `-fd`, stopping at a bare `--`.
fn has_short_flag(args: &[&str], flag: char) -> bool {
    args.iter().take_while(|a| **a != "--").any(|a| {
        a.strip_prefix('-')
            .is_some_and(|rest| !rest.starts_with('-') && rest.contains(flag))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(line: &str) -> bool {
        detect_destructive_git_command(line, Some("feature/x")).is_some()
    }

    #[test]
    fn flags_destructive_commands() {
        for line in [
            "git reset --hard HEAD~1",
            "git clean -fd",
            "git clean --force",
            "git push -f origin main",
            "git push --force",
            "git push --force-with-lease",
            "git push origin +main",
            "git branch -D old",
            "git checkout -- .",
            "git checkout -f main",
            "git restore src/lib.rs",
            "git restore --staged --worktree .",
            "git -C repo reset --hard",
            "cd repo && git reset --hard",
            "GIT_TRACE=1 git clean -xdf",
            "/usr/bin/git push -uf origin x",
        ] {
            assert!(flagged(line), "expected confirmation for `{line}`");
        }
    }

    #[test]
    fn flags_wrapper_and_shell_bypasses() {
        for line in [
            "sh -c 'git reset --hard'",
            "bash -c \"git clean -fdx\"",
            "bash -lc 'cd repo && git push -f'",
            "env git reset --hard",
            "env -i PATH=/usr/bin git reset --hard",
            "time git push --force",
            "nohup git clean -f &",
            "echo old | xargs git branch -D",
            "\"git\" reset --hard",
            "'git' reset --hard",
            "g\\it reset --hard",
            "echo $(git reset --hard)",
            "echo \"$(git reset --hard)\"",
            "echo `git reset --hard`",
            "(git reset --hard)",
            "if true; then git reset --hard; fi",
            "sudo -u root git reset --hard",
            "sudo git clean -fd",
            "git -C \"my dir\" reset --hard",
            "git -C 'my dir' -c core.pager=less --no-pager reset --hard",
            "git --git-dir=.git --work-tree . reset --hard",
            "git --git-dir .git reset --hard",
            "eval \"git reset --hard\"",
            "sh -c \"bash -c 'git reset --hard'\"",
            "ls # harmless\ngit reset --hard",
        ] {
            assert!(flagged(line), "expected confirmation for `{line}`");
        }
    }

    #[test]
    fn flags_additional_destructive_subcommands() {
        for line in [
            "git checkout .",
            "git checkout HEAD .",
            "git checkout HEAD~1 -- src/lib.rs",
            "git checkout main src/lib.rs",
            "git checkout '*.rs'",
            "git switch -f main",
            "git switch --discard-changes main",
            "git stash clear",
            "git stash drop",
            "git stash drop stash@{1}",
            "git push origin :feature",
            "git push origin --delete feature",
            "git push -d origin feature",
            "git push --prune origin",
            "git reflog expire --expire=now --all",
            "git reflog delete HEAD@{1}",
            "git update-ref -d refs/heads/x",
            "git update-ref --delete refs/heads/x",
            "git worktree remove -f ../wt",
            "git worktree remove --force ../wt",
            "git tag -d v1.0",
            "git tag --delete v1.0",
            "git branch -M new-name",
            "git branch -f topic HEAD~3",
            "git filter-branch --tree-filter 'rm x' HEAD",
        ] {
            assert!(flagged(line), "expected confirmation for `{line}`");
        }
    }

    #[test]
    fn unparsable_or_aliased_lines_fail_closed() {
        for line in [
            "git reset --hard 'unterminated",
            "sh -c \"git push -f",
            "alias g=git; g reset --hard",
            "g() { git \"$@\"; }; g reset --hard",
            "G=git; $G reset --hard",
            "git -c alias.nuke='reset --hard' nuke",
            "git $SUB --hard",
        ] {
            assert!(flagged(line), "expected confirmation for `{line}`");
        }
    }

    #[test]
    fn allows_safe_commands() {
        for line in [
            "git status",
            "git log --oneline -20",
            "git diff HEAD~1",
            "git reset --soft HEAD~1",
            "git clean -n",
            "git push origin feature/x",
            "git push -u origin feature/x",
            "git branch -d merged-topic",
            "git branch -c copy",
            "git checkout main",
            "git checkout -b x",
            "git checkout -b x origin/x",
            "git switch x",
            "git switch -c new-feature",
            "git restore --staged src/lib.rs",
            "git stash",
            "git stash pop",
            "git stash list",
            "git tag v1.0",
            "git tag -a v1.0 -m 'release'",
            "git worktree remove ../wt",
            "git reflog",
            "git commit -m 'docs: explain git reset --hard'",
            "echo reset --hard",
            "echo git reset --hard",
            "cat .gitignore",
            "cd git && ls",
            "which git",
            "ls -la",
            "",
            "   ",
        ] {
            assert!(!flagged(line), "unexpected confirmation for `{line}`");
        }
        // Unparsable lines without destructive keywords are not flagged.
        assert!(!flagged("echo 'unterminated"));
        assert!(!flagged("git log 'unterminated"));
    }

    #[test]
    fn reports_offending_segment() {
        let found = detect_destructive_git_command("ls && sudo git reset --hard; ls", None);
        assert_eq!(
            found.map(|f| f.command).as_deref(),
            Some("sudo git reset --hard")
        );
    }

    #[test]
    fn deleting_trunk_or_current_branch_is_flagged_even_with_safe_delete() {
        let trunk = detect_destructive_git_command("git branch -d main", None);
        assert!(trunk.is_some_and(|d| d.reason.contains("primary branch")));
        let current =
            detect_destructive_git_command("git branch --delete feature/x", Some("feature/x"));
        assert!(current.is_some_and(|d| d.reason.contains("checked out")));
    }
}
