//! Modal overlays for Command Palette (`nucleo`), Interactive Rebase (`git rebase -i`),
//! Drag-Drop Merge/Rebase/Reset with Tier 2 conflict prediction, Branch creation, Worktree creation, and Open Repo.

use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{modal_card_shell, to_rgba, ui_button, ui_pill};
use crate::presenter::{
    ActiveModal, AiFeature, AiUiError, ForcePushChoice, force_push_explanation, force_push_title,
};
use crate::theme::BranvioTheme;
use branvio_core::domain::ResetMode;
use branvio_core::ids::{GitOid, RepoId};
use branvio_core::paths::HostPath;
use git_engine::RebaseAction;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};

/// Renders whichever [`ActiveModal`] overlay is currently open above the main workspace.
pub fn render_active_modal_overlay(
    view: &WorkspaceView,
    cx: &mut Context<'_, WorkspaceView>,
) -> Option<AnyElement> {
    let theme = view.presenter.theme;
    let modal_card: AnyElement = match &view.presenter.modal {
        ActiveModal::None => return None,
        ActiveModal::CommandPalette {
            query,
            selected_index,
        } => render_command_palette_modal(view, query, *selected_index, &theme, cx),
        ActiveModal::InteractiveRebase {
            planner,
            editing_message_row,
            validation_error,
            ..
        } => render_interactive_rebase_modal(
            view,
            planner,
            *editing_message_row,
            validation_error.as_deref(),
            &theme,
            cx,
        ),
        ActiveModal::DropBranchAction {
            preview,
            merge_no_ff,
        } => render_drop_branch_popover(preview, *merge_no_ff, &theme, cx),
        ActiveModal::CreateBranch {
            base_rev,
            checkout_immediately,
            error,
            ..
        } => render_create_branch_modal(
            view,
            base_rev,
            *checkout_immediately,
            error.as_deref(),
            &theme,
            cx,
        ),
        ActiveModal::CreateWorktree {
            create_new_branch,
            error,
            ..
        } => render_create_worktree_modal(view, *create_new_branch, error.as_deref(), &theme, cx),
        ActiveModal::OpenRepository { error, .. } => {
            render_open_repo_modal(view, error.as_deref(), &theme, cx)
        }
        ActiveModal::ConfirmRewritePushedUndo {
            is_redo,
            remote_branches,
        } => render_confirm_pushed_undo_modal(*is_redo, remote_branches, &theme, cx),
        ActiveModal::ConfirmForcePush {
            branch,
            expected_remote_oid,
            ..
        } => render_confirm_force_push_modal(branch, *expected_remote_oid, &theme, cx),
        ActiveModal::ConfirmAiCloudUpload {
            repo_id,
            provider,
            feature,
        } => render_ai_cloud_consent_modal(*repo_id, provider, *feature, &theme, cx),
        ActiveModal::ConfirmDestructiveTerminalCommand {
            command, reason, ..
        } => render_confirm_terminal_command_modal(command, reason, &theme, cx),
        ActiveModal::ConfirmContinueWithoutUndo {
            summary, reason, ..
        } => render_confirm_without_undo_modal(summary, reason, &theme, cx),
        ActiveModal::CreatePullRequest {
            head_branch,
            draft,
            error,
            ..
        } => render_create_pr_modal(view, head_branch, *draft, error.as_deref(), &theme, cx),
    };

    Some(
        div()
            .id("modal-backdrop")
            .absolute()
            .inset_0()
            .bg(to_rgba(0x00000077))
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|this, _ev, _w, cx| {
                this.presenter.modal = ActiveModal::None;
                cx.notify();
            }))
            .child(
                div()
                    .id("modal-surface-stop")
                    .on_click(|_ev, _w, cx| cx.stop_propagation())
                    .child(modal_card),
            )
            .into_any_element(),
    )
}

fn render_command_palette_modal(
    view: &WorkspaceView,
    query: &str,
    selected_index: usize,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let (entries, matches) = view.presenter.search_command_palette(query);
    let mut results_list = div()
        .id("palette-results-scroll")
        .max_h(px(320.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(3.0));

    for (rank_idx, m) in matches.into_iter().enumerate() {
        let entry = &entries[m.entry_index];
        let action = entry.action.clone();
        let is_selected = rank_idx == selected_index;
        let mut row = div()
            .id(("palette-row", rank_idx))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_between()
            .cursor_pointer()
            .bg(to_rgba(if is_selected { theme.bg_selected } else { 0 }))
            .hover(|s| s.bg(to_rgba(theme.bg_hover)))
            .on_click(cx.listener(move |this, _ev, _w, cx| {
                this.presenter.execute_palette_action(action.clone());
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(ui_pill(
                        entry.item.category.as_str(),
                        theme.bg_elevated,
                        theme.accent_primary,
                    ))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(to_rgba(theme.text_primary))
                            .child(entry.item.title.to_string()),
                    ),
            );

        if let Some(ref hint) = entry.item.shortcut_hint {
            row = row.child(ui_pill(
                hint.as_str(),
                theme.bg_elevated,
                theme.text_secondary,
            ));
        }
        results_list = results_list.child(row);
    }

    modal_card_shell(
        px(580.0),
        theme,
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(to_rgba(theme.text_secondary))
                    .child("COMMAND PALETTE (Fuzzy Search)"),
            )
            .child(view.modal_primary_input.clone())
            .child(results_list)
            .into_any_element(),
    )
    .into_any_element()
}

fn next_rebase_action(action: RebaseAction) -> RebaseAction {
    match action {
        RebaseAction::Pick => RebaseAction::Reword,
        RebaseAction::Reword => RebaseAction::Edit,
        RebaseAction::Edit => RebaseAction::Squash,
        RebaseAction::Squash => RebaseAction::Fixup,
        RebaseAction::Fixup => RebaseAction::Drop,
        RebaseAction::Drop => RebaseAction::Pick,
    }
}

fn render_interactive_rebase_modal(
    view: &WorkspaceView,
    planner: &crate::presenter::rebase_planner::InteractiveRebasePlanner,
    editing_message_row: Option<usize>,
    validation_error: Option<&str>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let mut rows_col = div()
        .id("rebase-rows-scroll")
        .max_h(px(320.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(4.0));

    for (idx, row) in planner.rows.iter().enumerate() {
        let next_act = next_rebase_action(row.action);
        let can_reword = matches!(row.action, RebaseAction::Reword | RebaseAction::Squash);
        let action_bg = match row.action {
            RebaseAction::Pick => theme.bg_elevated,
            RebaseAction::Reword => theme.accent_primary,
            RebaseAction::Edit => theme.warning,
            RebaseAction::Squash | RebaseAction::Fixup => theme.accent_secondary,
            RebaseAction::Drop => theme.deleted,
        };
        let action_fg = if row.action == RebaseAction::Pick {
            theme.text_primary
        } else {
            0x0B0F17FF
        };

        let mut line = div()
            .id(("rebase-step", idx))
            .p(px(6.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .bg(to_rgba(theme.bg_base))
            .border_1()
            .border_color(to_rgba(theme.border_subtle))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        ui_button(("rb-act", idx), row.action.todo_keyword(), theme, false)
                            .bg(to_rgba(action_bg))
                            .text_color(to_rgba(action_fg))
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.presenter.update_rebase_row_action(idx, next_act);
                                cx.notify();
                            })),
                    )
                    .child(ui_pill(
                        row.short_sha.as_str(),
                        theme.bg_elevated,
                        theme.text_muted,
                    ))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(to_rgba(theme.text_primary))
                            .child(
                                row.updated_message
                                    .as_deref()
                                    .unwrap_or(&row.original_subject)
                                    .lines()
                                    .next()
                                    .unwrap_or(&row.original_subject)
                                    .to_string(),
                            ),
                    ),
            );

        let mut controls = div().flex().items_center().gap(px(4.0));
        if can_reword {
            controls = controls.child(
                ui_button(
                    ("rb-msg", idx),
                    "Edit Msg",
                    theme,
                    editing_message_row == Some(idx),
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    let draft = if let ActiveModal::InteractiveRebase {
                        ref mut editing_message_row,
                        ref planner,
                        ref mut message_draft,
                        ..
                    } = this.presenter.modal
                    {
                        *editing_message_row = Some(idx);
                        let init = planner
                            .rows
                            .get(idx)
                            .and_then(|r| {
                                r.updated_message
                                    .clone()
                                    .or_else(|| Some(r.original_subject.clone()))
                            })
                            .unwrap_or_default();
                        *message_draft = init.clone();
                        Some(init)
                    } else {
                        None
                    };
                    if let Some(txt) = draft {
                        this.modal_primary_input.update(cx, |input, cx| {
                            input.set_text(txt, cx);
                        });
                    }
                    cx.notify();
                })),
            );
        }
        controls = controls
            .child(
                ui_button(("rb-up", idx), "^", theme, false).on_click(cx.listener(
                    move |this, _ev, _w, cx| {
                        if let ActiveModal::InteractiveRebase {
                            ref mut planner,
                            ref mut validation_error,
                            ..
                        } = this.presenter.modal
                        {
                            planner.move_up(idx);
                            *validation_error = planner.validate().err().map(|e| e.to_string());
                        }
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button(("rb-dn", idx), "v", theme, false).on_click(cx.listener(
                    move |this, _ev, _w, cx| {
                        if let ActiveModal::InteractiveRebase {
                            ref mut planner,
                            ref mut validation_error,
                            ..
                        } = this.presenter.modal
                        {
                            planner.move_down(idx);
                            *validation_error = planner.validate().err().map(|e| e.to_string());
                        }
                        cx.notify();
                    },
                )),
            );

        line = line.child(controls);
        rows_col = rows_col.child(line);
    }

    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(to_rgba(theme.text_primary))
                        .child(format!("Interactive Rebase onto '{}'", planner.onto_rev)),
                )
                .child(ui_pill(
                    "Oldest (top) -> Newest (bottom)",
                    theme.bg_elevated,
                    theme.text_secondary,
                )),
        )
        .child(rows_col);

    if let Some(edit_idx) = editing_message_row {
        card = card
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(to_rgba(theme.accent_primary))
                    .child(format!("Editing Commit Message for Step #{}", edit_idx + 1)),
            )
            .child(view.modal_primary_input.clone())
            .child(
                ui_button("rb-save-msg", "Apply Message Draft", theme, true).on_click(cx.listener(
                    move |this, _ev, _w, cx| {
                        let draft = this.modal_primary_input.read(cx).text().to_string();
                        if let ActiveModal::InteractiveRebase {
                            ref mut planner,
                            ref mut editing_message_row,
                            ..
                        } = this.presenter.modal
                        {
                            planner.set_message(edit_idx, Some(draft));
                            *editing_message_row = None;
                        }
                        cx.notify();
                    },
                )),
            );
    }

    if let Some(err) = validation_error {
        card = card.child(ui_pill(err, theme.conflicted_bg, theme.error));
    }

    let valid = validation_error.is_none() && planner.validate().is_ok();
    card = card.child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(
                ui_button("rb-cancel", "Cancel", theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.modal = ActiveModal::None;
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("rb-run", "Start Rebase", theme, valid).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.execute_active_interactive_rebase();
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(720.0), theme, card.into_any_element()).into_any_element()
}

fn render_drop_branch_popover(
    preview: &crate::ports::DragDropPreviewState,
    merge_no_ff: bool,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let source = preview.source_label.to_string();
    let target = preview
        .target_label
        .as_ref()
        .map_or_else(|| "HEAD".to_string(), ToString::to_string);

    let prediction_box = match &preview.prediction {
        None => div()
            .text_size(px(12.0))
            .text_color(to_rgba(theme.text_muted))
            .child("Running Tier 2 in-memory merge prediction..."),
        Some(pred) if pred.will_succeed_cleanly => {
            div().flex().items_center().gap(px(6.0)).child(ui_pill(
                if pred.is_fast_forward {
                    "Fast-forward merge possible cleanly"
                } else {
                    "Clean merge predicted (0 conflicts)"
                },
                theme.added_bg,
                theme.added,
            ))
        }
        Some(pred) => {
            let files: Vec<String> = pred
                .conflicts
                .iter()
                .take(4)
                .map(|c| c.path.as_str().to_string())
                .collect();
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(ui_pill(
                    &format!(
                        "WARNING: {} predicted conflict(s) in-memory",
                        pred.conflicts.len()
                    ),
                    theme.conflicted_bg,
                    theme.conflicted,
                ))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(to_rgba(theme.text_secondary))
                        .child(files.join(", ")),
                )
        }
    };

    let src_merge = source.clone();
    let src_rebase = source.clone();
    let src_reset = target.clone();

    modal_card_shell(
        px(460.0),
        theme,
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(to_rgba(theme.text_primary))
                    .child(format!("Drop Action: '{source}' -> '{target}'")),
            )
            .child(prediction_box)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.0))
                    .child(
                        ui_button("drop-merge", "Merge", theme, true).on_click(cx.listener(
                            move |this, _ev, _w, cx| {
                                this.presenter.modal = ActiveModal::None;
                                this.presenter
                                    .merge_revision(src_merge.clone(), merge_no_ff);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        ui_button("drop-rebase", "Rebase", theme, false).on_click(cx.listener(
                            move |this, _ev, _w, cx| {
                                this.presenter.modal = ActiveModal::None;
                                this.presenter.rebase_onto(src_rebase.clone());
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        ui_button("drop-reset", "Reset (--mixed)", theme, false).on_click(
                            cx.listener(move |this, _ev, _w, cx| {
                                this.presenter.modal = ActiveModal::None;
                                this.presenter
                                    .reset_to_revision(src_reset.clone(), ResetMode::Mixed);
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        ui_button("drop-cancel", "Cancel", theme, false).on_click(cx.listener(
                            |this, _ev, _w, cx| {
                                this.presenter.modal = ActiveModal::None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .into_any_element(),
    )
    .into_any_element()
}

fn render_create_branch_modal(
    view: &WorkspaceView,
    base_rev: &str,
    checkout_immediately: bool,
    error: Option<&str>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let base_owned = base_rev.to_string();
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(to_rgba(theme.text_primary))
                .child(format!("Create Branch from '{base_rev}'")),
        )
        .child(view.modal_primary_input.clone())
        .child(
            ui_button(
                "cb-checkout-toggle",
                if checkout_immediately {
                    "[x] Checkout branch immediately"
                } else {
                    "[ ] Stay on current branch"
                },
                theme,
                false,
            )
            .on_click(cx.listener(|this, _ev, _w, cx| {
                if let ActiveModal::CreateBranch {
                    ref mut checkout_immediately,
                    ..
                } = this.presenter.modal
                {
                    *checkout_immediately = !*checkout_immediately;
                }
                cx.notify();
            })),
        );

    if let Some(err) = error {
        col = col.child(ui_pill(err, theme.conflicted_bg, theme.error));
    }

    col = col.child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(
                ui_button("cb-cancel", "Cancel", theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.modal = ActiveModal::None;
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("cb-submit", "Create Branch", theme, true).on_click(cx.listener(
                    move |this, _ev, _w, cx| {
                        let name = this.modal_primary_input.read(cx).text().trim().to_string();
                        if name.is_empty() {
                            if let ActiveModal::CreateBranch { ref mut error, .. } =
                                this.presenter.modal
                            {
                                *error = Some("Branch name cannot be empty".to_string());
                            }
                            cx.notify();
                            return;
                        }
                        this.presenter.modal = ActiveModal::None;
                        this.presenter
                            .checkout_revision(base_owned.clone(), Some(name));
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(440.0), theme, col.into_any_element()).into_any_element()
}

fn render_create_worktree_modal(
    view: &WorkspaceView,
    create_new_branch: bool,
    error: Option<&str>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(to_rgba(theme.text_primary))
                .child("Create Linked Git Worktree"),
        )
        .child(view.modal_primary_input.clone())
        .child(view.modal_secondary_input.clone())
        .child(
            ui_button(
                "wt-new-br-toggle",
                if create_new_branch {
                    "[x] Create new branch (-b)"
                } else {
                    "[ ] Checkout existing ref"
                },
                theme,
                false,
            )
            .on_click(cx.listener(|this, _ev, _w, cx| {
                if let ActiveModal::CreateWorktree {
                    ref mut create_new_branch,
                    ..
                } = this.presenter.modal
                {
                    *create_new_branch = !*create_new_branch;
                }
                cx.notify();
            })),
        );

    if let Some(err) = error {
        col = col.child(ui_pill(err, theme.conflicted_bg, theme.error));
    }

    col = col.child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(
                ui_button("wt-cancel", "Cancel", theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.modal = ActiveModal::None;
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("wt-create", "Add Worktree", theme, true).on_click(cx.listener(
                    move |this, _ev, _w, cx| {
                        let target_dir =
                            this.modal_primary_input.read(cx).text().trim().to_string();
                        let branch_ref = this
                            .modal_secondary_input
                            .read(cx)
                            .text()
                            .trim()
                            .to_string();
                        if target_dir.is_empty() || branch_ref.is_empty() {
                            if let ActiveModal::CreateWorktree { ref mut error, .. } =
                                this.presenter.modal
                            {
                                *error = Some(
                                    "Both target directory and branch/ref are required".to_string(),
                                );
                            }
                            cx.notify();
                            return;
                        }
                        this.presenter.modal = ActiveModal::None;
                        this.presenter.create_worktree(
                            HostPath::from_path_buf(target_dir.into()),
                            branch_ref,
                            create_new_branch,
                        );
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(480.0), theme, col.into_any_element()).into_any_element()
}

fn render_open_repo_modal(
    view: &WorkspaceView,
    error: Option<&str>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(to_rgba(theme.text_primary))
                .child("Open Local Git Repository Path"),
        )
        .child(view.modal_primary_input.clone());

    if let Some(err) = error {
        col = col.child(ui_pill(err, theme.conflicted_bg, theme.error));
    }

    col = col.child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(
                ui_button("open-repo-cancel", "Cancel", theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.modal = ActiveModal::None;
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("open-repo-submit", "Open Tab", theme, true).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        let path_str = this.modal_primary_input.read(cx).text().trim().to_string();
                        match this
                            .presenter
                            .open_repository_tab(std::path::Path::new(&path_str))
                        {
                            Ok(_) => this.presenter.modal = ActiveModal::None,
                            Err(err) => {
                                if let ActiveModal::OpenRepository { ref mut error, .. } =
                                    this.presenter.modal
                                {
                                    *error = Some(err);
                                }
                            }
                        }
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(460.0), theme, col.into_any_element()).into_any_element()
}

fn render_confirm_pushed_undo_modal(
    is_redo: bool,
    remote_branches: &[compact_str::CompactString],
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let verb = if is_redo { "Redo" } else { "Undo" };
    let branches_text = remote_branches
        .iter()
        .map(compact_str::CompactString::as_str)
        .collect::<Vec<_>>()
        .join(", ");

    let col = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(ui_pill("PUSHED HISTORY WARNING", theme.conflicted_bg, theme.warning))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(to_rgba(theme.text_primary))
                        .child(format!("Confirm {verb} Affecting Pushed Commits")),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_secondary))
                .child(format!(
                    "This rollback rewrites commits already present on remote tracking branch(es): {branches_text}. Subsequent pushes will require --force-with-lease."
                )),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.0))
                .child(
                    ui_button("pushed-undo-cancel", "Cancel", theme, false).on_click(
                        cx.listener(|this, _ev, _w, cx| {
                            this.presenter.modal = ActiveModal::None;
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    ui_button("pushed-undo-confirm", &format!("Proceed With {verb}"), theme, true)
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            this.presenter.modal = ActiveModal::None;
                            if is_redo {
                                this.presenter.redo_last_operation(true);
                            } else {
                                this.presenter.undo_last_operation(true);
                            }
                            cx.notify();
                        })),
                ),
        );

    modal_card_shell(px(480.0), theme, col.into_any_element()).into_any_element()
}

/// Header row (warning pill + title) + explanatory paragraph shared by confirmation modals.
fn confirm_modal_body(pill: &str, title: String, body: String, theme: &BranvioTheme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(ui_pill(pill, theme.conflicted_bg, theme.warning))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(to_rgba(theme.text_primary))
                        .child(title),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_secondary))
                .child(body),
        )
}

/// "Cancel" button that simply closes the active modal.
fn modal_cancel_button(
    id: &'static str,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Stateful<gpui::Div> {
    ui_button(id, "Cancel", theme, false).on_click(cx.listener(|this, _ev, _w, cx| {
        this.presenter.modal = ActiveModal::None;
        cx.notify();
    }))
}

fn render_confirm_force_push_modal(
    branch: &compact_str::CompactString,
    expected_remote_oid: Option<GitOid>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let col = confirm_modal_body(
        "FORCE PUSH",
        force_push_title(branch),
        force_push_explanation(branch, expected_remote_oid),
        theme,
    )
    .child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(modal_cancel_button("force-push-cancel", theme, cx))
            .child(
                ui_button("force-push-unconditional", "Force push", theme, false).on_click(
                    cx.listener(|this, _ev, _w, cx| {
                        this.presenter.confirm_force_push(ForcePushChoice::Force);
                        cx.notify();
                    }),
                ),
            )
            .child(
                ui_button("force-push-lease", "Force push safely", theme, true).on_click(
                    cx.listener(|this, _ev, _w, cx| {
                        this.presenter
                            .confirm_force_push(ForcePushChoice::WithLease);
                        cx.notify();
                    }),
                ),
            ),
    );

    modal_card_shell(px(500.0), theme, col.into_any_element()).into_any_element()
}

fn render_ai_cloud_consent_modal(
    repo_id: RepoId,
    provider: &str,
    feature: AiFeature,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let body = AiUiError::CloudUploadNotAllowed {
        provider: provider.to_string(),
    }
    .user_message(feature);
    let col = confirm_modal_body(
        "PRIVACY",
        format!("Send {} to {provider}?", feature.payload_label()),
        body,
        theme,
    )
    .child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(modal_cancel_button("ai-cloud-cancel", theme, cx))
            .child(
                ui_button(
                    "ai-cloud-allow-repo",
                    "Allow for this repository",
                    theme,
                    true,
                )
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter.allow_cloud_ai_for_repository(repo_id);
                    cx.notify();
                })),
            ),
    );

    modal_card_shell(px(500.0), theme, col.into_any_element()).into_any_element()
}

fn render_confirm_terminal_command_modal(
    command: &str,
    reason: &str,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let col = confirm_modal_body(
        "DESTRUCTIVE COMMAND",
        format!("Run `{command}`?"),
        format!("{reason} This cannot be undone from Branvio."),
        theme,
    )
    .child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(modal_cancel_button("terminal-cmd-cancel", theme, cx))
            .child(
                ui_button("terminal-cmd-run", "Run anyway", theme, true).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.confirm_destructive_terminal_command();
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(500.0), theme, col.into_any_element()).into_any_element()
}

fn render_confirm_without_undo_modal(
    summary: &str,
    reason: &str,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let col = confirm_modal_body(
        "NO UNDO POINT",
        format!("Branvio could not save an undo point ({reason}). Continue without undo?"),
        format!("Operation: {summary}. If you continue, it cannot be undone from Branvio."),
        theme,
    )
    .child(
        div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(
                ui_button("no-undo-continue", "Continue without undo", theme, false).on_click(
                    cx.listener(|this, _ev, _w, cx| {
                        this.presenter.confirm_continue_without_undo();
                        cx.notify();
                    }),
                ),
            )
            // Cancel is the default (primary) action.
            .child(
                ui_button("no-undo-cancel", "Cancel", theme, true).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.cancel_continue_without_undo();
                        cx.notify();
                    },
                )),
            ),
    );

    modal_card_shell(px(520.0), theme, col.into_any_element()).into_any_element()
}

fn render_create_pr_modal(
    view: &WorkspaceView,
    head_branch: &compact_str::CompactString,
    draft: bool,
    error: Option<&str>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let head = head_branch.clone();
    let mut col = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(to_rgba(theme.text_primary))
                        .child("Create Pull / Merge Request"),
                )
                .child(ui_pill(
                    head.as_str(),
                    theme.bg_elevated,
                    theme.accent_primary,
                )),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_muted))
                .child("Pull Request Title:"),
        )
        .child(view.modal_primary_input.clone())
        .child(
            div()
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_muted))
                .child("Base Branch (e.g. main):"),
        )
        .child(view.modal_secondary_input.clone());

    if let Some(err) = error {
        col = col.child(ui_pill(err, theme.conflicted_bg, theme.error));
    }

    col =
        col.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    ui_button("pr-draft-toggle", "Draft PR", theme, draft).on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            if let ActiveModal::CreatePullRequest { ref mut draft, .. } =
                                this.presenter.modal
                            {
                                *draft = !*draft;
                            }
                            cx.notify();
                        },
                    )),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(ui_button("pr-cancel", "Cancel", theme, false).on_click(
                            cx.listener(|this, _ev, _w, cx| {
                                this.presenter.modal = ActiveModal::None;
                                cx.notify();
                            }),
                        ))
                        .child(ui_button("pr-submit", "Create PR", theme, true).on_click(
                            cx.listener(move |this, _ev, _w, cx| {
                                let title =
                                    this.modal_primary_input.read(cx).text().trim().to_string();
                                let base = this
                                    .modal_secondary_input
                                    .read(cx)
                                    .text()
                                    .trim()
                                    .to_string();
                                if title.is_empty() {
                                    if let ActiveModal::CreatePullRequest {
                                        ref mut error, ..
                                    } = this.presenter.modal
                                    {
                                        *error = Some("Pull Request title is required".to_string());
                                    }
                                    cx.notify();
                                    return;
                                }
                                let base_branch = if base.is_empty() {
                                    "main".to_string()
                                } else {
                                    base
                                };
                                this.presenter.create_pull_request(
                                    forge_client::CreatePullRequestInput {
                                        title,
                                        body: String::new(),
                                        head_branch: head.clone(),
                                        base_branch: compact_str::CompactString::from(base_branch),
                                        draft,
                                    },
                                );
                                cx.notify();
                            }),
                        )),
                ),
        );

    modal_card_shell(px(480.0), theme, col.into_any_element()).into_any_element()
}
