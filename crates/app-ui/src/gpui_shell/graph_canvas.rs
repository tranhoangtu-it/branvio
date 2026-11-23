//! Virtualized 120 FPS Commit Graph canvas (`uniform_list` + GPU `PathBuilder` splines)
//! with Metro lanes, ref badges, signature indicators, drag-drop merging, and auto-pagination.

use crate::gpui_shell::sidebar_pane::{DragBranchGhost, SidebarBranchDragPayload};
use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{to_hsla, to_rgba, ui_pill};
use crate::presenter::graph_geometry::{
    GraphLayoutMetrics, GraphRowGeometry, compute_commit_row_geometry, compute_lane_canvas_width,
    compute_wip_row_geometry,
};
use crate::presenter::pipeline_worker::DEFAULT_REVWALK_CHUNK_SIZE;
use crate::theme::BranvioTheme;
use branvio_core::domain::{CommitNode, CommitSignatureState, GraphNodeKind, RefBadge, RefKind};
use branvio_core::ids::GitOid;
use branvio_core::platform::DisplayScalePolicy;
use compact_str::CompactString;
use gpui::{
    AnyElement, AppContext, BorderStyle, Context, InteractiveElement, IntoElement, PaintQuad,
    ParentElement, PathBuilder, StatefulInteractiveElement, Styled, UniformListScrollHandle,
    Window, canvas, div, point, px, size, uniform_list,
};
use std::ops::Range;

/// Drag payload emitted when dragging a commit row directly in the graph canvas.
#[derive(Debug, Clone)]
pub struct GraphRowDragPayload {
    /// Preferred ref label (branch name if present, else 7-char SHA).
    pub label: CompactString,
    /// Target commit OID.
    pub oid: GitOid,
}

/// Renders the virtualized Commit Graph center list view.
pub fn render_graph_canvas(
    view: &WorkspaceView,
    scroll_handle: UniformListScrollHandle,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let theme = view.presenter.theme;
    let Some(tab) = view.presenter.active_tab() else {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(to_rgba(theme.text_muted))
            .child("Open a Git repository to view commit history")
            .into_any_element();
    };

    let row_count = tab.snapshot.graph.display_row_count();
    let has_more = tab.snapshot.graph.has_more_history;
    let metrics = GraphLayoutMetrics::default();

    let list_el = uniform_list(
        "commit-graph-uniform-list",
        row_count,
        cx.processor(move |this, range: Range<usize>, window, cx| {
            render_visible_graph_rows(this, range, metrics, window, cx)
        }),
    )
    .track_scroll(scroll_handle)
    .flex_1()
    .h_full();

    let footer_hint = if has_more {
        "Scroll down to stream additional history..."
    } else {
        "All reachable history loaded"
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .bg(to_rgba(theme.bg_base))
        .child(render_graph_column_header(&theme))
        .child(list_el)
        .child(
            div()
                .h(px(22.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .justify_between()
                .bg(to_rgba(theme.bg_surface))
                .border_t_1()
                .border_color(to_rgba(theme.border_subtle))
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_muted))
                .child(format!("{row_count} visible row(s)"))
                .child(footer_hint),
        )
        .into_any_element()
}

fn render_graph_column_header(theme: &BranvioTheme) -> gpui::Div {
    div()
        .h(px(24.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .bg(to_rgba(theme.bg_surface))
        .border_b_1()
        .border_color(to_rgba(theme.border_subtle))
        .text_size(px(11.0))
        .text_color(to_rgba(theme.text_muted))
        .child("GRAPH  |  BRANCHES & COMMIT MESSAGE")
        .child("AUTHOR  /  SHA")
}

fn render_visible_graph_rows(
    view: &mut WorkspaceView,
    range: Range<usize>,
    metrics: GraphLayoutMetrics,
    window: &mut Window,
    cx: &mut Context<'_, WorkspaceView>,
) -> Vec<AnyElement> {
    let scale_policy = DisplayScalePolicy::from_scale_factor(window.scale_factor());
    let theme = view.presenter.theme;

    let Some(tab) = view.presenter.active_tab() else {
        return Vec::new();
    };
    let snapshot = &tab.snapshot;
    let graph = &snapshot.graph;

    // Trigger incremental chunked pagination when viewport approaches the end of loaded rows.
    if graph.has_more_history
        && !tab.is_loading_more
        && !tab.revwalk_exhausted
        && range.end.saturating_add(40) >= graph.display_row_count()
    {
        view.presenter.load_more_history(DEFAULT_REVWALK_CHUNK_SIZE);
    }

    let Some(tab) = view.presenter.active_tab() else {
        return Vec::new();
    };
    let snapshot = &tab.snapshot;
    let graph = &snapshot.graph;
    let shared_graph_width = compute_lane_canvas_width(
        graph.max_lane_width.saturating_add(1),
        metrics,
        scale_policy,
    )
    .max(metrics.min_graph_width);

    let selected_commit_oid = tab.selected_commit_oid;
    let selected_wip = tab.selected_wip;
    let head_oid = snapshot.head_oid;
    let mut elements = Vec::with_capacity(range.len());

    for display_idx in range {
        if display_idx == 0
            && let Some(ref wip) = graph.wip_row
        {
            let geom = compute_wip_row_geometry(wip, metrics, scale_policy, &theme, selected_wip);
            let total_dirty =
                wip.staged_count + wip.unstaged_count + wip.conflicted_count + wip.untracked_count;
            let wip_summary = format!(
                "// WIP: {} staged, {} modified, {} conflicted, {} untracked",
                wip.staged_count, wip.unstaged_count, wip.conflicted_count, wip.untracked_count
            );
            elements.push(render_wip_row(
                display_idx,
                shared_graph_width,
                metrics.row_height,
                geom,
                selected_wip,
                total_dirty,
                wip_summary,
                &theme,
                cx,
            ));
            continue;
        }

        let Some(commit_idx) = graph.display_to_commit_index(display_idx) else {
            continue;
        };
        let (Some(commit), Some(row)) = (
            graph.commits.get(commit_idx),
            graph.commit_rows.get(commit_idx),
        ) else {
            continue;
        };

        let is_head = head_oid == Some(commit.oid);
        let is_selected = !selected_wip && selected_commit_oid == Some(commit.oid);
        let geom =
            compute_commit_row_geometry(row, metrics, scale_policy, &theme, is_head, is_selected);
        let badges = graph
            .refs_by_oid
            .get(&commit.oid)
            .map_or(&[][..], |s| s.as_slice());

        elements.push(render_commit_row(
            display_idx,
            shared_graph_width,
            metrics.row_height,
            commit,
            badges,
            geom,
            is_head,
            is_selected,
            &theme,
            cx,
        ));
    }

    elements
}

#[allow(clippy::too_many_arguments)]
fn render_wip_row(
    row_id: usize,
    graph_width: f32,
    row_height: f32,
    geom: GraphRowGeometry,
    selected: bool,
    total_dirty: u32,
    label: String,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    div()
        .id(("graph-row-wip", row_id))
        .h(px(row_height))
        .w_full()
        .flex()
        .items_center()
        .px(px(8.0))
        .bg(to_rgba(if selected { theme.bg_selected } else { 0 }))
        .hover(|s| s.bg(to_rgba(theme.bg_hover)))
        .cursor_pointer()
        .on_click(cx.listener(|this, _ev, _w, cx| {
            this.presenter.select_wip_row();
            cx.notify();
        }))
        .child(render_metro_row_canvas(graph_width, row_height, geom))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .flex_1()
                .overflow_hidden()
                .child(ui_pill(
                    &format!("WIP ({total_dirty})"),
                    theme.wip_node,
                    0x0B0F17FF,
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(to_rgba(theme.wip_node))
                        .child(label),
                ),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_commit_row(
    row_id: usize,
    graph_width: f32,
    row_height: f32,
    commit: &CommitNode,
    badges: &[RefBadge],
    geom: GraphRowGeometry,
    is_head: bool,
    is_selected: bool,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let oid = commit.oid;
    let short_sha = commit.oid.short_hex(7);
    let primary_label = badges
        .first()
        .map(|b| b.name.clone())
        .unwrap_or_else(|| CompactString::from(short_sha.as_str()));

    let drag_payload = GraphRowDragPayload {
        label: primary_label.clone(),
        oid,
    };
    let drag_bg = theme.accent_primary;
    let target_label_for_drop = primary_label;
    let sig_badge = signature_indicator(&commit.signature_state, theme);
    let node_kind = geom.node.kind;

    let mut row = div()
        .id(("graph-row-commit", row_id))
        .h(px(row_height))
        .w_full()
        .flex()
        .items_center()
        .px(px(8.0))
        .bg(to_rgba(if is_selected { theme.bg_selected } else { 0 }))
        .hover(|s| s.bg(to_rgba(theme.bg_hover)))
        .drag_over::<SidebarBranchDragPayload>(|style, _p, _w, _cx| style.bg(to_rgba(0x3B82F633)))
        .drag_over::<GraphRowDragPayload>(|style, _p, _w, _cx| style.bg(to_rgba(0x3B82F633)))
        .on_drag(drag_payload, move |payload, _pos, _w, cx| {
            cx.new(|_| DragBranchGhost::new(payload.label.clone(), drag_bg, 0x0B0F17FF))
        })
        .on_drop(
            cx.listener(move |this, payload: &SidebarBranchDragPayload, _w, cx| {
                this.presenter.update_drag_hover(
                    payload.label.clone(),
                    payload.oid,
                    Some(target_label_for_drop.clone()),
                    Some(oid),
                );
                this.presenter.finish_drag_drop_popover();
                cx.notify();
            }),
        )
        .on_click(cx.listener(move |this, _ev, _w, cx| {
            this.presenter.select_commit_row(oid);
            cx.notify();
        }));

    row = row.child(render_metro_row_canvas(graph_width, row_height, geom));

    let mut left_content = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .flex_1()
        .overflow_hidden();

    for badge in badges.iter().take(4) {
        let bg = match &badge.kind {
            RefKind::LocalBranch => {
                if badge.is_head || is_head {
                    theme.head_accent
                } else {
                    theme.accent_primary
                }
            }
            RefKind::RemoteBranch { .. } => theme.bg_elevated,
            RefKind::Tag { .. } => theme.warning,
            RefKind::Stash => theme.accent_secondary,
            _ => theme.bg_elevated,
        };
        let fg = match &badge.kind {
            RefKind::RemoteBranch { .. } => theme.text_secondary,
            _ => 0x0B0F17FF,
        };
        left_content = left_content.child(ui_pill(badge.name.as_str(), bg, fg));
    }

    if let GraphNodeKind::OctopusMerge { parent_count } = node_kind {
        left_content = left_content.child(ui_pill(
            &format!("x{parent_count}"),
            theme.bg_elevated,
            theme.accent_secondary,
        ));
    }

    left_content = left_content.child(
        div()
            .text_size(px(12.5))
            .text_color(to_rgba(theme.text_primary))
            .overflow_hidden()
            .child(commit.summary.to_string()),
    );

    let mut right_meta = div().flex().items_center().gap(px(8.0)).ml(px(12.0));
    if let Some((label, color)) = sig_badge {
        right_meta = right_meta.child(ui_pill(label, theme.bg_elevated, color));
    }
    right_meta = right_meta
        .child(
            div()
                .text_size(px(11.5))
                .text_color(to_rgba(theme.text_secondary))
                .child(commit.author.name.to_string()),
        )
        .child(
            div()
                .w(px(64.0))
                .text_size(px(11.5))
                .text_color(to_rgba(theme.text_muted))
                .child(short_sha.to_string()),
        );

    row.child(left_content).child(right_meta).into_any_element()
}

fn signature_indicator(
    sig: &CommitSignatureState,
    theme: &BranvioTheme,
) -> Option<(&'static str, u32)> {
    match sig {
        CommitSignatureState::ValidGpg { .. } => Some(("GPG OK", theme.added)),
        CommitSignatureState::ValidSsh { .. } => Some(("SSH OK", theme.added)),
        CommitSignatureState::Invalid { .. } => Some(("SIG BAD", theme.error)),
        CommitSignatureState::Unverified | CommitSignatureState::UnknownKey { .. } => {
            Some(("SIG ?", theme.warning))
        }
        CommitSignatureState::Unsigned => None,
    }
}

fn render_metro_row_canvas(width: f32, height: f32, geom: GraphRowGeometry) -> AnyElement {
    let w = width.max(geom.lane_canvas_width);
    canvas(
        move |_bounds, _w, _cx| (),
        move |bounds, _state, window, _cx| {
            let ox = bounds.origin.x;
            let oy = bounds.origin.y;

            for seg in &geom.lines {
                let mut builder = PathBuilder::stroke(px(seg.stroke_width));
                if seg.dashed {
                    builder = builder.dash_array(&[px(3.5), px(3.0)]);
                }
                builder.move_to(point(ox + px(seg.from.x), oy + px(seg.from.y)));
                builder.line_to(point(ox + px(seg.to.x), oy + px(seg.to.y)));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, to_hsla(seg.color_rgba));
                }
            }

            for curve in &geom.curves {
                let mut builder = PathBuilder::stroke(px(curve.stroke_width));
                if curve.dashed {
                    builder = builder.dash_array(&[px(3.5), px(3.0)]);
                }
                builder.move_to(point(ox + px(curve.start.x), oy + px(curve.start.y)));
                builder.cubic_bezier_to(
                    point(ox + px(curve.end.x), oy + px(curve.end.y)),
                    point(ox + px(curve.ctrl1.x), oy + px(curve.ctrl1.y)),
                    point(ox + px(curve.ctrl2.x), oy + px(curve.ctrl2.y)),
                );
                if let Ok(path) = builder.build() {
                    window.paint_path(path, to_hsla(curve.color_rgba));
                }
            }

            let r = geom.node.radius;
            let cx_pt = ox + px(geom.node.center.x);
            let cy_pt = oy + px(geom.node.center.y);
            let border_w = if geom.node.is_head || geom.node.is_selected {
                2.0
            } else {
                1.25
            };
            window.paint_quad(PaintQuad {
                bounds: gpui::Bounds::new(
                    point(cx_pt - px(r), cy_pt - px(r)),
                    size(px(r * 2.0), px(r * 2.0)),
                ),
                corner_radii: px(r).into(),
                background: to_hsla(geom.node.fill_rgba).into(),
                border_widths: px(border_w).into(),
                border_color: to_hsla(geom.node.border_rgba),
                border_style: BorderStyle::Solid,
            });
        },
    )
    .w(px(w))
    .h(px(height))
    .into_any_element()
}
