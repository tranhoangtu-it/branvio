//! End-to-end integration tests wiring `diff-merge` with real Git repositories (`git2` + `git`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use branvio_core::domain::{
    ConflictLinePick, ConflictResolution, ConflictSide, DeltaArea, DeltaStatus, DiffLineOrigin,
    EolChange, FileDelta, FileMode, SymbolKind,
};
use branvio_core::paths::{HostPath, RepoRelPath};
use branvio_core::platform::HydratedGitEnv;
use diff_merge::{
    BranvioDiffMergeEngine, ConflictedFileInputs, DiffEngine, DiffOptions, LineSelectionSpec,
    MergeConflictEngine, PatchBuilder, PatchDirection, SyntaxHighlighter,
};
use git_engine::{BranvioGitEngine, GitIndexEngine};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn run_git(repo_dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Branvio Test")
        .env("GIT_AUTHOR_EMAIL", "test@branvio.rs")
        .env("GIT_COMMITTER_NAME", "Branvio Test")
        .env("GIT_COMMITTER_EMAIL", "test@branvio.rs")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git command should run");
    assert!(
        out.status.success(),
        "git {:?} failed: stderr={}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn init_repo() -> (TempDir, HostPath, BranvioGitEngine) {
    let dir = TempDir::new().unwrap();
    run_git(dir.path(), &["init", "-b", "main"]);
    run_git(dir.path(), &["config", "user.name", "Branvio Test"]);
    run_git(dir.path(), &["config", "user.email", "test@branvio.rs"]);
    run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
    run_git(dir.path(), &["config", "core.autocrlf", "false"]);

    let root = HostPath::canonicalize_existing(dir.path()).unwrap();
    let engine = BranvioGitEngine::new(Arc::new(HydratedGitEnv::from_current_process()), None);
    (dir, root, engine)
}

fn make_delta(path: &str, status: DeltaStatus, area: DeltaArea) -> FileDelta {
    FileDelta {
        old_path: None,
        new_path: RepoRelPath::normalize(path).unwrap(),
        status,
        area,
        old_mode: if matches!(status, DeltaStatus::Added | DeltaStatus::Untracked) {
            FileMode::Absent
        } else {
            FileMode::Regular644
        },
        new_mode: if status == DeltaStatus::Deleted {
            FileMode::Absent
        } else {
            FileMode::Regular644
        },
        old_oid: None,
        new_oid: None,
        is_binary: false,
        lfs: None,
        eol_change: EolChange::Unchanged,
        additions: 0,
        deletions: 0,
    }
}

#[tokio::test]
async fn surgical_line_stage_unstage_discard_and_ast_highlighting_with_git2() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();

    let mut base_src = String::from("pub fn work() {\n");
    for i in 1..=22 {
        base_src.push_str(&format!("    let v{i} = {i};\n"));
    }
    base_src.push_str("}\n");

    std::fs::write(dir.path().join("work.rs"), &base_src).unwrap();
    run_git(dir.path(), &["add", "work.rs"]);
    run_git(dir.path(), &["commit", "-m", "base work.rs"]);

    // Modify line 2 in hunk 0 (`let v1 = 100;` + add `let v1_extra = 999;`) and line 22 in hunk 1 (`let v21 = 210;`).
    let edited_src = base_src
        .replace(
            "    let v1 = 1;\n",
            "    let v1 = 100;\n    let v1_extra = 999;\n",
        )
        .replace("    let v21 = 21;\n", "    let v21 = 210;\n");
    std::fs::write(dir.path().join("work.rs"), &edited_src).unwrap();

    let mut patch = dm
        .diff_blobs(
            make_delta("work.rs", DeltaStatus::Modified, DeltaArea::Unstaged),
            base_src.as_bytes(),
            edited_src.as_bytes(),
            &DiffOptions::default(),
        )
        .unwrap();
    dm.highlight_patch(
        &mut patch,
        Some(base_src.as_bytes()),
        Some(edited_src.as_bytes()),
    )
    .unwrap();

    assert_eq!(patch.hunks.len(), 2);
    let sym0 = patch.hunks[0].enclosing_symbol.as_ref().unwrap();
    assert_eq!(sym0.kind, SymbolKind::Function);
    assert!(sym0.signature.contains("pub fn work()"));

    // Select ONLY `+    let v1_extra = 999;` in hunk 0 (leaving `-v1 = 1` + `+v1 = 100` and hunk 1 unstaged!).
    let extra_idx = patch.hunks[0]
        .lines
        .iter()
        .position(|l| {
            l.origin == DiffLineOrigin::Addition && patch.line_text(l).contains("v1_extra")
        })
        .unwrap();
    let stage_patch = dm
        .build_line_selection_patch(
            &patch,
            &[LineSelectionSpec {
                hunk_index: 0,
                selected_line_indices: BTreeSet::from([extra_idx]),
            }],
            PatchDirection::StageToIndex,
        )
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &stage_patch)
        .await
        .unwrap();

    let index_after_stage = String::from_utf8(run_git(dir.path(), &["show", ":work.rs"])).unwrap();
    assert!(index_after_stage.contains("let v1 = 1;\n    let v1_extra = 999;\n"));
    assert!(index_after_stage.contains("let v21 = 21;\n"));

    // Now diff index -> worktree for remaining unstaged edits, and discard hunk 1 from worktree!
    let mut unstaged_patch = dm
        .diff_blobs(
            make_delta("work.rs", DeltaStatus::Modified, DeltaArea::Unstaged),
            index_after_stage.as_bytes(),
            edited_src.as_bytes(),
            &DiffOptions::default(),
        )
        .unwrap();
    assert_eq!(unstaged_patch.hunks.len(), 2);
    let discard_hunk1 = dm
        .build_hunk_patch(&unstaged_patch, 1, PatchDirection::DiscardInWorktree)
        .unwrap();
    git_engine
        .apply_reverse_patch_to_worktree(
            &root,
            &RepoRelPath::normalize("work.rs").unwrap(),
            &discard_hunk1,
        )
        .await
        .unwrap();
    let worktree_after_discard = std::fs::read_to_string(dir.path().join("work.rs")).unwrap();
    assert!(worktree_after_discard.contains("let v1 = 100;"));
    assert!(worktree_after_discard.contains("let v21 = 21;\n"));

    // Finally unstage the single staged line from `.git/index` back to HEAD!
    let staged_patch = dm
        .diff_blobs(
            make_delta("work.rs", DeltaStatus::Modified, DeltaArea::Staged),
            base_src.as_bytes(),
            index_after_stage.as_bytes(),
            &DiffOptions::default(),
        )
        .unwrap();
    let unstage_bytes = dm
        .build_hunk_patch(&staged_patch, 0, PatchDirection::UnstageFromIndex)
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &unstage_bytes)
        .await
        .unwrap();
    let index_reverted = String::from_utf8(run_git(dir.path(), &["show", ":work.rs"])).unwrap();
    assert_eq!(index_reverted, base_src);
    let _ = &mut unstaged_patch;
}

#[tokio::test]
async fn partial_and_full_stage_unstage_of_brand_new_added_file_with_git2() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();

    std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
    run_git(dir.path(), &["add", "seed.txt"]);
    run_git(dir.path(), &["commit", "-m", "seed"]);

    let added_text = b"fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n";
    std::fs::write(dir.path().join("new_mod.rs"), added_text).unwrap();

    let patch = dm
        .diff_blobs(
            make_delta("new_mod.rs", DeltaStatus::Added, DeltaArea::Unstaged),
            b"",
            added_text,
            &DiffOptions::default(),
        )
        .unwrap();

    // Stage only line 0 ("fn one") and line 2 ("fn three") from untracked file into index!
    let stage_two = dm
        .build_line_selection_patch(
            &patch,
            &[LineSelectionSpec {
                hunk_index: 0,
                selected_line_indices: BTreeSet::from([0, 2]),
            }],
            PatchDirection::StageToIndex,
        )
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &stage_two)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(run_git(dir.path(), &["show", ":new_mod.rs"])).unwrap(),
        "fn one() {}\nfn three() {}\n"
    );

    // Unstage ONLY line 0 ("fn one") -> index retains "fn three() {}\n"!
    let staged_bytes = run_git(dir.path(), &["show", ":new_mod.rs"]);
    let staged_patch = dm
        .diff_blobs(
            make_delta("new_mod.rs", DeltaStatus::Added, DeltaArea::Staged),
            b"",
            &staged_bytes,
            &DiffOptions::default(),
        )
        .unwrap();
    let unstage_one = dm
        .build_line_selection_patch(
            &staged_patch,
            &[LineSelectionSpec {
                hunk_index: 0,
                selected_line_indices: BTreeSet::from([0]),
            }],
            PatchDirection::UnstageFromIndex,
        )
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &unstage_one)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(run_git(dir.path(), &["show", ":new_mod.rs"])).unwrap(),
        "fn three() {}\n"
    );

    // Unstage remaining hunk -> file completely deleted from `.git/index`!
    let rem_bytes = run_git(dir.path(), &["show", ":new_mod.rs"]);
    let rem_patch = dm
        .diff_blobs(
            make_delta("new_mod.rs", DeltaStatus::Added, DeltaArea::Staged),
            b"",
            &rem_bytes,
            &DiffOptions::default(),
        )
        .unwrap();
    let unstage_all = dm
        .build_hunk_patch(&rem_patch, 0, PatchDirection::UnstageFromIndex)
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &unstage_all)
        .await
        .unwrap();
    let cached_diff = run_git(dir.path(), &["diff", "--cached", "--name-only"]);
    assert!(cached_diff.is_empty());
}

#[tokio::test]
async fn crlf_and_eof_no_newline_partial_line_staging_roundtrip_with_git2() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();

    let crlf_old = b"line 1\r\nline 2\r\nline 3\r\n";
    let crlf_new = b"line 1\r\nline 2B\r\nline 3B\r\n";
    std::fs::write(dir.path().join("crlf.txt"), crlf_old).unwrap();
    std::fs::write(dir.path().join("noeol.txt"), b"head\ntail").unwrap();
    run_git(dir.path(), &["add", "."]);
    run_git(dir.path(), &["commit", "-m", "crlf and noeol"]);

    std::fs::write(dir.path().join("crlf.txt"), crlf_new).unwrap();
    std::fs::write(dir.path().join("noeol.txt"), b"head\ntail_updated").unwrap();

    // Stage ONLY the line 2 -> line 2B replacement in crlf.txt, preserving exact `\r\n`!
    let crlf_patch = dm
        .diff_blobs(
            make_delta("crlf.txt", DeltaStatus::Modified, DeltaArea::Unstaged),
            crlf_old,
            crlf_new,
            &DiffOptions::default(),
        )
        .unwrap();
    let del_2 = crlf_patch.hunks[0]
        .lines
        .iter()
        .position(|l| {
            l.origin == DiffLineOrigin::Deletion && crlf_patch.line_text(l).starts_with("line 2")
        })
        .unwrap();
    let add_2b = crlf_patch.hunks[0]
        .lines
        .iter()
        .position(|l| {
            l.origin == DiffLineOrigin::Addition && crlf_patch.line_text(l).starts_with("line 2B")
        })
        .unwrap();
    let stage_crlf = dm
        .build_line_selection_patch(
            &crlf_patch,
            &[LineSelectionSpec {
                hunk_index: 0,
                selected_line_indices: BTreeSet::from([del_2, add_2b]),
            }],
            PatchDirection::StageToIndex,
        )
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &stage_crlf)
        .await
        .unwrap();
    assert_eq!(
        run_git(dir.path(), &["cat-file", "blob", ":crlf.txt"]),
        b"line 1\r\nline 2B\r\nline 3\r\n"
    );

    // Stage the full hunk changing non-newline-terminated EOF line.
    let noeol_patch = dm
        .diff_blobs(
            make_delta("noeol.txt", DeltaStatus::Modified, DeltaArea::Unstaged),
            b"head\ntail",
            b"head\ntail_updated",
            &DiffOptions::default(),
        )
        .unwrap();
    let stage_noeol = dm
        .build_hunk_patch(&noeol_patch, 0, PatchDirection::StageToIndex)
        .unwrap();
    git_engine
        .apply_synthetic_patch_to_index(&root, &stage_noeol)
        .await
        .unwrap();
    assert_eq!(
        run_git(dir.path(), &["cat-file", "blob", ":noeol.txt"]),
        b"head\ntail_updated"
    );
}

#[tokio::test]
async fn end_to_end_git_merge_conflict_4_rope_resolution_and_commit() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();

    std::fs::write(
        dir.path().join("shared.rs"),
        "fn header() {}\nlet v = 0;\nfn footer() {}\n",
    )
    .unwrap();
    run_git(dir.path(), &["add", "shared.rs"]);
    run_git(dir.path(), &["commit", "-m", "base"]);

    run_git(dir.path(), &["checkout", "-b", "topic"]);
    std::fs::write(
        dir.path().join("shared.rs"),
        "fn header() {}\nlet v = 2;\nfn footer() {}\n",
    )
    .unwrap();
    run_git(dir.path(), &["commit", "-am", "topic change"]);

    run_git(dir.path(), &["checkout", "main"]);
    std::fs::write(
        dir.path().join("shared.rs"),
        "fn header() {}\nlet v = 1;\nfn footer() {}\n",
    )
    .unwrap();
    run_git(dir.path(), &["commit", "-am", "main change"]);

    let merge_out = Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "topic"])
        .output()
        .unwrap();
    assert!(!merge_out.status.success());

    let base_bytes = run_git(dir.path(), &["cat-file", "blob", ":1:shared.rs"]);
    let ours_bytes = run_git(dir.path(), &["cat-file", "blob", ":2:shared.rs"]);
    let theirs_bytes = run_git(dir.path(), &["cat-file", "blob", ":3:shared.rs"]);
    let wt_bytes = std::fs::read(dir.path().join("shared.rs")).unwrap();

    let mut doc = dm
        .open_conflicted_file(ConflictedFileInputs {
            path: RepoRelPath::normalize("shared.rs").unwrap(),
            base_bytes: Some(&base_bytes),
            ours_bytes: &ours_bytes,
            theirs_bytes: &theirs_bytes,
            worktree_with_markers: Some(&wt_bytes),
            ours_label: "HEAD",
            theirs_label: "topic",
        })
        .unwrap();
    assert_eq!(doc.regions.len(), 1);
    assert_eq!(
        doc.extract_region_lines(&doc.regions[0], false, true),
        vec!["let v = 0;\n"]
    );

    dm.resolve_region(
        &mut doc,
        0,
        ConflictResolution::LineLevelSelection(vec![ConflictLinePick {
            side: ConflictSide::Theirs,
            relative_line_idx: 0,
        }]),
    )
    .unwrap();
    let exported = dm.export_merged_bytes(&doc, true).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&exported),
        "fn header() {}\nlet v = 2;\nfn footer() {}\n"
    );

    std::fs::write(dir.path().join("shared.rs"), &exported).unwrap();
    git_engine
        .stage_files(&root, &[RepoRelPath::normalize("shared.rs").unwrap()])
        .await
        .unwrap();
    run_git(dir.path(), &["commit", "--no-edit"]);
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn gen_range(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }

    fn gen_bool(&mut self, pct: usize) -> bool {
        self.gen_range(100) < pct
    }
}

#[inline]
fn append_line_eol(
    out: &mut String,
    eol: &str,
    is_last: bool,
    allow_no_nl: bool,
    rng: &mut SplitMix64,
) {
    let omit_eol = is_last && allow_no_nl && rng.gen_bool(25);
    if !omit_eol {
        out.push_str(eol);
    }
}

fn gen_text_from_lines(rng: &mut SplitMix64, crlf: bool, allow_no_nl: bool) -> Vec<u8> {
    const VOCAB: &[&str] = &["alpha", "beta", "gamma", "delta", "epsilon", "zeta", ""];
    let n = rng.gen_range(18);
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    for idx in 0..n {
        let base_tok = if rng.gen_bool(75) {
            VOCAB[rng.gen_range(VOCAB.len())]
        } else {
            // Mix unique-ish lines so histogram/myers produces realistic multi-hunk gaps.
            return build_seeded_doc(rng, n, crlf, allow_no_nl);
        };
        if idx > 0 {
            s.push_str(eol);
        }
        s.push_str(base_tok);
    }
    if !s.is_empty() {
        append_line_eol(&mut s, eol, true, allow_no_nl, rng);
    }
    s.into_bytes()
}

fn build_seeded_doc(
    rng: &mut SplitMix64,
    line_count: usize,
    crlf: bool,
    allow_no_nl: bool,
) -> Vec<u8> {
    const VOCAB: &[&str] = &[
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "line_k", "",
    ];
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    for i in 0..line_count {
        let pick = rng.gen_range(VOCAB.len() + 3);
        if pick < VOCAB.len() {
            s.push_str(VOCAB[pick]);
        } else {
            s.push_str(&format!("u_{}", rng.gen_range(12)));
        }
        append_line_eol(&mut s, eol, i + 1 == line_count, allow_no_nl, rng);
    }
    s.into_bytes()
}

fn mutate_doc_lines(rng: &mut SplitMix64, base: &[u8], crlf: bool, allow_no_nl: bool) -> Vec<u8> {
    let eol = if crlf { "\r\n" } else { "\n" };
    let base_str = std::str::from_utf8(base).unwrap();
    let mut lines: Vec<String> = base_str
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if base_str.ends_with('\n') && matches!(lines.last(), Some(last) if last.is_empty()) {
        lines.pop();
    }

    let mut out_lines = Vec::new();
    for line in &lines {
        match rng.gen_range(10) {
            0..=6 => out_lines.push(line.clone()),
            7 => out_lines.push(format!("{line}_m{}", rng.gen_range(4))),
            8 => {
                // Keep line and insert another after it.
                out_lines.push(line.clone());
                out_lines.push(format!("ins_{}", rng.gen_range(6)));
            }
            _ => {} // delete line
        }
    }
    if rng.gen_bool(30) {
        let pos = rng.gen_range(out_lines.len() + 1);
        out_lines.insert(pos, format!("extra_{}", rng.gen_range(5)));
    }

    let mut out = String::new();
    let total = out_lines.len();
    for (idx, l) in out_lines.iter().enumerate() {
        out.push_str(l);
        append_line_eol(&mut out, eol, idx + 1 == total, allow_no_nl, rng);
    }
    out.into_bytes()
}

fn run_git_with_stdin(repo_dir: &Path, args: &[&str], stdin_bytes: &[u8]) -> std::process::Output {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .current_dir(repo_dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Branvio Test")
        .env("GIT_AUTHOR_EMAIL", "test@branvio.rs")
        .env("GIT_COMMITTER_NAME", "Branvio Test")
        .env("GIT_COMMITTER_EMAIL", "test@branvio.rs")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git spawn should succeed");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_bytes).unwrap();
    }
    child.wait_with_output().unwrap()
}

#[tokio::test]
async fn prop_synthetic_patch_matches_git_apply_across_directions() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();
    let mut rng = SplitMix64(0x51a7_3301_cafe_babe);

    std::fs::write(dir.path().join("target.txt"), b"seed\n").unwrap();
    run_git(dir.path(), &["add", "target.txt"]);
    run_git(dir.path(), &["commit", "-m", "seed"]);

    let target_rel = RepoRelPath::normalize("target.txt").unwrap();

    for case_idx in 0..160 {
        let crlf = case_idx % 3 == 1;
        let old_bytes = if case_idx % 20 == 0 {
            Vec::new()
        } else {
            gen_text_from_lines(&mut rng, crlf, true)
        };
        let mut new_bytes = if case_idx % 25 == 2 {
            Vec::new()
        } else {
            mutate_doc_lines(&mut rng, &old_bytes, crlf, true)
        };
        if old_bytes == new_bytes {
            new_bytes.extend_from_slice(if crlf {
                b"delta_end\r\n"
            } else {
                b"delta_end\n"
            });
        }

        let status = if old_bytes.is_empty() {
            DeltaStatus::Added
        } else if new_bytes.is_empty() {
            DeltaStatus::Deleted
        } else {
            DeltaStatus::Modified
        };
        let ctx_lines = match case_idx % 4 {
            0 => 0,
            1 => 1,
            _ => 3,
        };
        let opts = DiffOptions {
            algorithm: if case_idx % 2 == 0 {
                diff_merge::DiffAlgorithmKind::Histogram
            } else {
                diff_merge::DiffAlgorithmKind::Myers
            },
            context_lines: ctx_lines,
            ignore_whitespace: false,
            respect_autocrlf: false,
        };
        let delta = make_delta("target.txt", status, DeltaArea::Unstaged);
        let patch = dm.diff_blobs(delta, &old_bytes, &new_bytes, &opts).unwrap();
        if patch.hunks.is_empty() {
            continue;
        }

        // Full selection across all hunks must reconstruct `new_bytes` (forward) and `old_bytes` (reverse).
        let full_specs: Vec<LineSelectionSpec> = patch
            .hunks
            .iter()
            .map(|h| LineSelectionSpec {
                hunk_index: h.hunk_index,
                selected_line_indices: h
                    .lines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, l)| {
                        matches!(
                            l.origin,
                            DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                        )
                        .then_some(i)
                    })
                    .collect(),
            })
            .collect();

        // Prepare index = old_bytes (or removed if old_bytes.is_empty()).
        if old_bytes.is_empty() {
            let _ = std::fs::remove_file(dir.path().join("target.txt"));
            run_git(dir.path(), &["rm", "-f", "--ignore-unmatch", "target.txt"]);
        } else {
            std::fs::write(dir.path().join("target.txt"), &old_bytes).unwrap();
            run_git(dir.path(), &["add", "target.txt"]);
        }

        let cached_apply_args: &[&str] = if ctx_lines == 0 {
            &["apply", "--cached", "--unidiff-zero"]
        } else {
            &["apply", "--cached"]
        };
        let worktree_apply_args: &[&str] = if ctx_lines == 0 {
            &["apply", "--unidiff-zero"]
        } else {
            &["apply"]
        };

        let fwd_full = dm
            .build_line_selection_patch(&patch, &full_specs, PatchDirection::StageToIndex)
            .unwrap();
        let res_fwd = run_git_with_stdin(dir.path(), cached_apply_args, &fwd_full);
        assert!(
            res_fwd.status.success(),
            "case {case_idx} full forward failed:\npatch:\n{}\nstderr: {}",
            String::from_utf8_lossy(&fwd_full),
            String::from_utf8_lossy(&res_fwd.stderr)
        );
        if new_bytes.is_empty() {
            let staged_exists = Command::new("git")
                .current_dir(dir.path())
                .args(["cat-file", "-e", ":target.txt"])
                .output()
                .unwrap()
                .status
                .success();
            assert!(
                !staged_exists,
                "case {case_idx}: expected deleted file in index"
            );
        } else {
            assert_eq!(
                run_git(dir.path(), &["cat-file", "blob", ":target.txt"]),
                new_bytes,
                "case {case_idx} full forward byte mismatch"
            );
        }

        // Now reverse UnstageFromIndex from `new_bytes` back to `old_bytes` via `git apply --cached`.
        let rev_full = dm
            .build_line_selection_patch(&patch, &full_specs, PatchDirection::UnstageFromIndex)
            .unwrap();
        let res_rev = run_git_with_stdin(dir.path(), cached_apply_args, &rev_full);
        assert!(
            res_rev.status.success(),
            "case {case_idx} full reverse failed:\npatch:\n{}\nstderr: {}",
            String::from_utf8_lossy(&rev_full),
            String::from_utf8_lossy(&res_rev.stderr)
        );
        if old_bytes.is_empty() {
            let staged_exists = Command::new("git")
                .current_dir(dir.path())
                .args(["cat-file", "-e", ":target.txt"])
                .output()
                .unwrap()
                .status
                .success();
            assert!(
                !staged_exists,
                "case {case_idx}: expected reverted Added file absent"
            );
        } else {
            assert_eq!(
                run_git(dir.path(), &["cat-file", "blob", ":target.txt"]),
                old_bytes,
                "case {case_idx} full reverse byte mismatch"
            );
        }

        // Pick a random non-empty subset of actionable lines across hunks and test BOTH `git apply` and `git2`.
        let mut partial_specs = Vec::new();
        for h in &patch.hunks {
            let actionable: Vec<usize> = h
                .lines
                .iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    matches!(
                        l.origin,
                        DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                    )
                    .then_some(i)
                })
                .collect();
            let mut picked = BTreeSet::new();
            for &idx in &actionable {
                if rng.gen_bool(55) {
                    picked.insert(idx);
                }
            }
            if !picked.is_empty() {
                partial_specs.push(LineSelectionSpec {
                    hunk_index: h.hunk_index,
                    selected_line_indices: picked,
                });
            }
        }
        if partial_specs.is_empty() {
            let first_hunk = &patch.hunks[0];
            let first_act = first_hunk
                .lines
                .iter()
                .position(|l| {
                    matches!(
                        l.origin,
                        DiffLineOrigin::Addition | DiffLineOrigin::Deletion
                    )
                })
                .unwrap();
            partial_specs.push(LineSelectionSpec {
                hunk_index: first_hunk.hunk_index,
                selected_line_indices: BTreeSet::from([first_act]),
            });
        }

        // Partial forward StageToIndex with git CLI & git2.
        let part_fwd = dm
            .build_line_selection_patch(&patch, &partial_specs, PatchDirection::StageToIndex)
            .unwrap();
        let res_pf = run_git_with_stdin(dir.path(), cached_apply_args, &part_fwd);
        assert!(
            res_pf.status.success(),
            "case {case_idx} partial forward failed:\nold={:?}\nnew={:?}\npatch:\n{}\nstderr: {}",
            String::from_utf8_lossy(&old_bytes),
            String::from_utf8_lossy(&new_bytes),
            String::from_utf8_lossy(&part_fwd),
            String::from_utf8_lossy(&res_pf.stderr)
        );
        if case_idx % 5 == 0 {
            if old_bytes.is_empty() {
                run_git(dir.path(), &["rm", "-f", "--ignore-unmatch", "target.txt"]);
            } else {
                std::fs::write(dir.path().join("target.txt"), &old_bytes).unwrap();
                run_git(dir.path(), &["add", "target.txt"]);
            }
            git_engine
                .apply_synthetic_patch_to_index(&root, &part_fwd)
                .await
                .unwrap();
        }

        // Partial reverse UnstageFromIndex (starting index at new_bytes) & DiscardInWorktree (starting worktree at new_bytes).
        if new_bytes.is_empty() {
            let _ = std::fs::remove_file(dir.path().join("target.txt"));
            run_git(dir.path(), &["rm", "-f", "--ignore-unmatch", "target.txt"]);
        } else {
            std::fs::write(dir.path().join("target.txt"), &new_bytes).unwrap();
            run_git(dir.path(), &["add", "target.txt"]);
        }
        let part_rev = dm
            .build_line_selection_patch(&patch, &partial_specs, PatchDirection::UnstageFromIndex)
            .unwrap();
        let res_pr = run_git_with_stdin(dir.path(), cached_apply_args, &part_rev);
        assert!(
            res_pr.status.success(),
            "case {case_idx} partial unstage failed:\nold={:?}\nnew={:?}\npatch:\n{}\nstderr: {}",
            String::from_utf8_lossy(&old_bytes),
            String::from_utf8_lossy(&new_bytes),
            String::from_utf8_lossy(&part_rev),
            String::from_utf8_lossy(&res_pr.stderr)
        );

        let part_discard = dm
            .build_line_selection_patch(&patch, &partial_specs, PatchDirection::DiscardInWorktree)
            .unwrap();
        if new_bytes.is_empty() {
            let _ = std::fs::remove_file(dir.path().join("target.txt"));
        } else {
            std::fs::write(dir.path().join("target.txt"), &new_bytes).unwrap();
        }
        let res_pd = run_git_with_stdin(dir.path(), worktree_apply_args, &part_discard);
        assert!(
            res_pd.status.success(),
            "case {case_idx} partial discard failed:\npatch:\n{}\nstderr: {}",
            String::from_utf8_lossy(&part_discard),
            String::from_utf8_lossy(&res_pd.stderr)
        );
        let _ = &target_rel;
    }
}

fn build_distinct_base_doc(
    rng: &mut SplitMix64,
    line_count: usize,
    crlf: bool,
    allow_no_nl: bool,
) -> Vec<u8> {
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    for i in 0..line_count {
        s.push_str(&format!("base_line_{i}"));
        append_line_eol(&mut s, eol, i + 1 == line_count, allow_no_nl, rng);
    }
    s.into_bytes()
}

#[test]
fn prop_three_way_merge_matches_git_merge_file_diff3() {
    let dir = TempDir::new().unwrap();
    let dm = BranvioDiffMergeEngine::new();
    let target = RepoRelPath::normalize("merged.txt").unwrap();
    let mut rng = SplitMix64(0x9876_5432_dead_beef);

    let base_p = dir.path().join("base.txt");
    let ours_p = dir.path().join("ours.txt");
    let theirs_p = dir.path().join("theirs.txt");

    let run_git_merge_file = |base_bytes: &[u8], ours_bytes: &[u8], theirs_bytes: &[u8]| {
        std::fs::write(&base_p, base_bytes).unwrap();
        std::fs::write(&ours_p, ours_bytes).unwrap();
        std::fs::write(&theirs_p, theirs_bytes).unwrap();
        let git_out = Command::new("git")
            .args([
                "merge-file",
                "-p",
                "--diff3",
                "-L",
                "HEAD",
                "-L",
                "base",
                "-L",
                "topic",
            ])
            .arg(&ours_p)
            .arg(&base_p)
            .arg(&theirs_p)
            .output()
            .expect("git merge-file should run");
        assert!(
            matches!(git_out.status.code(), Some(0..=127)),
            "git merge-file errored (code={:?}): {}",
            git_out.status.code(),
            String::from_utf8_lossy(&git_out.stderr)
        );
        git_out
    };

    let resolve_all_and_verify = |doc: &mut diff_merge::ThreeWayMergeDocument| {
        let region_ids: Vec<u32> = doc.regions.iter().map(|r| r.region_id).collect();
        for (idx, rid) in region_ids.into_iter().enumerate() {
            let choice = match idx % 4 {
                0 => ConflictResolution::TakeOurs,
                1 => ConflictResolution::TakeTheirs,
                2 => ConflictResolution::TakeBase,
                _ => ConflictResolution::TakeBothOursFirst,
            };
            dm.resolve_region(doc, rid, choice).unwrap();
        }
        assert!(dm.export_merged_bytes(doc, true).is_ok());
    };

    // Sub-phase A: 200 randomized triples with distinct base lines (unique LCS).
    // Verifies 100% byte-for-byte parity against `git merge-file -p --diff3`.
    for case_idx in 0..200 {
        let crlf = case_idx % 4 == 1;
        let base_bytes = if crlf {
            let line_count = 1 + rng.gen_range(15);
            build_distinct_base_doc(&mut rng, line_count, true, line_count > 1)
        } else if case_idx % 25 == 0 {
            Vec::new()
        } else {
            let line_count = rng.gen_range(16);
            build_distinct_base_doc(&mut rng, line_count, false, true)
        };
        let ours_bytes = mutate_doc_lines(&mut rng, &base_bytes, crlf, true);
        let theirs_bytes = if case_idx % 12 == 0 {
            ours_bytes.clone()
        } else {
            mutate_doc_lines(&mut rng, &base_bytes, crlf, true)
        };

        let git_out = run_git_merge_file(&base_bytes, &ours_bytes, &theirs_bytes);
        let mut doc = dm
            .open_conflicted_file(ConflictedFileInputs {
                path: target.clone(),
                base_bytes: Some(&base_bytes),
                ours_bytes: &ours_bytes,
                theirs_bytes: &theirs_bytes,
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        assert_eq!(
            git_out.status.code() == Some(0),
            doc.regions.is_empty(),
            "sub-phase A case {case_idx}: conflict count parity mismatch (git code={:?}, doc regions={})",
            git_out.status.code(),
            doc.regions.len()
        );
        let branvio_bytes = dm.export_merged_bytes(&doc, false).unwrap();

        assert_eq!(
            branvio_bytes,
            git_out.stdout,
            "sub-phase A case {case_idx} (crlf={crlf}) diverged from git merge-file --diff3!\nbase={:?}\nours={:?}\ntheirs={:?}\nbranvio:\n{}\ngit:\n{}",
            String::from_utf8_lossy(&base_bytes),
            String::from_utf8_lossy(&ours_bytes),
            String::from_utf8_lossy(&theirs_bytes),
            String::from_utf8_lossy(&branvio_bytes),
            String::from_utf8_lossy(&git_out.stdout)
        );

        if !doc.regions.is_empty() {
            let mut wt_doc = dm
                .open_conflicted_file(ConflictedFileInputs {
                    path: target.clone(),
                    base_bytes: Some(&base_bytes),
                    ours_bytes: &ours_bytes,
                    theirs_bytes: &theirs_bytes,
                    worktree_with_markers: Some(&git_out.stdout),
                    ours_label: "HEAD",
                    theirs_label: "topic",
                })
                .unwrap();
            assert_eq!(wt_doc.regions.len(), doc.regions.len());
            resolve_all_and_verify(&mut wt_doc);
            resolve_all_and_verify(&mut doc);
        }
    }

    // Explicit regression check matching C Git's Case 10 downward EOF blank-line slider placement.
    let case10_base =
        b"u_10\ndelta\ndelta\n\nzeta\nline_k\ndelta\nbeta\n\nline_k\ndelta\nalpha\n\n\n";
    let case10_ours = b"delta\n\nzeta\nline_k\ndelta\nbeta\n\nline_k\ndelta\nalpha\n\n";
    let case10_theirs =
        b"u_10\nins_0\ndelta\ndelta_m0\n\nline_k\ndelta\n\ndelta\nalpha\n\n\nins_0\n";
    let case10_git = run_git_merge_file(case10_base, case10_ours, case10_theirs);
    let case10_doc = dm
        .open_conflicted_file(ConflictedFileInputs {
            path: target.clone(),
            base_bytes: Some(case10_base),
            ours_bytes: case10_ours,
            theirs_bytes: case10_theirs,
            worktree_with_markers: None,
            ours_label: "HEAD",
            theirs_label: "topic",
        })
        .unwrap();
    assert_eq!(
        dm.export_merged_bytes(&case10_doc, false).unwrap(),
        case10_git.stdout
    );

    // Sub-phase B: 150 duplicate-line & blank-line stress cases.
    // Verifies clean merges, worktree marker parsing on `git merge-file` output, and clean exports.
    for case_idx in 0..150 {
        let crlf = case_idx % 4 == 1;
        let base_bytes = if crlf {
            let line_count = 1 + rng.gen_range(15);
            build_seeded_doc(&mut rng, line_count, true, line_count > 1)
        } else if case_idx % 25 == 0 {
            Vec::new()
        } else {
            let line_count = rng.gen_range(16);
            build_seeded_doc(&mut rng, line_count, false, true)
        };
        let ours_bytes = mutate_doc_lines(&mut rng, &base_bytes, crlf, true);
        let theirs_bytes = if case_idx % 12 == 0 {
            ours_bytes.clone()
        } else {
            mutate_doc_lines(&mut rng, &base_bytes, crlf, true)
        };

        let git_out = run_git_merge_file(&base_bytes, &ours_bytes, &theirs_bytes);
        let mut doc = dm
            .open_conflicted_file(ConflictedFileInputs {
                path: target.clone(),
                base_bytes: Some(&base_bytes),
                ours_bytes: &ours_bytes,
                theirs_bytes: &theirs_bytes,
                worktree_with_markers: None,
                ours_label: "HEAD",
                theirs_label: "topic",
            })
            .unwrap();
        let branvio_bytes = dm.export_merged_bytes(&doc, false).unwrap();

        let single_side_or_both_clean = ours_bytes == theirs_bytes
            || ours_bytes == base_bytes
            || theirs_bytes == base_bytes
            || (git_out.status.code() == Some(0) && doc.regions.is_empty());
        if single_side_or_both_clean {
            assert_eq!(
                branvio_bytes, git_out.stdout,
                "sub-phase B clean/single-side case {case_idx} diverged"
            );
        }

        if let Some(code) = git_out.status.code()
            && code > 0
        {
            let mut wt_doc = dm
                .open_conflicted_file(ConflictedFileInputs {
                    path: target.clone(),
                    base_bytes: Some(&base_bytes),
                    ours_bytes: &ours_bytes,
                    theirs_bytes: &theirs_bytes,
                    worktree_with_markers: Some(&git_out.stdout),
                    ours_label: "HEAD",
                    theirs_label: "topic",
                })
                .unwrap();
            assert_eq!(wt_doc.regions.len(), code as usize);
            resolve_all_and_verify(&mut wt_doc);
        }
        resolve_all_and_verify(&mut doc);
    }
}

#[tokio::test]
async fn supports_special_paths_mode_transitions_whitespace_context_and_non_utf8_tier2_fallback() {
    let (dir, root, git_engine) = init_repo();
    let dm = BranvioDiffMergeEngine::new();

    // 1. C-quoted path, executable mode change, and whitespace/autocrlf context.
    #[cfg(unix)]
    let special_raw = "sub/tab\there\"quote\nline_café.txt";
    #[cfg(not(unix))]
    let special_raw = "sub/space_utf8_café.txt";
    let special_rel = RepoRelPath::normalize(special_raw).unwrap();
    let sp_path = dir.path().join(special_rel.as_str());
    std::fs::create_dir_all(sp_path.parent().unwrap()).unwrap();
    let old_ws = b"keep   indent\nold line\nend line\n";
    let new_ws = b"keep indent\r\nnew line\r\nend line\r\n";

    std::fs::write(&sp_path, old_ws).unwrap();
    run_git(dir.path(), &["add", "--", special_rel.as_str()]);
    run_git(dir.path(), &["commit", "-m", "add special"]);

    std::fs::write(&sp_path, new_ws).unwrap();
    let sp_delta = FileDelta {
        old_path: None,
        new_path: special_rel.clone(),
        status: DeltaStatus::Modified,
        area: DeltaArea::Unstaged,
        old_mode: FileMode::Regular644,
        new_mode: FileMode::Executable755,
        old_oid: None,
        new_oid: None,
        is_binary: false,
        lfs: None,
        eol_change: EolChange::Unchanged,
        additions: 0,
        deletions: 0,
    };
    let ws_patch = dm
        .diff_blobs(
            sp_delta,
            old_ws,
            new_ws,
            &DiffOptions {
                ignore_whitespace: true,
                respect_autocrlf: true,
                ..DiffOptions::default()
            },
        )
        .unwrap();

    // Full patch includes C-escaped path, `old mode 100644\nnew mode 100755`, and old context bytes (`keep   indent\n`).
    let full_stage = dm
        .build_hunk_patch(&ws_patch, 0, PatchDirection::StageToIndex)
        .unwrap();
    let apply_res = run_git_with_stdin(dir.path(), &["apply", "--cached"], &full_stage);
    assert!(
        apply_res.status.success(),
        "C-quoted special path apply failed: {}",
        String::from_utf8_lossy(&apply_res.stderr)
    );
    let ls_s = String::from_utf8(run_git(
        dir.path(),
        &["ls-files", "-s", "--", special_rel.as_str()],
    ))
    .unwrap();
    assert!(
        ls_s.starts_with("100755 "),
        "mode should now be 100755: {ls_s}"
    );

    // Reverse DiscardInWorktree uses `new`'s context (`keep indent\r\n` + `end line\r\n`) cleanly!
    let discard_ws = dm
        .build_hunk_patch(&ws_patch, 0, PatchDirection::DiscardInWorktree)
        .unwrap();
    let discard_res = run_git_with_stdin(dir.path(), &["apply"], &discard_ws);
    assert!(
        discard_res.status.success(),
        "DiscardInWorktree with whitespace/autocrlf context failed: {}",
        String::from_utf8_lossy(&discard_res.stderr)
    );

    // 2. Non-UTF-8 file: diff-merge rejects with UnsupportedEncoding while Tier 2 stage/unstage works losslessly!
    let latin1_rel = RepoRelPath::normalize("latin1.txt").unwrap();
    let latin1_bytes = b"caf\xe9\n";
    std::fs::write(dir.path().join("latin1.txt"), latin1_bytes).unwrap();
    let diff_err = dm
        .diff_blobs(
            make_delta("latin1.txt", DeltaStatus::Added, DeltaArea::Unstaged),
            b"",
            latin1_bytes,
            &DiffOptions::default(),
        )
        .unwrap_err();
    assert!(matches!(
        diff_err,
        diff_merge::DiffMergeError::UnsupportedEncoding { .. }
    ));

    git_engine
        .stage_files(&root, std::slice::from_ref(&latin1_rel))
        .await
        .unwrap();
    assert_eq!(
        run_git(dir.path(), &["cat-file", "blob", ":latin1.txt"]),
        latin1_bytes
    );
    git_engine
        .unstage_files(&root, std::slice::from_ref(&latin1_rel))
        .await
        .unwrap();
    let staged_after = run_git(dir.path(), &["diff", "--cached", "--name-only"]);
    assert!(!String::from_utf8_lossy(&staged_after).contains("latin1.txt"));
}
