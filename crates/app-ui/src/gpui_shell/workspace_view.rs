//! Root GPUI workspace view composing Tab Bar, Action Toolbar, Left Sidebar,
//! Virtualized Commit Graph Canvas, Diff / 3-Way Merge Split Pane, Right Inspector,
//! Bottom Streaming CLI Output Drawer, and Modal Overlays.

use crate::gpui_shell::diff_pane::render_diff_pane;
use crate::gpui_shell::graph_canvas::render_graph_canvas;
use crate::gpui_shell::inspector_pane::render_inspector_pane;
use crate::gpui_shell::merge_editor_pane::render_merge_editor_pane;
use crate::gpui_shell::modals::render_active_modal_overlay;
use crate::gpui_shell::sidebar_pane::render_sidebar_pane;
use crate::gpui_shell::text_field::BranvioTextField;
use crate::gpui_shell::{
    CloseActiveModal, CommitStagedAction, FocusSearchAction, GpuiWakeupBridge,
    OpenBranchDialogAction, OpenRepoDialogAction, QuitApp, RedoAction, ToggleCommandPalette,
    ToggleDrawerAction, UndoAction, to_rgba, ui_button, ui_pill,
};
use crate::ports::{CoreAppHost, DiffViewMode};
use crate::presenter::command_palette::PaletteActionKind;
use crate::presenter::{ActiveModal, BottomDrawerTab, BranvioPresenter, operation_state_banner};
use crate::theme::ThemeAppearance;
use branvio_core::pipeline::UiWakeupSink;
use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Task,
    UniformListScrollHandle, WeakEntity, Window, div, px,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Top-level window root view driving the `PURE_RUST_GPUI` desktop experience.
pub struct WorkspaceView {
    /// UI-agnostic presenter state & async workflow coordinator.
    pub presenter: BranvioPresenter,
    /// Wakeup bridge shared with Tokio background pipeline tasks.
    pub wakeup_bridge: Arc<GpuiWakeupBridge>,
    /// Virtualized `uniform_list` scroll handle for the Commit Graph.
    pub graph_scroll_handle: UniformListScrollHandle,
    /// Root focus handle catching global key shortcuts.
    pub focus_handle: FocusHandle,
    /// Toolbar graph search / pickaxe query input.
    pub search_input: Entity<BranvioTextField>,
    /// Right inspector commit subject input.
    pub commit_subject_input: Entity<BranvioTextField>,
    /// Right inspector commit body input.
    pub commit_body_input: Entity<BranvioTextField>,
    /// Shared primary input used inside active modal overlays (palette query, branch name, repo path).
    pub modal_primary_input: Entity<BranvioTextField>,
    /// Shared secondary input used inside multi-field modals (e.g., Worktree branch/ref).
    pub modal_secondary_input: Entity<BranvioTextField>,
    /// Interactive command input sent directly to the active PTY shell session.
    pub terminal_input: Entity<BranvioTextField>,
    /// Background GPUI task pumping [`GpuiWakeupBridge`] notifications into `drain_events()`.
    _wakeup_task: Task<()>,
}

impl WorkspaceView {
    /// Initializes the GPUI [`WorkspaceView`] around [`CoreAppHost`].
    pub fn new(
        host: CoreAppHost,
        wakeup_bridge: Arc<GpuiWakeupBridge>,
        wakeup_rx: Option<mpsc::UnboundedReceiver<()>>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let presenter =
            BranvioPresenter::new(host, Arc::clone(&wakeup_bridge) as Arc<dyn UiWakeupSink>);
        let weak_self = cx.entity().downgrade();

        let search_input = cx.new(|cx| {
            let weak_chg = weak_self.clone();
            let weak_sub = weak_self.clone();
            BranvioTextField::new("Filter commits or Pickaxe (-S)...", false, cx)
                .on_change(move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_chg.update(cx, |view, cx| {
                        let (is_pk, is_rx) = view
                            .presenter
                            .active_tab()
                            .map_or((false, false), |t| (t.search_is_pickaxe, t.search_is_regex));
                        view.presenter
                            .set_search_filter(owned.clone(), is_pk, is_rx);
                        cx.notify();
                    });
                })
                .on_submit(move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_sub.update(cx, |view, cx| {
                        let (is_pk, is_rx) = view
                            .presenter
                            .active_tab()
                            .map_or((false, false), |t| (t.search_is_pickaxe, t.search_is_regex));
                        view.presenter
                            .set_search_filter(owned.clone(), is_pk, is_rx);
                        cx.notify();
                    });
                })
        });

        let weak_subj = weak_self.clone();
        let weak_subj_sub = weak_self.clone();
        let commit_subject_input = cx.new(|cx| {
            BranvioTextField::new("Commit summary (subject)...", false, cx)
                .on_change(move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_subj.update(cx, |view, cx| {
                        if let Some(tab) = view.presenter.active_tab_mut() {
                            tab.commit_subject = owned.clone();
                        }
                        cx.notify();
                    });
                })
                .on_submit(move |_text, _w, cx| {
                    let _ = weak_subj_sub.update(cx, |view, cx| {
                        view.presenter.commit_staged();
                        cx.notify();
                    });
                })
        });

        let weak_body = weak_self.clone();
        let commit_body_input = cx.new(|cx| {
            BranvioTextField::new("Optional extended description...", true, cx).on_change(
                move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_body.update(cx, |view, cx| {
                        if let Some(tab) = view.presenter.active_tab_mut() {
                            tab.commit_body = owned.clone();
                        }
                        cx.notify();
                    });
                },
            )
        });

        let weak_mod_chg = weak_self.clone();
        let weak_mod_sub = weak_self.clone();
        let modal_primary_input = cx.new(|cx| {
            BranvioTextField::new("Type query, path, branch, or message...", false, cx)
                .on_change(move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_mod_chg.update(cx, |view, cx| {
                        if let ActiveModal::CommandPalette {
                            ref mut query,
                            ref mut selected_index,
                        } = view.presenter.modal
                        {
                            *query = owned.clone();
                            *selected_index = 0;
                        }
                        cx.notify();
                    });
                })
                .on_submit(move |text, _w, cx| {
                    let owned = text.to_string();
                    let _ = weak_mod_sub.update(cx, |view, cx| {
                        if let ActiveModal::CommandPalette { selected_index, .. } =
                            view.presenter.modal
                        {
                            let (entries, matches) = view.presenter.search_command_palette(&owned);
                            if let Some(m) = matches.get(selected_index) {
                                let act = entries[m.entry_index].action.clone();
                                view.presenter.execute_palette_action(act);
                            }
                        }
                        cx.notify();
                    });
                })
        });

        let modal_secondary_input =
            cx.new(|cx| BranvioTextField::new("Branch or revision (for worktree)...", false, cx));

        let weak_term = weak_self.clone();
        let terminal_input = cx.new(|cx| {
            BranvioTextField::new("Type shell command and press Enter...", false, cx).on_submit(
                move |cmd_line, _w, cx| {
                    let line = cmd_line.to_string();
                    let _ = weak_term.update(cx, |view, cx| {
                        view.presenter.send_terminal_command_line(&line);
                        view.terminal_input
                            .update(cx, |f, cx| f.set_text(String::new(), cx));
                        cx.notify();
                    });
                },
            )
        });

        let bridge_clone = Arc::clone(&wakeup_bridge);
        let wakeup_task = cx.spawn(
            async move |weak: WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                if let Some(mut rx) = wakeup_rx {
                    while rx.recv().await.is_some() {
                        bridge_clone.clear_pending();
                        let updated = weak.update(cx, |view, cx| {
                            view.presenter.drain_events();
                            view.sync_inputs_from_active_tab(cx);
                            cx.notify();
                        });
                        if updated.is_err() {
                            break;
                        }
                    }
                }
            },
        );

        Self {
            presenter,
            wakeup_bridge,
            graph_scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            search_input,
            commit_subject_input,
            commit_body_input,
            modal_primary_input,
            modal_secondary_input,
            terminal_input,
            _wakeup_task: wakeup_task,
        }
    }

    fn sync_inputs_from_active_tab(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(tab) = self.presenter.active_tab() {
            let subj = tab.commit_subject.clone();
            let body = tab.commit_body.clone();
            let search = tab.search_query.clone();
            self.commit_subject_input
                .update(cx, |f, cx| f.set_text(subj, cx));
            self.commit_body_input
                .update(cx, |f, cx| f.set_text(body, cx));
            self.search_input.update(cx, |f, cx| f.set_text(search, cx));
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.presenter.modal = ActiveModal::CommandPalette {
            query: String::new(),
            selected_index: 0,
        };
        self.modal_primary_input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        let focus = self.modal_primary_input.read(cx).focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    fn render_tab_bar(&mut self, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = self.presenter.theme;
        let active_id = self.presenter.active_repo_id;
        let mut bar = div()
            .h(px(34.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .bg(to_rgba(theme.bg_surface))
            .border_b_1()
            .border_color(to_rgba(theme.border_subtle));

        let mut tabs_row = div().flex().items_center().gap(px(4.0));
        for (idx, tab) in self.presenter.tabs.iter().enumerate() {
            let repo_id = tab.repo_id;
            let is_active = Some(repo_id) == active_id;
            let head_label = tab.snapshot.head_label.clone();
            let title = format!("{} ({head_label})", tab.display_name);

            tabs_row = tabs_row.child(
                div()
                    .id(("repo-tab", idx))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .bg(to_rgba(if is_active {
                        theme.bg_elevated
                    } else {
                        theme.bg_base
                    }))
                    .border_1()
                    .border_color(to_rgba(if is_active {
                        theme.accent_primary
                    } else {
                        theme.border_subtle
                    }))
                    .text_size(px(12.0))
                    .text_color(to_rgba(if is_active {
                        theme.text_primary
                    } else {
                        theme.text_secondary
                    }))
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.presenter.select_tab(repo_id);
                        this.sync_inputs_from_active_tab(cx);
                        cx.notify();
                    }))
                    .child(title)
                    .child(
                        div()
                            .id(("repo-tab-close", idx))
                            .px(px(3.0))
                            .text_color(to_rgba(theme.text_muted))
                            .hover(|s| s.text_color(to_rgba(theme.error)))
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.presenter.close_tab(repo_id);
                                this.sync_inputs_from_active_tab(cx);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child("x"),
                    ),
            );
        }

        tabs_row = tabs_row.child(
            ui_button("tab-open-repo", "+ Open Repo", &theme, false).on_click(cx.listener(
                |this, _ev, window, cx| {
                    this.presenter
                        .execute_palette_action(PaletteActionKind::OpenRepositoryDialog);
                    this.modal_primary_input
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                    let focus = this.modal_primary_input.read(cx).focus_handle(cx);
                    window.focus(&focus);
                    cx.notify();
                },
            )),
        );

        let theme_label = match theme.appearance {
            ThemeAppearance::Dark => "Light Theme",
            ThemeAppearance::Light => "Dark Theme",
        };
        let right_actions = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                ui_button("tb-palette", "Palette", &theme, false).on_click(cx.listener(
                    |this, _ev, window, cx| {
                        this.open_command_palette(window, cx);
                    },
                )),
            )
            .child(
                ui_button("tb-theme", theme_label, &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter
                            .execute_palette_action(PaletteActionKind::ToggleThemeAppearance);
                        cx.notify();
                    },
                )),
            );

        bar = bar.child(tabs_row).child(right_actions);
        bar.into_any_element()
    }

    fn render_action_toolbar(&mut self, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = self.presenter.theme;
        let op_banner = self
            .presenter
            .active_tab()
            .and_then(|t| operation_state_banner(t.snapshot.operation_state));
        let (is_pickaxe, is_regex, can_undo, can_redo) =
            self.presenter
                .active_tab()
                .map_or((false, false, false, false), |t| {
                    (
                        t.search_is_pickaxe,
                        t.search_is_regex,
                        t.snapshot.can_undo,
                        t.snapshot.can_redo,
                    )
                });
        let term_active = self.presenter.bottom_drawer_open
            && self.presenter.bottom_drawer_tab == BottomDrawerTab::Terminal;

        let mut left = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                ui_button(
                    "act-sidebar",
                    "Sidebar",
                    &theme,
                    self.presenter.sidebar_visible,
                )
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    this.presenter.sidebar_visible = !this.presenter.sidebar_visible;
                    cx.notify();
                })),
            )
            .child(
                ui_button("act-undo", "Undo", &theme, can_undo).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.undo_last_operation(false);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-redo", "Redo", &theme, can_redo).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.redo_last_operation(false);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-fetch", "Fetch", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.fetch_all();
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-pull", "Pull", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.pull_current(false);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-push", "Push", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.push_current();
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-force-push", "Force Push...", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.request_force_push();
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-branch", "Branch", &theme, false).on_click(cx.listener(
                    |this, _ev, window, cx| {
                        this.presenter
                            .execute_palette_action(PaletteActionKind::OpenCreateBranchDialog);
                        this.modal_primary_input
                            .update(cx, |f, cx| f.set_text(String::new(), cx));
                        let focus = this.modal_primary_input.read(cx).focus_handle(cx);
                        window.focus(&focus);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-rebase", "Rebase -i", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.open_interactive_rebase_for_active_branch();
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-stash", "Stash", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter
                            .execute_palette_action(PaletteActionKind::StashPush);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-pop", "Pop", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter
                            .execute_palette_action(PaletteActionKind::StashPop);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button("act-terminal", "Terminal", &theme, term_active).on_click(cx.listener(
                    |this, _ev, window, cx| {
                        if this.presenter.bottom_drawer_open
                            && this.presenter.bottom_drawer_tab == BottomDrawerTab::Terminal
                        {
                            this.presenter.bottom_drawer_open = false;
                        } else {
                            this.presenter.open_or_focus_terminal();
                            let focus = this.terminal_input.read(cx).focus_handle(cx);
                            window.focus(&focus);
                        }
                        cx.notify();
                    },
                )),
            );

        if let Some(banner) = op_banner {
            left = left.child(ui_pill(banner, theme.conflicted_bg, theme.conflicted));
        }

        let search_controls = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(div().w(px(240.0)).child(self.search_input.clone()))
            .child(
                ui_button("search-pickaxe-toggle", "Pickaxe -S", &theme, is_pickaxe).on_click(
                    cx.listener(|this, _ev, _w, cx| {
                        let q = this.search_input.read(cx).text().to_string();
                        let (cur_pk, cur_rx) = this
                            .presenter
                            .active_tab()
                            .map_or((false, false), |t| (t.search_is_pickaxe, t.search_is_regex));
                        this.presenter.set_search_filter(q, !cur_pk, cur_rx);
                        cx.notify();
                    }),
                ),
            )
            .child(
                ui_button("search-regex-toggle", "Regex", &theme, is_regex).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        let q = this.search_input.read(cx).text().to_string();
                        let (cur_pk, cur_rx) = this
                            .presenter
                            .active_tab()
                            .map_or((false, false), |t| (t.search_is_pickaxe, t.search_is_regex));
                        this.presenter.set_search_filter(q, cur_pk, !cur_rx);
                        cx.notify();
                    },
                )),
            )
            .child(
                ui_button(
                    "act-drawer",
                    "CLI Output",
                    &theme,
                    self.presenter.bottom_drawer_open,
                )
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    this.presenter.bottom_drawer_open = !this.presenter.bottom_drawer_open;
                    cx.notify();
                })),
            )
            .child(
                ui_button(
                    "act-inspector",
                    "Inspector",
                    &theme,
                    self.presenter.inspector_visible,
                )
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    this.presenter.inspector_visible = !this.presenter.inspector_visible;
                    cx.notify();
                })),
            );

        div()
            .h(px(40.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .bg(to_rgba(theme.bg_surface))
            .border_b_1()
            .border_color(to_rgba(theme.border_subtle))
            .child(left)
            .child(search_controls)
            .into_any_element()
    }

    fn render_center_pane(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> AnyElement {
        let has_conflict_editor = self
            .presenter
            .active_tab()
            .is_some_and(|t| t.conflict_session.is_some());
        let has_diff_preview = self
            .presenter
            .active_tab()
            .is_some_and(|t| t.selected_delta.is_some() || t.diff_notice.is_some());
        let force_diff_view = matches!(
            self.presenter.diff_view_mode,
            DiffViewMode::ThreeWayMergeEditor
        );

        let graph = render_graph_canvas(self, self.graph_scroll_handle.clone(), cx);

        if has_conflict_editor || force_diff_view {
            let merge_el = render_merge_editor_pane(self, cx);
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .child(div().h_1_3().w_full().child(graph))
                .child(div().flex_1().w_full().child(merge_el))
                .into_any_element();
        }

        if has_diff_preview {
            let diff_el = render_diff_pane(self, window, cx);
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .child(div().h_1_2().w_full().child(graph))
                .child(div().h_1_2().w_full().child(diff_el))
                .into_any_element();
        }

        graph
    }

    fn render_bottom_drawer(&self, cx: &mut Context<'_, Self>) -> Option<AnyElement> {
        if !self.presenter.bottom_drawer_open {
            return None;
        }
        let theme = self.presenter.theme;
        let drawer_tab = self.presenter.bottom_drawer_tab;
        let tab = self.presenter.active_tab()?;

        let body_element: AnyElement = match drawer_tab {
            BottomDrawerTab::GitOutput => {
                let mut logs_col = div()
                    .id("cli-drawer-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .gap(px(2.0));

                for entry in tab.cli_logs.iter().rev().take(200).rev() {
                    logs_col = logs_col.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(to_rgba(if entry.is_stderr {
                                theme.warning
                            } else {
                                theme.text_primary
                            }))
                            .child(entry.text.clone()),
                    );
                }
                logs_col.into_any_element()
            }
            BottomDrawerTab::Terminal => {
                let mut rows_col = div()
                    .id("pty-viewport-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .p(px(8.0))
                    .gap(px(1.0))
                    .bg(to_rgba(theme.bg_base));

                if let Some(ref snap) = tab.terminal_snapshot {
                    let cols = usize::from(snap.size.cols.max(1));
                    for r in 0..usize::from(snap.size.rows) {
                        let start = r * cols;
                        let end = (start + cols).min(snap.cells.len());
                        let line_text: String = snap.cells[start..end]
                            .iter()
                            .map(|c| c.ch)
                            .collect::<String>()
                            .trim_end()
                            .to_string();
                        rows_col = rows_col.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(to_rgba(theme.text_primary))
                                .child(if line_text.is_empty() {
                                    " ".to_string()
                                } else {
                                    line_text
                                }),
                        );
                    }
                } else {
                    rows_col = rows_col.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(to_rgba(theme.text_muted))
                            .child("Spawning OS shell PTY..."),
                    );
                }

                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(rows_col)
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(6.0))
                            .border_t_1()
                            .border_color(to_rgba(theme.border_subtle))
                            .child(self.terminal_input.clone()),
                    )
                    .into_any_element()
            }
        };

        let mut hdr_right = div().flex().items_center().gap(px(8.0));
        if let Some((ref phase, pct)) = tab.cli_transfer_progress {
            hdr_right = hdr_right.child(ui_pill(
                &format!("{phase}: {pct}%"),
                theme.bg_elevated,
                theme.accent_primary,
            ));
        }
        if drawer_tab == BottomDrawerTab::Terminal && tab.terminal_session.is_some() {
            hdr_right = hdr_right.child(
                ui_button("pty-kill-btn", "Kill Shell", &theme, false).on_click(cx.listener(
                    |this, _ev, _w, cx| {
                        this.presenter.close_terminal_session();
                        cx.notify();
                    },
                )),
            );
        }
        if tab.cli_running {
            hdr_right = hdr_right.child(
                ui_button("cli-cancel-btn", "Cancel Git Process", &theme, true).on_click(
                    cx.listener(|this, _ev, _w, cx| {
                        this.presenter.cancel_active_cli();
                        cx.notify();
                    }),
                ),
            );
        }
        hdr_right = hdr_right.child(ui_button("cli-close-btn", "Hide", &theme, false).on_click(
            cx.listener(|this, _ev, _w, cx| {
                this.presenter.bottom_drawer_open = false;
                cx.notify();
            }),
        ));

        let term_label = format!("Terminal ({})", tab.terminal_title);
        let hdr_left = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                ui_button(
                    "drawer-tab-cli",
                    "Git Output",
                    &theme,
                    drawer_tab == BottomDrawerTab::GitOutput,
                )
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    this.presenter.bottom_drawer_tab = BottomDrawerTab::GitOutput;
                    cx.notify();
                })),
            )
            .child(
                ui_button(
                    "drawer-tab-pty",
                    &term_label,
                    &theme,
                    drawer_tab == BottomDrawerTab::Terminal,
                )
                .on_click(cx.listener(|this, _ev, window, cx| {
                    this.presenter.open_or_focus_terminal();
                    let focus = this.terminal_input.read(cx).focus_handle(cx);
                    window.focus(&focus);
                    cx.notify();
                })),
            );

        Some(
            div()
                .h(px(self.presenter.bottom_drawer_height_px))
                .w_full()
                .flex()
                .flex_col()
                .bg(to_rgba(theme.bg_surface))
                .border_t_1()
                .border_color(to_rgba(theme.border_subtle))
                .child(
                    div()
                        .h(px(30.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(to_rgba(theme.border_subtle))
                        .child(hdr_left)
                        .child(hdr_right),
                )
                .child(body_element)
                .into_any_element(),
        )
    }
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = self.presenter.theme;

        let tab_bar = self.render_tab_bar(cx);
        let toolbar = self.render_action_toolbar(cx);

        let mut body_row = div().flex().flex_1().w_full().overflow_hidden();
        if self.presenter.sidebar_visible {
            body_row = body_row.child(render_sidebar_pane(self, cx));
        }
        body_row = body_row.child(self.render_center_pane(window, cx));
        if self.presenter.inspector_visible {
            body_row = body_row.child(render_inspector_pane(self, cx));
        }

        let mut root = div()
            .id("workspace-root")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(to_rgba(theme.bg_base))
            .text_color(to_rgba(theme.text_primary))
            .on_action(cx.listener(|this, _ev: &ToggleCommandPalette, window, cx| {
                this.open_command_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _ev: &CommitStagedAction, _w, cx| {
                this.presenter.commit_staged();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _ev: &FocusSearchAction, window, cx| {
                let focus = this.search_input.read(cx).focus_handle(cx);
                window.focus(&focus);
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _ev: &OpenBranchDialogAction, window, cx| {
                    this.presenter
                        .execute_palette_action(PaletteActionKind::OpenCreateBranchDialog);
                    this.modal_primary_input
                        .update(cx, |f, cx| f.set_text(String::new(), cx));
                    let focus = this.modal_primary_input.read(cx).focus_handle(cx);
                    window.focus(&focus);
                    cx.notify();
                }),
            )
            .on_action(cx.listener(|this, _ev: &OpenRepoDialogAction, window, cx| {
                this.presenter
                    .execute_palette_action(PaletteActionKind::OpenRepositoryDialog);
                this.modal_primary_input
                    .update(cx, |f, cx| f.set_text(String::new(), cx));
                let focus = this.modal_primary_input.read(cx).focus_handle(cx);
                window.focus(&focus);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _ev: &ToggleDrawerAction, _w, cx| {
                this.presenter.bottom_drawer_open = !this.presenter.bottom_drawer_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _ev: &UndoAction, _w, cx| {
                this.presenter.undo_last_operation(false);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _ev: &RedoAction, _w, cx| {
                this.presenter.redo_last_operation(false);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _ev: &CloseActiveModal, _w, cx| {
                this.presenter.modal = ActiveModal::None;
                cx.notify();
            }))
            .on_action(cx.listener(|_this, _ev: &QuitApp, _w, cx| {
                cx.quit();
            }))
            .child(tab_bar)
            .child(toolbar)
            .child(body_row);

        if let Some(drawer) = self.render_bottom_drawer(cx) {
            root = root.child(drawer);
        }

        if let Some(ref banner) = self.presenter.status_banner {
            let banner_col = if banner.is_error {
                theme.error
            } else {
                theme.added
            };
            let banner_bg = if banner.is_error {
                theme.conflicted_bg
            } else {
                theme.added_bg
            };
            root = root.child(
                div()
                    .h(px(26.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(to_rgba(banner_bg))
                    .border_t_1()
                    .border_color(to_rgba(theme.border_subtle))
                    .text_size(px(11.5))
                    .text_color(to_rgba(banner_col))
                    .child(banner.message.clone())
                    .child(
                        ui_button("dismiss-banner", "Dismiss", &theme, false).on_click(
                            cx.listener(|this, _ev, _w, cx| {
                                this.presenter.status_banner = None;
                                cx.notify();
                            }),
                        ),
                    ),
            );
        }

        if let Some(overlay) = render_active_modal_overlay(self, cx) {
            root = root.child(overlay);
        }

        root
    }
}
