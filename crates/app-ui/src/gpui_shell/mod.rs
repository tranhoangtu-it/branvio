//! Pure-Rust native GPU shell powered by `gpui` (`wgpu` / Metal / Vulkan / DirectX 12)
//! implementing [`UiRuntimeBackend`] without leaking GUI types into Core crates.

pub mod diff_pane;
pub mod graph_canvas;
pub mod inspector_pane;
pub mod merge_editor_pane;
pub mod modals;
pub mod sidebar_pane;
pub mod text_field;
pub mod workspace_view;

use crate::ports::{CoreAppHost, UiBackendError, UiRuntimeBackend};
use crate::theme::BranvioTheme;
use branvio_core::ids::{RepoEpoch, RepoId};
use branvio_core::pipeline::UiWakeupSink;
use branvio_core::platform::PlatformShortcutModifier;
use gpui::{
    AnyElement, App, AppContext, Application, Bounds, BoxShadow, Div, Hsla, InteractiveElement,
    KeyBinding, ParentElement, Pixels, Rgba, Styled, WindowBounds, WindowOptions, div, point, px,
    rgba, size,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

#[allow(missing_docs)]
mod key_actions {
    gpui::actions!(
        branvio,
        [
            ToggleCommandPalette,
            CommitStagedAction,
            FocusSearchAction,
            OpenBranchDialogAction,
            OpenRepoDialogAction,
            ToggleDrawerAction,
            UndoAction,
            RedoAction,
            CloseActiveModal,
            QuitApp,
        ]
    );
}
pub(crate) use key_actions::{
    CloseActiveModal, CommitStagedAction, FocusSearchAction, OpenBranchDialogAction,
    OpenRepoDialogAction, QuitApp, RedoAction, ToggleCommandPalette, ToggleDrawerAction,
    UndoAction,
};

/// Converts packed `0xRRGGBBAA` theme integer to [`gpui::Rgba`].
#[inline]
pub fn to_rgba(hex_rgba: u32) -> Rgba {
    rgba(hex_rgba)
}

/// Converts packed `0xRRGGBBAA` theme integer to [`gpui::Hsla`].
#[inline]
pub fn to_hsla(hex_rgba: u32) -> Hsla {
    rgba(hex_rgba).into()
}

/// Coalescing async wakeup sink forwarding Core [`UiWakeupSink`] notifications into GPUI's event loop.
pub struct GpuiWakeupBridge {
    pending: AtomicBool,
    tx: mpsc::UnboundedSender<()>,
}

impl GpuiWakeupBridge {
    /// Creates a `(bridge, rx)` pair whose receiver wakes GPUI's `AsyncApp` pump task.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let bridge = Arc::new(Self {
            pending: AtomicBool::new(false),
            tx,
        });
        (bridge, rx)
    }

    /// Clears the coalescing latch right before draining presenter events on the UI thread.
    #[inline]
    pub fn clear_pending(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

impl UiWakeupSink for GpuiWakeupBridge {
    fn notify_repo_updated(&self, _repo_id: RepoId, _epoch: RepoEpoch) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(());
        }
    }
}

/// Native GPUI runtime backend (`PURE_RUST_GPUI`).
pub struct GpuiRuntimeBackend {
    wakeup: Arc<GpuiWakeupBridge>,
    wakeup_rx: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<()>>>>,
}

impl GpuiRuntimeBackend {
    /// Constructs a new [`GpuiRuntimeBackend`] wired with an async [`GpuiWakeupBridge`].
    pub fn new() -> Self {
        let (wakeup, rx) = GpuiWakeupBridge::new();
        Self {
            wakeup,
            wakeup_rx: Arc::new(std::sync::Mutex::new(Some(rx))),
        }
    }
}

impl Default for GpuiRuntimeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl UiRuntimeBackend for GpuiRuntimeBackend {
    fn backend_name(&self) -> &'static str {
        "PURE_RUST_GPUI (gpui + wgpu)"
    }

    fn create_wakeup_sink(&self) -> Arc<dyn UiWakeupSink> {
        Arc::clone(&self.wakeup) as Arc<dyn UiWakeupSink>
    }

    fn run_event_loop(self: Box<Self>, host: CoreAppHost) -> Result<(), UiBackendError> {
        let wakeup = Arc::clone(&self.wakeup);
        let wakeup_rx = self
            .wakeup_rx
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());

        Application::new().run(move |cx: &mut App| {
            register_global_keybindings(cx);

            let bounds = Bounds::centered(None, size(px(1480.0), px(920.0)), cx);
            let mut wakeup_rx_cell = wakeup_rx;
            let host_opt = std::cell::RefCell::new(Some(host));

            let open_res = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // Matches `StartupWMClass=branvio` in the Linux .desktop entry.
                    app_id: Some("branvio".to_string()),
                    ..Default::default()
                },
                move |window, cx| {
                    let initial_host = host_opt
                        .borrow_mut()
                        .take()
                        .unwrap_or_else(unreachable_fallback_host);
                    let rx = wakeup_rx_cell.take();
                    cx.new(|cx| {
                        workspace_view::WorkspaceView::new(
                            initial_host,
                            Arc::clone(&wakeup),
                            rx,
                            window,
                            cx,
                        )
                    })
                },
            );

            if let Err(err) = open_res {
                tracing::error!("failed to open GPUI window: {err}");
            }
            cx.activate(true);

            if let Ok(raw_ms) = std::env::var("BRANVIO_SMOKE_EXIT_AFTER_MS")
                && let Ok(ms) = raw_ms.trim().parse::<u64>()
            {
                cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(ms))
                        .await;
                    let _ = cx.update(|app| app.quit());
                })
                .detach();
            }
        });

        Ok(())
    }
}

fn unreachable_fallback_host() -> CoreAppHost {
    let rt = tokio::runtime::Handle::try_current().unwrap_or_else(|_| std::process::abort());
    let env = Arc::new(branvio_core::platform::HydratedGitEnv::from_current_process());
    let engine: Arc<dyn git_engine::HybridGitEngine> =
        Arc::new(git_engine::BranvioGitEngine::new(Arc::clone(&env), None));
    CoreAppHost::new(rt, env, engine, Vec::new())
}

fn register_global_keybindings(cx: &mut App) {
    let modifier = match PlatformShortcutModifier::host_primary() {
        PlatformShortcutModifier::Command => "cmd",
        PlatformShortcutModifier::Control => "ctrl",
    };

    cx.bind_keys([
        KeyBinding::new(&format!("{modifier}-p"), ToggleCommandPalette, None),
        KeyBinding::new(&format!("{modifier}-enter"), CommitStagedAction, None),
        KeyBinding::new(&format!("{modifier}-f"), FocusSearchAction, None),
        KeyBinding::new(&format!("{modifier}-b"), OpenBranchDialogAction, None),
        KeyBinding::new(&format!("{modifier}-o"), OpenRepoDialogAction, None),
        KeyBinding::new(&format!("{modifier}-`"), ToggleDrawerAction, None),
        KeyBinding::new(&format!("{modifier}-z"), UndoAction, None),
        KeyBinding::new(&format!("{modifier}-shift-z"), RedoAction, None),
        KeyBinding::new("ctrl-y", RedoAction, None),
        KeyBinding::new(&format!("{modifier}-q"), QuitApp, None),
        KeyBinding::new("escape", CloseActiveModal, None),
    ]);
}

/// Shared compact styled pill button builder used consistently across toolbars, panes, and modals.
pub fn ui_button(
    id: impl Into<gpui::ElementId>,
    label: &str,
    theme: &BranvioTheme,
    primary: bool,
) -> gpui::Stateful<Div> {
    let (bg, border, text_col) = if primary {
        (theme.accent_primary, theme.accent_primary, 0x0B0F17FF)
    } else {
        (theme.bg_elevated, theme.border_subtle, theme.text_primary)
    };

    div()
        .id(id)
        .px(px(9.0))
        .py(px(4.0))
        .rounded(px(5.0))
        .bg(to_rgba(bg))
        .border_1()
        .border_color(to_rgba(border))
        .text_size(px(12.0))
        .text_color(to_rgba(text_col))
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .child(label.to_string())
}

/// Shared compact badge pill rendered next to commits, files, or modal options.
pub fn ui_pill(label: &str, bg_rgba: u32, fg_rgba: u32) -> Div {
    div()
        .px(px(6.0))
        .py(px(1.5))
        .rounded(px(4.0))
        .bg(to_rgba(bg_rgba))
        .text_size(px(11.0))
        .text_color(to_rgba(fg_rgba))
        .child(label.to_string())
}

/// Shared modal card container with crisp border and native drop shadow.
pub fn modal_card_shell(width_px: Pixels, theme: &BranvioTheme, content: AnyElement) -> Div {
    div()
        .w(width_px)
        .bg(to_rgba(theme.bg_surface))
        .border_1()
        .border_color(to_rgba(theme.border_subtle))
        .rounded(px(8.0))
        .shadow(vec![BoxShadow {
            color: to_hsla(0x00000088),
            offset: point(px(0.0), px(8.0)),
            blur_radius: px(24.0),
            spread_radius: px(0.0),
        }])
        .p(px(14.0))
        .child(content)
}
