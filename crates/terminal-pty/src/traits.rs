//! Traits and viewport-grid snapshot contracts for the embedded terminal emulator.

use async_trait::async_trait;
use branvio_core::paths::HostPath;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

/// Result alias for `terminal-pty`.
pub type TerminalResult<T> = Result<T, TerminalError>;

/// Errors produced when spawning or communicating with an embedded OS shell PTY.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// Failed to allocate PTY or launch shell executable.
    #[error("failed to spawn PTY shell '{shell}': {reason}")]
    SpawnFailed {
        /// Shell command attempted.
        shell: String,
        /// Underlying OS/PTY error.
        reason: String,
    },

    /// PTY session has already terminated or closed its I/O pipe.
    #[error("terminal PTY session closed")]
    SessionClosed,
}

/// Terminal viewport geometry in character grid cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    /// Visible column count.
    pub cols: u16,
    /// Visible row count.
    pub rows: u16,
    /// Pixel width of a single monospace cell (for Sixel/OSC sizing).
    pub cell_width_px: u16,
    /// Pixel height of a single monospace cell.
    pub cell_height_px: u16,
}

/// Resolved native shell profile for Windows (`pwsh`/`powershell`/`cmd`/WSL), macOS (`zsh`), or Linux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellProfile {
    /// Human-readable label shown in terminal tab selector.
    pub label: CompactString,
    /// Executable path or command name.
    pub program: String,
    /// Startup arguments (e.g., `["-l"]` or `["-NoLogo"]`).
    pub args: Vec<String>,
    /// Initial working directory (normally the active repository root).
    pub working_dir: HostPath,
}

/// Packed ANSI styling attributes for a single grid cell rendered by `app-ui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyCellAttr {
    /// Foreground RGBA (`0xRRGGBBAA`).
    pub fg_rgba: u32,
    /// Background RGBA (`0xRRGGBBAA`).
    pub bg_rgba: u32,
    /// Bold weight flag.
    pub bold: bool,
    /// Italic slant flag.
    pub italic: bool,
    /// Underline flag.
    pub underline: bool,
}

/// Single character cell inside the terminal viewport snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyGridCell {
    /// Unicode scalar rendered in this cell.
    pub ch: char,
    /// Cell visual styling.
    pub attr: PtyCellAttr,
}

/// Frame snapshot of the visible terminal rows produced by `alacritty_terminal`.
#[derive(Debug, Clone)]
pub struct TerminalDamageSnapshot {
    /// Viewport dimensions matching `cells.len() == cols * rows`.
    pub size: PtySize,
    /// Row-major grid of visible cells.
    pub cells: Vec<PtyGridCell>,
    /// Cursor position `(col, row)` within viewport, if visible.
    pub cursor: Option<(u16, u16)>,
    /// Current terminal window title set by OSC escape sequences.
    pub title: CompactString,
}

/// Live handle to an active PTY shell session.
#[async_trait]
pub trait TerminalSession: Send + Sync {
    /// Writes raw UTF-8/VT keystrokes to the shell stdin.
    async fn write_bytes(&self, bytes: &[u8]) -> TerminalResult<()>;

    /// Resizes both the OS PTY and the `alacritty_terminal` grid state.
    async fn resize(&self, size: PtySize) -> TerminalResult<()>;

    /// Extracts the current visible cell grid snapshot for GPU rendering.
    fn snapshot_viewport(&self) -> TerminalResult<TerminalDamageSnapshot>;

    /// Gracefully terminates the PTY child process.
    async fn close(&self) -> TerminalResult<()>;
}

/// Factory discovering OS shells and spawning repository-scoped PTY sessions.
#[async_trait]
pub trait TerminalBackend: Send + Sync {
    /// Enumerates available shells on the current OS (PowerShell, CMD, WSL distros, zsh, bash, fish).
    fn detect_available_shells(&self, default_cwd: &HostPath) -> Vec<ShellProfile>;

    /// Spawns a shell session and returns both the session handle and a damage-notification stream.
    async fn spawn_session(
        &self,
        profile: ShellProfile,
        initial_size: PtySize,
    ) -> TerminalResult<(Box<dyn TerminalSession>, mpsc::Receiver<()>)>;
}
