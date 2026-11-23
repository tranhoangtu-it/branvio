//! Unified and Side-by-Side Split Diff Viewer with `tree-sitter` syntax highlighting,
//! word-level inline diff backgrounds, enclosing AST symbol headers, and line selection.

use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{to_hsla, to_rgba, ui_button, ui_pill};
use crate::ports::DiffViewMode;
use crate::presenter::{DiffPreviewNotice, RawConflictSide};
use crate::theme::BranvioTheme;
use branvio_core::domain::{
    DeltaArea, DiffHunk, DiffLine, DiffLineOrigin, EolChange, FileDelta, FileDiffPatch,
};
use branvio_core::paths::RepoRelPath;
use diff_merge::PatchDirection;
use gpui::{
    AnyElement, Context, HighlightStyle, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, StyledText, Window, div, px,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// Renders either Unified or Side-by-Side Diff View for the active tab's selected delta.
pub fn render_diff_pane(
    view: &WorkspaceView,
    window: &mut Window,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let theme = view.presenter.theme;
    let diff_mode = view.presenter.diff_view_mode;

    let Some(tab) = view.presenter.active_tab() else {
        return render_empty_diff(&theme, "No active repository");
    };

    if let Some(ref notice) = tab.diff_notice {
        return match notice {
            DiffPreviewNotice::Message(msg) => render_empty_diff(&theme, msg),
            DiffPreviewNotice::UnsupportedEncoding {
                path,
                conflict_labels,
            } => render_unsupported_encoding_card(
                path,
                conflict_labels.as_ref(),
                tab.selected_delta.as_ref(),
                &theme,
                cx,
            ),
        };
    }
    let Some(ref patch) = tab.active_diff_patch else {
        return render_empty_diff(
            &theme,
            "Select any changed file to inspect diff & stage lines",
        );
    };

    let mut total_add = 0usize;
    let mut total_del = 0usize;
    for hunk in &patch.hunks {
        for line in &hunk.lines {
            match line.origin {
                DiffLineOrigin::Addition => total_add += 1,
                DiffLineOrigin::Deletion => total_del += 1,
                _ => {}
            }
        }
    }

    let header = render_diff_file_header(patch, total_add, total_del, diff_mode, &theme, cx);

    let mut body = div()
        .id("diff-scroll-area")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_y_scroll()
        .p(px(10.0))
        .gap(px(12.0));

    let side_by_side = diff_mode == DiffViewMode::SideBySideDiff;
    let patch_clone = patch;
    let selected_map = &tab.selected_diff_lines;

    for hunk in &patch_clone.hunks {
        body = body.child(render_hunk_card(
            patch_clone,
            hunk,
            selected_map,
            side_by_side,
            &theme,
            window,
            cx,
        ));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .bg(to_rgba(theme.bg_base))
        .child(header)
        .child(body)
        .into_any_element()
}

fn render_empty_diff(theme: &BranvioTheme, message: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .items_center()
        .justify_center()
        .bg(to_rgba(theme.bg_base))
        .text_size(px(12.5))
        .text_color(to_rgba(theme.text_muted))
        .child(message.to_string())
        .into_any_element()
}

fn render_unsupported_encoding_card(
    path: &RepoRelPath,
    conflict_labels: Option<&(String, String)>,
    selected_delta: Option<&FileDelta>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let mut actions = div().flex().items_center().gap(px(8.0));

    if let Some((ours_label, theirs_label)) = conflict_labels {
        let ours_path = path.clone();
        let theirs_path = path.clone();
        actions = actions
            .child(
                ui_button(
                    "nonutf8-accept-ours",
                    &format!("Accept {ours_label} (Raw & Stage)"),
                    theme,
                    true,
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter.resolve_non_utf8_conflict_side(
                        ours_path.clone(),
                        RawConflictSide::Ours,
                        true,
                    );
                    cx.notify();
                })),
            )
            .child(
                ui_button(
                    "nonutf8-accept-theirs",
                    &format!("Accept {theirs_label} (Raw & Stage)"),
                    theme,
                    false,
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter.resolve_non_utf8_conflict_side(
                        theirs_path.clone(),
                        RawConflictSide::Theirs,
                        true,
                    );
                    cx.notify();
                })),
            );
    } else {
        match selected_delta.map(|d| d.area) {
            Some(DeltaArea::Unstaged) => {
                let stage_path = path.clone();
                let delta_discard = selected_delta.cloned();
                actions = actions.child(
                    ui_button("nonutf8-stage-whole", "Stage Whole File", theme, true).on_click(
                        cx.listener(move |this, _ev, _w, cx| {
                            this.presenter.stage_paths(vec![stage_path.clone()]);
                            cx.notify();
                        }),
                    ),
                );
                if let Some(d) = delta_discard {
                    actions = actions.child(
                        ui_button("nonutf8-discard-whole", "Discard Changes", theme, false)
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.presenter
                                    .discard_worktree_deltas(std::slice::from_ref(&d));
                                cx.notify();
                            })),
                    );
                }
            }
            Some(DeltaArea::Staged) => {
                let unstage_path = path.clone();
                actions = actions.child(
                    ui_button("nonutf8-unstage-whole", "Unstage Whole File", theme, true).on_click(
                        cx.listener(move |this, _ev, _w, cx| {
                            this.presenter.unstage_paths(vec![unstage_path.clone()]);
                            cx.notify();
                        }),
                    ),
                );
            }
            _ => {
                actions = actions.child(ui_pill(
                    "Committed non-UTF-8 blob (read-only)",
                    theme.bg_elevated,
                    theme.text_secondary,
                ));
            }
        }
    }

    actions = actions.child(
        ui_button("nonutf8-close-btn", "Close", theme, false).on_click(cx.listener(
            |this, _ev, _w, cx| {
                if let Some(tab) = this.presenter.active_tab_mut() {
                    tab.selected_delta = None;
                    tab.active_diff_patch = None;
                    tab.diff_notice = None;
                }
                cx.notify();
            },
        )),
    );

    let mode_text = if conflict_labels.is_some() {
        "Non-UTF-8 encoding detected in one or more conflict stages. Line-based 3-way merge is disabled to avoid corrupting non-UTF-8 bytes; choose either version wholesale as raw bytes."
    } else {
        "Non-UTF-8 encoding detected. Line-level partial staging and inline text diff are disabled so raw file bytes are never altered with UTF-8 replacement characters."
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .items_center()
        .justify_center()
        .bg(to_rgba(theme.bg_base))
        .p(px(20.0))
        .child(
            div()
                .max_w(px(560.0))
                .p(px(16.0))
                .rounded(px(8.0))
                .bg(to_rgba(theme.bg_surface))
                .border_1()
                .border_color(to_rgba(theme.warning))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(ui_pill(
                            "NON-UTF-8 ENCODING",
                            theme.bg_elevated,
                            theme.warning,
                        ))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(to_rgba(theme.text_primary))
                                .child(path.as_str().to_string()),
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(to_rgba(theme.text_secondary))
                        .child(mode_text.to_string()),
                )
                .child(actions),
        )
        .into_any_element()
}

fn render_diff_file_header(
    patch: &FileDiffPatch,
    additions: usize,
    deletions: usize,
    diff_mode: DiffViewMode,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let (badge_glyph, badge_col) = theme.delta_status_badge(patch.delta.status);
    let path_label = match &patch.delta.old_path {
        Some(old) => format!("{} -> {}", old.as_str(), patch.delta.new_path.as_str()),
        None => patch.delta.new_path.as_str().to_string(),
    };
    let eol_note = match patch.delta.eol_change {
        EolChange::Unchanged => None,
        EolChange::LfToCrlf => Some("EOL: LF -> CRLF"),
        EolChange::CrlfToLf => Some("EOL: CRLF -> LF"),
        EolChange::MixedLineEndings => Some("EOL: Mixed"),
    };
    let mode_changed = patch.delta.old_mode != patch.delta.new_mode;

    let mut left = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(ui_pill(badge_glyph, theme.bg_elevated, badge_col))
        .child(
            div()
                .text_size(px(12.5))
                .text_color(to_rgba(theme.text_primary))
                .child(path_label),
        )
        .child(ui_pill(
            &format!("+{additions}"),
            theme.added_bg,
            theme.added,
        ))
        .child(ui_pill(
            &format!("-{deletions}"),
            theme.deleted_bg,
            theme.deleted,
        ));

    if mode_changed {
        left = left.child(ui_pill(
            &format!("{:?} -> {:?}", patch.delta.old_mode, patch.delta.new_mode),
            theme.bg_elevated,
            theme.warning,
        ));
    }
    if let Some(note) = eol_note {
        left = left.child(ui_pill(note, theme.bg_elevated, theme.text_secondary));
    }

    let is_split = diff_mode == DiffViewMode::SideBySideDiff;
    let right = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            ui_button("diff-mode-unified", "Unified", theme, !is_split).on_click(cx.listener(
                |this, _ev, _w, cx| {
                    this.presenter.diff_view_mode = DiffViewMode::UnifiedDiff;
                    cx.notify();
                },
            )),
        )
        .child(
            ui_button("diff-mode-split", "Side-by-Side", theme, is_split).on_click(cx.listener(
                |this, _ev, _w, cx| {
                    this.presenter.diff_view_mode = DiffViewMode::SideBySideDiff;
                    cx.notify();
                },
            )),
        )
        .child(
            ui_button("diff-close-btn", "Close Diff", theme, false).on_click(cx.listener(
                |this, _ev, _w, cx| {
                    if let Some(tab) = this.presenter.active_tab_mut() {
                        tab.selected_delta = None;
                        tab.active_diff_patch = None;
                        tab.diff_notice = None;
                    }
                    cx.notify();
                },
            )),
        );

    div()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .bg(to_rgba(theme.bg_surface))
        .border_b_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(left)
        .child(right)
}

#[allow(clippy::too_many_arguments)]
fn render_hunk_card(
    patch: &FileDiffPatch,
    hunk: &DiffHunk,
    selected_lines_map: &BTreeMap<u32, BTreeSet<usize>>,
    side_by_side: bool,
    theme: &BranvioTheme,
    window: &Window,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let hunk_idx = hunk.hunk_index;
    let selected_set = selected_lines_map.get(&hunk_idx);
    let selected_count = selected_set.map_or(0, BTreeSet::len);
    let area = patch.delta.area;

    let symbol_label = hunk
        .enclosing_symbol
        .as_ref()
        .map(|s| format!("  {}", s.signature))
        .unwrap_or_default();

    let mut actions_row = div().flex().items_center().gap(px(6.0));
    match area {
        DeltaArea::Unstaged => {
            let stage_label = if selected_count > 0 {
                format!("Stage Selected ({selected_count})")
            } else {
                "Stage Hunk".to_string()
            };
            let discard_label = if selected_count > 0 {
                format!("Discard ({selected_count})")
            } else {
                "Discard Hunk".to_string()
            };
            actions_row = actions_row
                .child(
                    ui_button(("hunk-stage", hunk_idx as usize), &stage_label, theme, true)
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            this.presenter.apply_hunk_or_selected_lines(
                                hunk_idx,
                                PatchDirection::StageToIndex,
                            );
                            cx.notify();
                        })),
                )
                .child(
                    ui_button(
                        ("hunk-discard", hunk_idx as usize),
                        &discard_label,
                        theme,
                        false,
                    )
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.apply_hunk_or_selected_lines(
                            hunk_idx,
                            PatchDirection::DiscardInWorktree,
                        );
                        cx.notify();
                    })),
                );
        }
        DeltaArea::Staged => {
            let unstage_label = if selected_count > 0 {
                format!("Unstage Selected ({selected_count})")
            } else {
                "Unstage Hunk".to_string()
            };
            actions_row = actions_row.child(
                ui_button(
                    ("hunk-unstage", hunk_idx as usize),
                    &unstage_label,
                    theme,
                    false,
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter
                        .apply_hunk_or_selected_lines(hunk_idx, PatchDirection::UnstageFromIndex);
                    cx.notify();
                })),
            );
        }
        DeltaArea::Conflicted | DeltaArea::CommitDiff => {}
    }

    let hunk_bar = div()
        .px(px(10.0))
        .py(px(6.0))
        .flex()
        .items_center()
        .justify_between()
        .bg(to_rgba(theme.bg_surface))
        .border_b_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(
            div()
                .flex()
                .items_center()
                .text_size(px(11.5))
                .text_color(to_rgba(theme.accent_primary))
                .child(format!("{}{symbol_label}", hunk.header_raw)),
        )
        .child(actions_row);

    let mut card = div()
        .flex()
        .flex_col()
        .rounded(px(6.0))
        .overflow_hidden()
        .border_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(hunk_bar);

    let default_style = window.text_style();
    if side_by_side {
        card = card.child(render_hunk_side_by_side(
            patch,
            hunk,
            selected_set,
            area,
            theme,
            &default_style,
            cx,
        ));
    } else {
        let mut lines_col = div().flex().flex_col().w_full();
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            let is_checked = selected_set.is_some_and(|s| s.contains(&line_idx));
            lines_col = lines_col.child(render_unified_line(
                hunk_idx,
                line_idx,
                patch,
                line,
                is_checked,
                matches!(area, DeltaArea::Unstaged | DeltaArea::Staged),
                theme,
                &default_style,
                cx,
            ));
        }
        card = card.child(lines_col);
    }

    card
}

#[allow(clippy::too_many_arguments)]
fn render_unified_line(
    hunk_idx: u32,
    line_idx: usize,
    patch: &FileDiffPatch,
    line: &DiffLine,
    is_checked: bool,
    interactive_checkbox: bool,
    theme: &BranvioTheme,
    text_style: &gpui::TextStyle,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let (bg, prefix, word_bg) = match line.origin {
        DiffLineOrigin::Addition => (theme.added_bg, "+", Some(theme.added_word_bg)),
        DiffLineOrigin::Deletion => (theme.deleted_bg, "-", Some(theme.deleted_word_bg)),
        DiffLineOrigin::Context => (theme.bg_base, " ", None),
        DiffLineOrigin::EofNoNewlineOld | DiffLineOrigin::EofNoNewlineNew => {
            (theme.bg_surface, "\\", None)
        }
    };

    let old_no = line
        .old_lineno
        .map_or_else(|| "    ".to_string(), |n| format!("{n:>4}"));
    let new_no = line
        .new_lineno
        .map_or_else(|| "    ".to_string(), |n| format!("{n:>4}"));

    let line_str = patch.line_text(line);
    let can_select = interactive_checkbox
        && matches!(
            line.origin,
            DiffLineOrigin::Addition | DiffLineOrigin::Deletion
        );

    let checkbox = if can_select {
        let mark = if is_checked { "[x]" } else { "[ ]" };
        div()
            .id(("diff-chk", (hunk_idx as usize) * 100_000 + line_idx))
            .w(px(26.0))
            .px(px(2.0))
            .cursor_pointer()
            .text_size(px(11.0))
            .text_color(to_rgba(if is_checked {
                theme.accent_primary
            } else {
                theme.text_muted
            }))
            .on_click(cx.listener(move |this, _ev, _w, cx| {
                this.presenter
                    .toggle_diff_line_selection(hunk_idx, line_idx);
                cx.notify();
            }))
            .child(mark)
            .into_any_element()
    } else {
        div().w(px(26.0)).into_any_element()
    };

    div()
        .w_full()
        .min_h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .bg(to_rgba(bg))
        .text_size(px(12.0))
        .child(checkbox)
        .child(
            div()
                .w(px(76.0))
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_muted))
                .child(format!("{old_no} {new_no}")),
        )
        .child(
            div()
                .w(px(16.0))
                .text_color(to_rgba(theme.text_secondary))
                .child(prefix),
        )
        .child(build_styled_code_text(
            line_str, line, word_bg, theme, text_style,
        ))
}

enum SplitSlot<'a> {
    Context {
        line: &'a DiffLine,
    },
    Pair {
        left: Option<(usize, &'a DiffLine)>,
        right: Option<(usize, &'a DiffLine)>,
    },
}

fn flush_split_queues<'a>(
    rows: &mut Vec<SplitSlot<'a>>,
    dels: &mut Vec<(usize, &'a DiffLine)>,
    adds: &mut Vec<(usize, &'a DiffLine)>,
) {
    let max_len = dels.len().max(adds.len());
    for i in 0..max_len {
        rows.push(SplitSlot::Pair {
            left: dels.get(i).copied(),
            right: adds.get(i).copied(),
        });
    }
    dels.clear();
    adds.clear();
}

fn render_hunk_side_by_side(
    patch: &FileDiffPatch,
    hunk: &DiffHunk,
    selected_set: Option<&BTreeSet<usize>>,
    area: DeltaArea,
    theme: &BranvioTheme,
    text_style: &gpui::TextStyle,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let mut rows: Vec<SplitSlot<'_>> = Vec::with_capacity(hunk.lines.len());
    let mut del_queue: Vec<(usize, &DiffLine)> = Vec::new();
    let mut add_queue: Vec<(usize, &DiffLine)> = Vec::new();

    for (idx, line) in hunk.lines.iter().enumerate() {
        match line.origin {
            DiffLineOrigin::Deletion => del_queue.push((idx, line)),
            DiffLineOrigin::Addition => add_queue.push((idx, line)),
            _ => {
                flush_split_queues(&mut rows, &mut del_queue, &mut add_queue);
                rows.push(SplitSlot::Context { line });
            }
        }
    }
    flush_split_queues(&mut rows, &mut del_queue, &mut add_queue);

    let interactive = matches!(area, DeltaArea::Unstaged | DeltaArea::Staged);
    let mut col = div().flex().flex_col().w_full();
    for slot in rows {
        match slot {
            SplitSlot::Context { line } => {
                let left_cell = render_split_half_cell(
                    hunk.hunk_index,
                    None,
                    line.old_lineno,
                    patch.old_line_text(line),
                    line,
                    theme.bg_base,
                    None,
                    false,
                    false,
                    theme,
                    text_style,
                    cx,
                );
                let right_cell = render_split_half_cell(
                    hunk.hunk_index,
                    None,
                    line.new_lineno,
                    patch.line_text(line),
                    line,
                    theme.bg_base,
                    None,
                    false,
                    false,
                    theme,
                    text_style,
                    cx,
                );
                col = col.child(div().w_full().flex().child(left_cell).child(right_cell));
            }
            SplitSlot::Pair { left, right } => {
                let left_cell = match left {
                    Some((idx, line)) => render_split_half_cell(
                        hunk.hunk_index,
                        Some(idx),
                        line.old_lineno,
                        patch.line_text(line),
                        line,
                        theme.deleted_bg,
                        Some(theme.deleted_word_bg),
                        interactive,
                        selected_set.is_some_and(|s| s.contains(&idx)),
                        theme,
                        text_style,
                        cx,
                    ),
                    None => empty_split_cell(theme),
                };
                let right_cell = match right {
                    Some((idx, line)) => render_split_half_cell(
                        hunk.hunk_index,
                        Some(idx),
                        line.new_lineno,
                        patch.line_text(line),
                        line,
                        theme.added_bg,
                        Some(theme.added_word_bg),
                        interactive,
                        selected_set.is_some_and(|s| s.contains(&idx)),
                        theme,
                        text_style,
                        cx,
                    ),
                    None => empty_split_cell(theme),
                };
                col = col.child(div().w_full().flex().child(left_cell).child(right_cell));
            }
        }
    }
    col
}

fn empty_split_cell(theme: &BranvioTheme) -> gpui::Div {
    div()
        .w_1_2()
        .min_h(px(20.0))
        .bg(to_rgba(theme.bg_surface))
        .border_r_1()
        .border_color(to_rgba(theme.border_subtle))
}

#[allow(clippy::too_many_arguments)]
fn render_split_half_cell(
    hunk_idx: u32,
    line_idx_opt: Option<usize>,
    lineno: Option<u32>,
    text: &str,
    line: &DiffLine,
    bg_rgba: u32,
    word_bg_rgba: Option<u32>,
    interactive: bool,
    is_checked: bool,
    theme: &BranvioTheme,
    text_style: &gpui::TextStyle,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let num_str = lineno.map_or_else(|| "    ".to_string(), |n| format!("{n:>4}"));
    let mut cell = div()
        .w_1_2()
        .min_h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .bg(to_rgba(bg_rgba))
        .border_r_1()
        .border_color(to_rgba(theme.border_subtle))
        .text_size(px(12.0));

    if interactive && let Some(line_idx) = line_idx_opt {
        let mark = if is_checked { "[x]" } else { "[ ]" };
        cell = cell.child(
            div()
                .id(("split-chk", (hunk_idx as usize) * 100_000 + line_idx))
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(to_rgba(if is_checked {
                    theme.accent_primary
                } else {
                    theme.text_muted
                }))
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter
                        .toggle_diff_line_selection(hunk_idx, line_idx);
                    cx.notify();
                }))
                .child(mark),
        );
    }

    cell.child(
        div()
            .w(px(36.0))
            .text_size(px(11.0))
            .text_color(to_rgba(theme.text_muted))
            .child(num_str),
    )
    .child(build_styled_code_text(
        text,
        line,
        word_bg_rgba,
        theme,
        text_style,
    ))
}

fn build_styled_code_text(
    line_str: &str,
    line: &DiffLine,
    word_bg_rgba: Option<u32>,
    theme: &BranvioTheme,
    base_style: &gpui::TextStyle,
) -> StyledText {
    if line_str.is_empty() {
        return StyledText::new("");
    }

    let len = line_str.len();
    let mut cuts =
        Vec::with_capacity(2 + line.syntax_tokens.len() * 2 + line.word_highlights.len() * 2);
    cuts.push(0usize);
    cuts.push(len);

    for tok in &line.syntax_tokens {
        let s = (tok.start as usize).min(len);
        let e = (tok.end as usize).min(len);
        if s < e && line_str.is_char_boundary(s) && line_str.is_char_boundary(e) {
            cuts.push(s);
            cuts.push(e);
        }
    }
    for word in &line.word_highlights {
        let s = (word.start as usize).min(len);
        let e = (word.end as usize).min(len);
        if s < e && line_str.is_char_boundary(s) && line_str.is_char_boundary(e) {
            cuts.push(s);
            cuts.push(e);
        }
    }

    cuts.sort_unstable();
    cuts.dedup();

    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for pair in cuts.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if a >= b {
            continue;
        }
        let fg = line
            .syntax_tokens
            .iter()
            .find(|t| (t.start as usize) <= a && b <= (t.end as usize))
            .map(|t| to_hsla(theme.syntax_token_rgba(t.kind)));

        let bg = word_bg_rgba.and_then(|bg_col| {
            line.word_highlights
                .iter()
                .any(|w| (w.start as usize) <= a && b <= (w.end as usize))
                .then(|| to_hsla(bg_col))
        });

        if fg.is_some() || bg.is_some() {
            highlights.push((
                a..b,
                HighlightStyle {
                    color: fg,
                    background_color: bg,
                    ..Default::default()
                },
            ));
        }
    }

    let mut default_text_style = base_style.clone();
    default_text_style.color = to_hsla(theme.text_primary);
    StyledText::new(line_str.to_string()).with_default_highlights(&default_text_style, highlights)
}
