//! Cross-platform keyboard shortcut modifiers and HiDPI scaling policy.

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// Primary command modifier resolved per host OS (`Cmd` on macOS, `Ctrl` on Windows/Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformShortcutModifier {
    /// macOS `Command` (`⌘`) key.
    Command,
    /// Windows/Linux `Control` key.
    Control,
}

impl PlatformShortcutModifier {
    /// Returns the native primary modifier for the compiled target operating system.
    #[inline]
    pub const fn host_primary() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Command
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::Control
        }
    }

    /// Returns the UI display label (`⌘` vs `Ctrl`).
    #[inline]
    pub const fn display_label(self) -> &'static str {
        match self {
            Self::Command => "⌘",
            Self::Control => "Ctrl",
        }
    }
}

/// Platform-normalized shortcut descriptor for Command Palette and menu bindings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShortcutChord {
    /// Uses the OS primary modifier (`Cmd` on macOS, `Ctrl` on Windows/Linux).
    pub primary: bool,
    /// Requires `Shift` modifier.
    pub shift: bool,
    /// Requires `Alt`/`Option` modifier.
    pub alt: bool,
    /// Key identifier (e.g., `"P"`, `"Z"`, `"Enter"`).
    pub key: CompactString,
}

impl ShortcutChord {
    /// Formats the chord for human display using host OS conventions.
    pub fn format_for_host(&self) -> CompactString {
        let mut parts: Vec<&str> = Vec::with_capacity(4);
        if self.primary {
            parts.push(PlatformShortcutModifier::host_primary().display_label());
        }
        if self.alt {
            #[cfg(target_os = "macos")]
            parts.push("⌥");
            #[cfg(not(target_os = "macos"))]
            parts.push("Alt");
        }
        if self.shift {
            #[cfg(target_os = "macos")]
            parts.push("⇧");
            #[cfg(not(target_os = "macos"))]
            parts.push("Shift");
        }
        parts.push(self.key.as_str());

        #[cfg(target_os = "macos")]
        {
            CompactString::from(parts.concat())
        }
        #[cfg(not(target_os = "macos"))]
        {
            CompactString::from(parts.join("+"))
        }
    }
}

/// HiDPI / Retina / fractional scaling metrics provided to the graph and diff canvas renderers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplayScalePolicy {
    /// Logical-to-physical pixel ratio (e.g. `2.0` on Retina, `1.25`/`1.5` on Windows/Wayland).
    pub scale_factor: f32,
    /// Subpixel snapping unit (`1.0 / scale_factor`) so 1px graph rails stay razor-sharp.
    pub hairline_logical_px: f32,
}

impl DisplayScalePolicy {
    /// Creates a policy from a window's reported scale factor (clamped safely to `0.5..=4.0`).
    pub fn from_scale_factor(scale_factor: f32) -> Self {
        let clamped = scale_factor.clamp(0.5, 4.0);
        Self {
            scale_factor: clamped,
            hairline_logical_px: 1.0 / clamped,
        }
    }
}
