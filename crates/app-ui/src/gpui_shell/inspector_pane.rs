//! Right Inspector pane switching dynamically between Working Tree Staging + Commit Composer
//! and selected historical Commit Detail metadata + changed file trees.

use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{to_rgba, ui_button, ui_pill};
use crate::ports::{FileListLayoutMode, InspectorMode};
use crate::presenter::sidebar_tree::{FileTreeRow, build_file_tree_rows};
use crate::theme::BranvioTheme;
use branvio_core::domain::{DeltaArea, FileDelta};
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use std::collections::HashSet;

/// Renders the Right Inspector panel (`WorkingTreeStaging` or `CommitDetail`).
pub fn render_inspector_pane(
    view: &WorkspaceView,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let theme = view.presenter.theme;
    let Some(tab) = view.presenter.active_tab() else {
        return div()
            .w(px(330.0))
            .h_full()
            .bg(to_rgba(theme.bg_surface))
            .border_l_1()
            .border_color(to_rgba(theme.border_subtle))
            .into_any_element();
    };

    let layout_mode = view.presenter.file_list_layout;
    let inspector_mode = view.presenter.inspector_mode;
    let collapsed_dirs = tab.collapsed_file_dirs.clone();
    let selected_delta_path = tab.selected_delta.as_ref().map(|d| d.new_path.clone());

    let content = match inspector_mode {
        InspectorMode::WorkingTreeStaging | InspectorMode::MergeConflictSummary => {
            render_staging_view(
                view,
                layout_mode,
                &collapsed_dirs,
                selected_delta_path.as_ref(),
                &theme,
                cx,
            )
        }
        InspectorMode::CommitDetail { oid } => render_commit_detail_view(
            view,
            oid,
            layout_mode,
            &collapsed_dirs,
            selected_delta_path.as_ref(),
            &theme,
            cx,
        ),
    };

    div()
        .w(px(340.0))
        .h_full()
        .flex()
        .flex_col()
        .bg(to_rgba(theme.bg_surface))
        .border_l_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(content)
        .into_any_element()
}

fn render_layout_toggle_bar(
    title: &str,
    mode: FileListLayoutMode,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let toggle_label = match mode {
        FileListLayoutMode::FlatList => "Tree View",
        FileListLayoutMode::FolderTree => "Flat List",
    };
    div()
        .h(px(32.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_primary))
                .child(title.to_string()),
        )
        .child(
            ui_button("insp-layout-toggle", toggle_label, theme, false).on_click(cx.listener(
                |this, _ev, _w, cx| {
                    this.presenter.file_list_layout = match this.presenter.file_list_layout {
                        FileListLayoutMode::FlatList => FileListLayoutMode::FolderTree,
                        FileListLayoutMode::FolderTree => FileListLayoutMode::FlatList,
                    };
                    cx.notify();
                },
            )),
        )
}

fn render_staging_view(
    view: &WorkspaceView,
    mode: FileListLayoutMode,
    collapsed_dirs: &HashSet<CompactString>,
    selected_path: Option<&RepoRelPath>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let Some(tab) = view.presenter.active_tab() else {
        return div().into_any_element();
    };
    let wt = &tab.snapshot.working_tree;
    let conflicted = wt.conflicted.to_vec();
    let unstaged = wt.unstaged.to_vec();
    let staged = wt.staged.to_vec();
    let can_commit = tab.can_commit();
    let amend = tab.commit_amend;
    let gpg_label = match tab.commit_gpg_sign {
        None => "Sign: Auto",
        Some(true) => "Sign: On",
        Some(false) => "Sign: Off",
    };
    let no_verify = tab.commit_no_verify;

    let mut scroll = div()
        .id("inspector-staging-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_y_scroll()
        .p(px(8.0))
        .gap(px(10.0));

    if !conflicted.is_empty() {
        scroll = scroll.child(render_bucket_header(
            "conflicted",
            "CONFLICTED FILES",
            conflicted.len(),
            None,
            theme.conflicted,
            theme,
            cx,
        ));
        scroll = scroll.child(render_file_bucket_list(
            "conf",
            &conflicted,
            mode,
            collapsed_dirs,
            selected_path,
            theme,
            cx,
        ));
    }

    scroll = scroll.child(render_bucket_header(
        "unstaged",
        "UNSTAGED FILES",
        unstaged.len(),
        Some(("Stage All", true)),
        theme.modified,
        theme,
        cx,
    ));
    scroll = scroll.child(render_file_bucket_list(
        "unstaged",
        &unstaged,
        mode,
        collapsed_dirs,
        selected_path,
        theme,
        cx,
    ));

    scroll = scroll.child(render_bucket_header(
        "staged",
        "STAGED FILES",
        staged.len(),
        Some(("Unstage All", false)),
        theme.added,
        theme,
        cx,
    ));
    scroll = scroll.child(render_file_bucket_list(
        "staged",
        &staged,
        mode,
        collapsed_dirs,
        selected_path,
        theme,
        cx,
    ));

    let composer = div()
        .p(px(10.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .border_t_1()
        .border_color(to_rgba(theme.border_subtle))
        .bg(to_rgba(theme.bg_elevated))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(11.5))
                .text_color(to_rgba(theme.text_secondary))
                .child("COMMIT COMPOSER")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            ui_button(
                                "ai-commit-draft-btn",
                                if tab.ai_commit_busy {
                                    "AI..."
                                } else {
                                    "AI Draft"
                                },
                                theme,
                                false,
                            )
                            .on_click(cx.listener(
                                |this, _ev, _w, cx| {
                                    this.presenter.generate_ai_commit_message();
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            ui_button("toggle-amend-btn", "Amend", theme, amend).on_click(
                                cx.listener(|this, _ev, _w, cx| {
                                    if let Some(tab) = this.presenter.active_tab_mut() {
                                        tab.commit_amend = !tab.commit_amend;
                                    }
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            ui_button(
                                "toggle-gpg-btn",
                                gpg_label,
                                theme,
                                tab.commit_gpg_sign == Some(true),
                            )
                            .on_click(cx.listener(
                                |this, _ev, _w, cx| {
                                    if let Some(tab) = this.presenter.active_tab_mut() {
                                        tab.commit_gpg_sign = match tab.commit_gpg_sign {
                                            None => Some(true),
                                            Some(true) => Some(false),
                                            Some(false) => None,
                                        };
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            ui_button("toggle-noverify-btn", "NoHooks", theme, no_verify).on_click(
                                cx.listener(|this, _ev, _w, cx| {
                                    if let Some(tab) = this.presenter.active_tab_mut() {
                                        tab.commit_no_verify = !tab.commit_no_verify;
                                    }
                                    cx.notify();
                                }),
                            ),
                        ),
                ),
        )
        .child(view.commit_subject_input.clone())
        .child(view.commit_body_input.clone())
        .child(
            ui_button(
                "commit-submit-btn",
                if amend {
                    "Amend Previous Commit"
                } else {
                    "Commit Staged Changes"
                },
                theme,
                can_commit,
            )
            .w_full()
            .on_click(cx.listener(|this, _ev, _w, cx| {
                this.presenter.commit_staged();
                cx.notify();
            })),
        );

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .child(render_layout_toggle_bar(
            "Working Tree Changes",
            mode,
            theme,
            cx,
        ))
        .child(scroll)
        .child(composer)
        .into_any_element()
}

fn render_commit_detail_view(
    view: &WorkspaceView,
    oid: branvio_core::ids::GitOid,
    mode: FileListLayoutMode,
    collapsed_dirs: &HashSet<CompactString>,
    selected_path: Option<&RepoRelPath>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let Some(tab) = view.presenter.active_tab() else {
        return div().into_any_element();
    };

    let mut scroll = div()
        .id("inspector-commit-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_y_scroll()
        .p(px(10.0))
        .gap(px(10.0));

    scroll = scroll.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(ui_pill(
                &oid.short_hex(10),
                theme.bg_elevated,
                theme.accent_primary,
            ))
            .child(
                ui_button("insp-back-wip", "View WIP Staging", theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.select_wip_row();
                        cx.notify();
                    },
                )),
            ),
    );

    if let Some(ref details) = tab.selected_commit_details
        && details.oid == oid
    {
        scroll = scroll.child(
            div()
                .p(px(8.0))
                .rounded(px(5.0))
                .bg(to_rgba(theme.bg_base))
                .border_1()
                .border_color(to_rgba(theme.border_subtle))
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_primary))
                .child(details.full_message.clone()),
        );

        if !details.trailers.is_empty() {
            let mut trailers_box = div().flex().flex_wrap().gap(px(4.0));
            for (k, v) in &details.trailers {
                trailers_box = trailers_box.child(ui_pill(
                    &format!("{k}: {v}"),
                    theme.bg_elevated,
                    theme.text_secondary,
                ));
            }
            scroll = scroll.child(trailers_box);
        }

        scroll = scroll.child(
            div()
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_muted))
                .child(format!("CHANGED FILES ({})", details.changed_files.len())),
        );
        scroll = scroll.child(render_file_bucket_list(
            "commit-files",
            &details.changed_files,
            mode,
            collapsed_dirs,
            selected_path,
            theme,
            cx,
        ));
    } else {
        scroll = scroll.child(
            div()
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_muted))
                .child("Loading commit details..."),
        );
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .child(render_layout_toggle_bar(
            "Commit Inspector",
            mode,
            theme,
            cx,
        ))
        .child(scroll)
        .into_any_element()
}

fn render_bucket_header(
    id_prefix: &'static str,
    title: &str,
    count: usize,
    action: Option<(&'static str, bool)>,
    accent: u32,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let mut row = div()
        .flex()
        .items_center()
        .justify_between()
        .px(px(4.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(11.0))
                .text_color(to_rgba(accent))
                .child(format!("{title} ({count})")),
        );

    if let Some((btn_label, stage_all)) = action
        && count > 0
    {
        row = row.child(
            ui_button((id_prefix, count), btn_label, theme, false).on_click(cx.listener(
                move |this, _ev, _w, cx| {
                    if stage_all {
                        this.presenter.stage_all_working_tree();
                    } else {
                        this.presenter.unstage_all_staged();
                    }
                    cx.notify();
                },
            )),
        );
    }

    row
}

fn render_file_bucket_list(
    prefix: &str,
    deltas: &[FileDelta],
    mode: FileListLayoutMode,
    collapsed_dirs: &HashSet<CompactString>,
    selected_path: Option<&RepoRelPath>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let rows = build_file_tree_rows(deltas, mode, collapsed_dirs);
    let mut container = div().flex().flex_col().gap(px(2.0));

    for (idx, row) in rows.into_iter().enumerate() {
        match row {
            FileTreeRow::Directory {
                name,
                dir_path,
                depth,
                collapsed,
                file_count,
            } => {
                let caret = if collapsed { ">" } else { "v" };
                let indent = px(6.0 + (depth as f32) * 12.0);
                container = container.child(
                    div()
                        .id((gpui::SharedString::from(format!("ft-dir-{prefix}")), idx))
                        .pl(indent)
                        .pr(px(6.0))
                        .py(px(3.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(12.0))
                        .text_color(to_rgba(theme.text_secondary))
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            if let Some(tab) = this.presenter.active_tab_mut()
                                && !tab.collapsed_file_dirs.remove(&dir_path)
                            {
                                tab.collapsed_file_dirs.insert(dir_path.clone());
                            }
                            cx.notify();
                        }))
                        .child(format!("{caret} {name}/"))
                        .child(file_count.to_string()),
                );
            }
            FileTreeRow::File {
                depth,
                display_name,
                delta,
            } => {
                let is_selected = selected_path == Some(&delta.new_path);
                let (glyph, glyph_col) = theme.delta_status_badge(delta.status);
                let indent = px(6.0 + (depth as f32) * 12.0);
                let delta_click = delta.clone();
                let path_act = delta.new_path.clone();
                let area = delta.area;

                let mut row_el = div()
                    .id((gpui::SharedString::from(format!("ft-file-{prefix}")), idx))
                    .pl(indent)
                    .pr(px(4.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(to_rgba(if is_selected { theme.bg_selected } else { 0 }))
                    .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.select_file_delta(delta_click.clone());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .flex_1()
                            .overflow_hidden()
                            .child(ui_pill(glyph, theme.bg_elevated, glyph_col))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(to_rgba(theme.text_primary))
                                    .child(display_name.to_string()),
                            ),
                    );

                match area {
                    DeltaArea::Unstaged => {
                        let delta_discard = delta.clone();
                        row_el = row_el.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child(ui_button(("st-one", idx), "+", theme, false).on_click(
                                    cx.listener(move |this, _ev, _w, cx| {
                                        this.presenter.stage_paths(vec![path_act.clone()]);
                                        cx.notify();
                                    }),
                                ))
                                .child(ui_button(("disc-one", idx), "x", theme, false).on_click(
                                    cx.listener(move |this, _ev, _w, cx| {
                                        this.presenter.discard_worktree_deltas(
                                            std::slice::from_ref(&delta_discard),
                                        );
                                        cx.notify();
                                    }),
                                )),
                        );
                    }
                    DeltaArea::Staged => {
                        row_el =
                            row_el.child(ui_button(("unst-one", idx), "-", theme, false).on_click(
                                cx.listener(move |this, _ev, _w, cx| {
                                    this.presenter.unstage_paths(vec![path_act.clone()]);
                                    cx.notify();
                                }),
                            ));
                    }
                    DeltaArea::Conflicted => {
                        let delta_open = delta.clone();
                        row_el = row_el.child(
                            ui_button(("res-one", idx), "Resolve", theme, true).on_click(
                                cx.listener(move |this, _ev, _w, cx| {
                                    this.presenter.select_file_delta(delta_open.clone());
                                    cx.notify();
                                }),
                            ),
                        );
                    }
                    DeltaArea::CommitDiff => {}
                }

                container = container.child(row_el);
            }
        }
    }

    container
}
