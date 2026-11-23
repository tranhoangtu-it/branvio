//! Interactive 4-buffer `ropey::Rope` 3-Way Merge Conflict Editor (`Ours`, `Base`, `Theirs`, `Merged Output`).

use crate::gpui_shell::workspace_view::WorkspaceView;
use crate::gpui_shell::{to_rgba, ui_button, ui_pill};
use crate::theme::BranvioTheme;
use branvio_core::domain::{ConflictRegion, ConflictResolution};
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};

/// Renders the interactive 4-pane 3-Way Merge Editor for the active conflicted file.
pub fn render_merge_editor_pane(
    view: &WorkspaceView,
    cx: &mut Context<'_, WorkspaceView>,
) -> AnyElement {
    let theme = view.presenter.theme;
    let Some(tab) = view.presenter.active_tab() else {
        return div().into_any_element();
    };
    let Some(ref session) = tab.conflict_session else {
        return div().into_any_element();
    };

    let doc = &session.document;
    let (resolved, manual, unresolved) = doc.resolution_summary();
    let ready_to_save = doc.is_fully_resolved();

    let header = div()
        .h(px(38.0))
        .px(px(12.0))
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
                .gap(px(8.0))
                .child(ui_pill(
                    "3-WAY MERGE",
                    theme.conflicted_bg,
                    theme.conflicted,
                ))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(to_rgba(theme.text_primary))
                        .child(doc.path.as_str().to_string()),
                )
                .child(ui_pill(
                    &format!("{unresolved} unresolved  |  {resolved} auto  |  {manual} manual"),
                    theme.bg_elevated,
                    if ready_to_save {
                        theme.added
                    } else {
                        theme.warning
                    },
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    ui_button(
                        "merge-save-stage-btn",
                        if ready_to_save {
                            "Save & Mark Resolved"
                        } else {
                            "Resolve Remaining Conflicts First"
                        },
                        &theme,
                        ready_to_save,
                    )
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.presenter.save_and_mark_conflict_resolved(true);
                        cx.notify();
                    })),
                )
                .child(
                    ui_button("merge-close-btn", "Close", &theme, false).on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            if let Some(tab) = this.presenter.active_tab_mut() {
                                tab.conflict_session = None;
                            }
                            cx.notify();
                        },
                    )),
                ),
        );

    let mut regions_scroll = div()
        .id("merge-regions-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_y_scroll()
        .p(px(10.0))
        .gap(px(12.0));

    for region in &doc.regions {
        let advice = tab.ai_conflict_advice.get(&region.region_id);
        regions_scroll = regions_scroll.child(render_conflict_region_card(
            doc,
            region,
            &session.ours_label,
            &session.theirs_label,
            advice,
            &theme,
            cx,
        ));
    }

    let output_preview_lines: String = doc
        .output
        .lines()
        .take(250)
        .enumerate()
        .map(|(idx, line)| format!("{:>4} | {}", idx + 1, line))
        .collect::<Vec<_>>()
        .join("");

    let output_pane = div()
        .h(px(220.0))
        .flex()
        .flex_col()
        .bg(to_rgba(theme.bg_surface))
        .border_t_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(
            div()
                .h(px(26.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(to_rgba(theme.border_subtle))
                .text_size(px(11.0))
                .text_color(to_rgba(theme.text_secondary))
                .child("LIVE MERGED OUTPUT BUFFER (`ropey::Rope`)")
                .child(format!("{} total lines", doc.output.len_lines())),
        )
        .child(
            div()
                .id("merge-output-scroll")
                .flex_1()
                .overflow_y_scroll()
                .p(px(8.0))
                .text_size(px(11.5))
                .text_color(to_rgba(theme.text_primary))
                .child(output_preview_lines),
        );

    div()
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .bg(to_rgba(theme.bg_base))
        .child(header)
        .child(regions_scroll)
        .child(output_pane)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_conflict_region_card(
    doc: &diff_merge::ThreeWayMergeDocument,
    region: &ConflictRegion,
    ours_label: &str,
    theirs_label: &str,
    ai_advice: Option<&ai_copilot::ConflictResolutionAdvice>,
    theme: &BranvioTheme,
    cx: &mut Context<'_, WorkspaceView>,
) -> gpui::Div {
    let region_id = region.region_id;
    let ours_text = doc.extract_region_lines(region, true, false).join("");
    let base_text = doc.extract_region_lines(region, false, true).join("");
    let theirs_text = doc.extract_region_lines(region, false, false).join("");

    let status_label = match &region.resolution {
        ConflictResolution::Unresolved => "UNRESOLVED",
        ConflictResolution::TakeOurs => "Accepted Ours",
        ConflictResolution::TakeTheirs => "Accepted Theirs",
        ConflictResolution::TakeBase => "Reverted Base",
        ConflictResolution::TakeBothOursFirst => "Both (Ours -> Theirs)",
        ConflictResolution::TakeBothTheirsFirst => "Both (Theirs -> Ours)",
        ConflictResolution::LineLevelSelection(_) => "Line Pick",
        ConflictResolution::ManualEdit => "Manual Edit",
    };

    let controls = div()
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
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(to_rgba(theme.text_primary))
                        .child(format!("Conflict #{}", region_id + 1)),
                )
                .child(ui_pill(
                    status_label,
                    if region.is_resolved {
                        theme.added_bg
                    } else {
                        theme.conflicted_bg
                    },
                    if region.is_resolved {
                        theme.added
                    } else {
                        theme.conflicted
                    },
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    ui_button(("mc-ours", region_id as usize), "Accept Ours", theme, false)
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            this.presenter
                                .resolve_conflict_region(region_id, ConflictResolution::TakeOurs);
                            cx.notify();
                        })),
                )
                .child(
                    ui_button(
                        ("mc-theirs", region_id as usize),
                        "Accept Theirs",
                        theme,
                        false,
                    )
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter
                            .resolve_conflict_region(region_id, ConflictResolution::TakeTheirs);
                        cx.notify();
                    })),
                )
                .child(
                    ui_button(
                        ("mc-both", region_id as usize),
                        "Both (Ours->Theirs)",
                        theme,
                        false,
                    )
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.resolve_conflict_region(
                            region_id,
                            ConflictResolution::TakeBothOursFirst,
                        );
                        cx.notify();
                    })),
                )
                .child(
                    ui_button(("mc-base", region_id as usize), "Use Base", theme, false).on_click(
                        cx.listener(move |this, _ev, _w, cx| {
                            this.presenter
                                .resolve_conflict_region(region_id, ConflictResolution::TakeBase);
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    ui_button(("mc-ai", region_id as usize), "AI Suggest", theme, false).on_click(
                        cx.listener(move |this, _ev, _w, cx| {
                            this.presenter.suggest_ai_conflict_resolution(region_id);
                            cx.notify();
                        }),
                    ),
                ),
        );

    let columns = div()
        .flex()
        .w_full()
        .child(render_stage_column(
            &format!("OURS ({ours_label})"),
            &ours_text,
            theme.added,
            theme,
        ))
        .child(render_stage_column(
            "BASE (Ancestor)",
            if base_text.is_empty() {
                "<no common ancestor>"
            } else {
                &base_text
            },
            theme.text_secondary,
            theme,
        ))
        .child(render_stage_column(
            &format!("THEIRS ({theirs_label})"),
            &theirs_text,
            theme.accent_primary,
            theme,
        ));

    let mut card = div()
        .flex()
        .flex_col()
        .rounded(px(6.0))
        .overflow_hidden()
        .border_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(controls);

    if let Some(advice) = ai_advice {
        let strat_label =
            crate::presenter::phase5_worker::conflict_strategy_label(&advice.suggested_strategy);
        card = card.child(
            div()
                .px(px(10.0))
                .py(px(6.0))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .bg(to_rgba(theme.bg_elevated))
                .border_b_1()
                .border_color(to_rgba(theme.border_subtle))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(ui_pill(
                            "AI COPILOT",
                            theme.bg_surface,
                            theme.accent_primary,
                        ))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(to_rgba(theme.text_secondary))
                                .child(advice.rationale.clone()),
                        ),
                )
                .child(
                    ui_button(
                        ("mc-ai-apply", region_id as usize),
                        strat_label,
                        theme,
                        true,
                    )
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.apply_ai_conflict_resolution(region_id);
                        cx.notify();
                    })),
                ),
        );
    }

    card.child(columns)
}

fn render_stage_column(title: &str, text: &str, accent: u32, theme: &BranvioTheme) -> gpui::Div {
    div()
        .w_1_3()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(to_rgba(theme.border_subtle))
        .child(
            div()
                .px(px(8.0))
                .py(px(4.0))
                .bg(to_rgba(theme.bg_elevated))
                .text_size(px(11.0))
                .text_color(to_rgba(accent))
                .child(title.to_string()),
        )
        .child(
            div()
                .p(px(8.0))
                .min_h(px(48.0))
                .bg(to_rgba(theme.bg_base))
                .text_size(px(11.5))
                .text_color(to_rgba(theme.text_primary))
                .child(text.to_string()),
        )
}
