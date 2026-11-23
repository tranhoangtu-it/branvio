//! Cross-platform integrated terminal PTY engine backed by `portable-pty` and `alacritty_terminal`.

pub mod backend;
pub mod traits;

pub use backend::{AlacrittyPtySession, PortablePtyBackend};
pub use traits::{
    PtyCellAttr, PtyGridCell, PtySize, ShellProfile, TerminalBackend, TerminalDamageSnapshot,
    TerminalError, TerminalResult, TerminalSession,
};
