//! Branvio application UI shell, UI-agnostic Presenter ports, and fast-path Git CLI helpers.
//!
//! All business logic resides in independent core crates (`branvio-core`, `git-engine`,
//! `graph-layout`, `diff-merge`, `oplog-undo`, `fs-watcher`, `terminal-pty`, `forge-client`).
//! `app-ui` implements [`UiRuntimeBackend`] for `gpui` + `wgpu` (`gpui-backend` feature),
//! while enabling instant swapping to `Iced` or `Tauri v2` via [`CoreAppHost`].

pub mod cli_helper;
#[cfg(feature = "gpui-backend")]
pub mod gpui_shell;
pub mod ports;
pub mod presenter;
pub mod theme;

#[cfg(feature = "gpui-backend")]
pub use gpui_shell::GpuiRuntimeBackend;
pub use ports::{
    CoreAppHost, DiffMergePort, DiffViewMode, DragDropPreviewState, FileListLayoutMode,
    GraphInteractionAction, InspectorMode, PaletteCommandItem, UiBackendError, UiRuntimeBackend,
};
pub use presenter::BranvioPresenter;
pub use theme::{BranvioTheme, ThemeAppearance};
