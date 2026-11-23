//! Collapsible Left Sidebar displaying `/` nested branches, grouped remotes, tags,
//! stashes, linked worktrees, and parsed `.gitmodules` submodules with drag sources.

use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{to_rgba, ui_button, ui_pill};
use crate::presenter::sidebar_tree::{FlatSidebarBranchRow, flatten_branch_tree};
use branvio_core::domain::RefBadge;
use branvio_core::ids::GitOid;
use compact_str::CompactString;
use forge_client::CiCheckStatus;
use gpui::{
    AnyElement, AppContext, Context, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, div, px,
};
use std::collections::HashSet;

/// Drag payload emitted when dragging a local or remote branch item out of the Left Sidebar.
#[derive(Debug, Clone)]
pub struct SidebarBranchDragPayload {
    /// Display & revision label (e.g., `feature/auth` or `origin/main`).
    pub label: CompactString,
    /// Commit OID resolved from `snapshot.graph.refs_by_oid`, if loaded in graph.
    pub oid: Option<GitOid>,
}

/// Visual floating badge shown under the cursor while dragging a branch.
pub struct DragBranchGhost {
    label: CompactString,
    bg_rgba: u32,
    fg_rgba: u32,
}

impl DragBranchGhost {
    /// Creates a new floating branch drag pill.
    pub fn new(label: CompactString, bg_rgba: u32, fg_rgba: u32) -> Self {
        Self {
            label,
            bg_rgba,
            fg_rgba,
        }
    }
}

impl Render for DragBranchGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        ui_pill(&self.label, self.bg_rgba, self.fg_rgba)
            .px(px(10.0))
            .py(px(4.0))
    }
}

/// Renders the full Left Sidebar for the currently active repository tab.
pub fn render_sidebar_pane(
    view: &mut WorkspaceView,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let theme = view.presenter.theme;
    let Some(tab) = view.presenter.active_tab() else {
        return div()
            .w(px(250.0))
            .h_full()
            .bg(to_rgba(theme.bg_surface))
            .border_r_1()
            .border_color(to_rgba(theme.border_subtle))
            .p(px(12.0))
            .text_size(px(12.0))
            .text_color(to_rgba(theme.text_muted))
            .child("No repository open")
            .into_any_element();
    };

    let sections = tab.sidebar_sections();
    let local_rows = flatten_branch_tree(&sections.local_branches);
    let remote_groups: Vec<(CompactString, Vec<FlatSidebarBranchRow>)> = sections
        .remotes
        .into_iter()
        .map(|g| (g.remote_name, flatten_branch_tree(&g.tree)))
        .collect();
    let tags = sections.tags;
    let stashes = sections.stashes;
    let worktrees = tab.worktrees.clone();
    let submodules = tab.submodules.clone();
    let pull_requests = tab.pull_requests.clone();
    let pr_loading = tab.pull_requests_loading;
    let pr_error = tab.pull_requests_error.clone();
    let active_root = tab.repo_root.clone();
    let mut oid_by_full_ref = std::collections::HashMap::new();
    for (commit_oid, badges) in tab.snapshot.graph.refs_by_oid.iter() {
        for badge in badges {
            oid_by_full_ref.insert(badge.full_ref.clone(), *commit_oid);
        }
    }

    let mut scroll = div()
        .id("sidebar-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_y_scroll()
        .p(px(8.0))
        .gap(px(10.0));

    // 1. LOCAL BRANCHES
    scroll = scroll.child(render_section_header(
        "LOCAL BRANCHES",
        local_rows.len(),
        &theme,
    ));
    let mut local_list = div().flex().flex_col().gap(px(2.0));
    for (idx, row) in local_rows.into_iter().enumerate() {
        local_list = local_list.child(render_branch_tree_row(
            "local",
            idx,
            row,
            &oid_by_full_ref,
            &theme,
            cx,
        ));
    }
    scroll = scroll.child(local_list);

    // 2. REMOTES
    scroll = scroll.child(render_section_header(
        "REMOTES",
        remote_groups.len(),
        &theme,
    ));
    let mut remote_box = div().flex().flex_col().gap(px(4.0));
    for (r_idx, (remote_name, rows)) in remote_groups.into_iter().enumerate() {
        let mut group = div().flex().flex_col().gap(px(2.0)).child(
            div()
                .px(px(6.0))
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_secondary))
                .child(format!("@{remote_name}")),
        );
        for (b_idx, row) in rows.into_iter().enumerate() {
            group = group.child(render_branch_tree_row(
                &format!("remote-{r_idx}"),
                b_idx,
                row,
                &oid_by_full_ref,
                &theme,
                cx,
            ));
        }
        remote_box = remote_box.child(group);
    }
    scroll = scroll.child(remote_box);

    // 3. TAGS
    scroll = scroll.child(render_section_header("TAGS", tags.len(), &theme));
    let mut tags_box = div().flex().flex_col().gap(px(2.0));
    for (idx, tag) in tags.into_iter().enumerate() {
        let target_name = tag.name.clone();
        tags_box = tags_box.child(
            div()
                .id(("sb-tag", idx))
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_secondary))
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.presenter
                        .checkout_revision(target_name.to_string(), None);
                    cx.notify();
                }))
                .child(format!("# {}", tag.name)),
        );
    }
    scroll = scroll.child(tags_box);

    // 4. STASHES
    scroll = scroll.child(render_section_header("STASHES", stashes.len(), &theme));
    let mut stash_box = div().flex().flex_col().gap(px(2.0));
    for (idx, stash) in stashes.into_iter().enumerate() {
        stash_box = stash_box.child(
            div()
                .id(("sb-stash", idx))
                .flex()
                .items_center()
                .justify_between()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                .text_size(px(12.0))
                .text_color(to_rgba(theme.accent_secondary))
                .child(stash.name.to_string()),
        );
    }
    scroll = scroll.child(stash_box);

    // 5. LINKED WORKTREES
    scroll = scroll.child(render_section_header("WORKTREES", worktrees.len(), &theme));
    let mut wt_box = div().flex().flex_col().gap(px(2.0));
    for (idx, wt) in worktrees.into_iter().enumerate() {
        let is_active = wt.path == active_root;
        let path_for_open = wt.path.as_path().to_path_buf();
        let short_path = wt
            .path
            .as_path()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("worktree")
            .to_string();
        let label = match &wt.branch {
            Some(b) => format!("{b} ({short_path})"),
            None => short_path,
        };
        wt_box = wt_box.child(
            div()
                .id(("sb-wt", idx))
                .flex()
                .items_center()
                .justify_between()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .bg(to_rgba(if is_active { theme.bg_selected } else { 0 }))
                .cursor_pointer()
                .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_primary))
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    let _ = this.presenter.open_repository_tab(&path_for_open);
                    cx.notify();
                }))
                .child(label),
        );
    }
    scroll = scroll.child(wt_box);

    // 6. SUBMODULES (.gitmodules)
    if !submodules.is_empty() {
        scroll = scroll.child(render_section_header(
            "SUBMODULES",
            submodules.len(),
            &theme,
        ));
        let mut sub_box = div().flex().flex_col().gap(px(2.0));
        for (idx, sub) in submodules.into_iter().enumerate() {
            sub_box = sub_box.child(
                div()
                    .id(("sb-sub", idx))
                    .px(px(8.0))
                    .py(px(3.0))
                    .text_size(px(12.0))
                    .text_color(to_rgba(theme.text_secondary))
                    .child(format!("{} ({})", sub.name, sub.path.as_str())),
            );
        }
        scroll = scroll.child(sub_box);
    }

    // 7. PULL REQUESTS (Forge Integration)
    scroll = scroll.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(6.0))
            .pt(px(8.0))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(to_rgba(theme.text_muted))
                    .child(format!("PULL REQUESTS ({})", pull_requests.len())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        ui_button(
                            "sb-pr-refresh",
                            if pr_loading { "..." } else { "Sync" },
                            &theme,
                            false,
                        )
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.presenter.refresh_pull_requests();
                            cx.notify();
                        })),
                    )
                    .child(
                        ui_button("sb-pr-new", "+PR", &theme, false).on_click(cx.listener(
                            |this, _ev, window, cx| {
                                this.presenter.open_create_pull_request_modal();
                                this.modal_primary_input
                                    .update(cx, |f, cx| f.set_text(String::new(), cx));
                                this.modal_secondary_input
                                    .update(cx, |f, cx| f.set_text("main".to_string(), cx));
                                let focus = this.modal_primary_input.read(cx).focus_handle(cx);
                                window.focus(&focus);
                                cx.notify();
                            },
                        )),
                    ),
            ),
    );

    if let Some(ref err) = pr_error {
        scroll = scroll.child(
            div()
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(10.5))
                .text_color(to_rgba(theme.text_muted))
                .child(err.clone()),
        );
    } else if !pull_requests.is_empty() {
        let mut pr_box = div().flex().flex_col().gap(px(2.0));
        for (idx, pr) in pull_requests.into_iter().enumerate() {
            let pr_num = pr.number;
            let (ci_label, ci_col) = match pr.ci_status {
                CiCheckStatus::Passing => ("CI OK", theme.added),
                CiCheckStatus::Failing => ("FAIL", theme.error),
                CiCheckStatus::Pending => ("RUN", theme.warning),
                CiCheckStatus::None => ("PR", theme.text_muted),
            };
            pr_box = pr_box.child(
                div()
                    .id(("sb-pr", idx))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                    .text_size(px(11.5))
                    .text_color(to_rgba(theme.text_primary))
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.checkout_pull_request(pr_num);
                        cx.notify();
                    }))
                    .child(format!("#{pr_num} {}", pr.title))
                    .child(ui_pill(ci_label, theme.bg_elevated, ci_col)),
            );
        }
        scroll = scroll.child(pr_box);
    }

    div()
        .w(px(250.0))
        .h_full()
        .flex()
        .flex_col()
        .bg(to_rgba(theme.bg_surface))
        .border_r_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(scroll)
        .into_any_element()
}

fn render_section_header(
    title: &str,
    count: usize,
    theme: &crate::theme::BranvioTheme,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .px(px(6.0))
        .pt(px(4.0))
        .text_size(px(10.0))
        .text_color(to_rgba(theme.text_muted))
        .child(title.to_string())
        .child(count.to_string())
}

fn render_branch_tree_row(
    prefix: &str,
    idx: usize,
    row: FlatSidebarBranchRow,
    oid_by_full_ref: &std::collections::HashMap<CompactString, GitOid>,
    theme: &crate::theme::BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    match row {
        FlatSidebarBranchRow::Folder {
            name,
            key,
            depth,
            collapsed,
        } => {
            let caret = if collapsed { ">" } else { "v" };
            let indent = px(8.0 + (depth as f32) * 12.0);
            div()
                .id((gpui::SharedString::from(format!("sb-dir-{prefix}")), idx))
                .pl(indent)
                .pr(px(6.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(to_rgba(theme.bg_hover)))
                .text_size(px(12.0))
                .text_color(to_rgba(theme.text_secondary))
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    if let Some(tab) = this.presenter.active_tab_mut() {
                        toggle_folder_key(&mut tab.collapsed_branch_folders, &key);
                    }
                    cx.notify();
                }))
                .child(format!("{caret} {name}/"))
                .into_any_element()
        }
        FlatSidebarBranchRow::Branch {
            leaf_name,
            depth,
            badge,
        } => {
            let oid = oid_by_full_ref.get(&badge.full_ref).copied();
            render_branch_leaf(prefix, idx, leaf_name, depth, badge, oid, theme, cx)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_branch_leaf(
    prefix: &str,
    idx: usize,
    leaf_name: CompactString,
    depth: u16,
    badge: RefBadge,
    oid: Option<GitOid>,
    theme: &crate::theme::BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let indent = px(8.0 + (depth as f32) * 12.0);
    let drag_payload = SidebarBranchDragPayload {
        label: badge.name.clone(),
        oid,
    };
    let bg_pill = theme.accent_primary;
    let checkout_rev = badge.name.clone();

    let mut row = div()
        .id((gpui::SharedString::from(format!("sb-br-{prefix}")), idx))
        .flex()
        .items_center()
        .justify_between()
        .pl(indent)
        .pr(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .bg(to_rgba(if badge.is_head { theme.bg_selected } else { 0 }))
        .hover(|s| s.bg(to_rgba(theme.bg_hover)))
        .text_size(px(12.0))
        .text_color(to_rgba(if badge.is_head {
            theme.head_accent
        } else {
            theme.text_primary
        }))
        .on_drag(drag_payload, move |payload, _pos, _w, cx| {
            cx.new(|_| DragBranchGhost::new(payload.label.clone(), bg_pill, 0x0B0F17FF))
        })
        .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _w, cx| {
            if ev.click_count() >= 2 {
                this.presenter
                    .checkout_revision(checkout_rev.to_string(), None);
            } else if let Some(target_oid) = oid {
                this.presenter.select_commit_row(target_oid);
            }
            cx.notify();
        }));

    let display_text = if badge.is_head {
        format!("* {leaf_name}")
    } else {
        leaf_name.to_string()
    };
    row = row.child(display_text);

    if let Some((ahead, behind)) = badge.upstream_divergence
        && (ahead > 0 || behind > 0)
    {
        row = row.child(ui_pill(
            &format!("↑{ahead} ↓{behind}"),
            theme.bg_elevated,
            theme.text_secondary,
        ));
    }

    row.into_any_element()
}

fn toggle_folder_key(set: &mut HashSet<CompactString>, key: &CompactString) {
    if !set.remove(key) {
        set.insert(key.clone());
    }
}
