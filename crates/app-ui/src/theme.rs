//! UI-agnostic color palette and semantic Git/AST syntax token styling (`Dark` and `Light`).
//!
//! Colors are stored as packed `0xRRGGBBAA` integers so both headless presenter tests and
//! GPUI / Iced / Tauri renderers share deterministic visual tokens without pulling GUI crates.

use branvio_core::domain::{DeltaStatus, DeterministicColor, SyntaxTokenKind};

/// Active visual theme appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemeAppearance {
    /// High-contrast dark workspace modeled after modern Git GUIs.
    #[default]
    Dark,
    /// Crisp light workspace for bright environments.
    Light,
}

impl ThemeAppearance {
    /// Toggles between [`ThemeAppearance::Dark`] and [`ThemeAppearance::Light`].
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// Converts a packed `0xRRGGBB` value with full opacity (`0xFF`) into `0xRRGGBBAA`.
#[inline]
pub const fn rgb_hex(rgb: u32) -> u32 {
    (rgb << 8) | 0xFF
}

/// Converts a packed `0xRRGGBB` value and byte alpha into `0xRRGGBBAA`.
#[inline]
pub const fn rgba_hex(rgb: u32, alpha: u8) -> u32 {
    (rgb << 8) | (alpha as u32)
}

/// Complete semantic theme palette for Branvio (zero GUI-framework dependencies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranvioTheme {
    /// Selected appearance polarity.
    pub appearance: ThemeAppearance,
    /// Main window & graph canvas backdrop (`0xRRGGBBAA`).
    pub bg_base: u32,
    /// Sidebar, diff gutter, and tab-bar surface (`0xRRGGBBAA`).
    pub bg_surface: u32,
    /// Elevated popovers, modals, and toolbar inputs (`0xRRGGBBAA`).
    pub bg_elevated: u32,
    /// Subtle row hover fill (`0xRRGGBBAA`).
    pub bg_hover: u32,
    /// Selected commit/file row tint (`0xRRGGBBAA`).
    pub bg_selected: u32,
    /// Crisp 1px panel/divider stroke (`0xRRGGBBAA`).
    pub border_subtle: u32,
    /// Focused input or active drop-target border (`0xRRGGBBAA`).
    pub border_focus: u32,
    /// Primary foreground text (`0xRRGGBBAA`).
    pub text_primary: u32,
    /// Secondary labels (authors, timestamps, paths) (`0xRRGGBBAA`).
    pub text_secondary: u32,
    /// Muted line numbers and hints (`0xRRGGBBAA`).
    pub text_muted: u32,
    /// Primary interactive accent (`0xRRGGBBAA`).
    pub accent_primary: u32,
    /// Secondary highlight / tag accent (`0xRRGGBBAA`).
    pub accent_secondary: u32,
    /// Added file / addition line foreground (`0xRRGGBBAA`).
    pub added: u32,
    /// Addition diff line fill (`0xRRGGBBAA`).
    pub added_bg: u32,
    /// Intra-line added word highlight fill (`0xRRGGBBAA`).
    pub added_word_bg: u32,
    /// Deleted file / deletion line foreground (`0xRRGGBBAA`).
    pub deleted: u32,
    /// Deletion diff line fill (`0xRRGGBBAA`).
    pub deleted_bg: u32,
    /// Intra-line removed word highlight fill (`0xRRGGBBAA`).
    pub deleted_word_bg: u32,
    /// Modified file / hunk indicator (`0xRRGGBBAA`).
    pub modified: u32,
    /// Conflicted state foreground (`0xRRGGBBAA`).
    pub conflicted: u32,
    /// Unresolved conflict block background (`0xRRGGBBAA`).
    pub conflicted_bg: u32,
    /// Warning badge / predictive conflict alert (`0xRRGGBBAA`).
    pub warning: u32,
    /// Error toast / invalid action (`0xRRGGBBAA`).
    pub error: u32,
    /// Virtual WIP node outline/fill accent (`0xRRGGBBAA`).
    pub wip_node: u32,
    /// `HEAD` badge and active branch indicator (`0xRRGGBBAA`).
    pub head_accent: u32,
}

impl BranvioTheme {
    /// Returns the built-in dark theme palette.
    pub const fn dark() -> Self {
        Self {
            appearance: ThemeAppearance::Dark,
            bg_base: rgb_hex(0x13161D),
            bg_surface: rgb_hex(0x191D26),
            bg_elevated: rgb_hex(0x222834),
            bg_hover: rgba_hex(0xFFFFFF, 0x10),
            bg_selected: rgba_hex(0x3B82F6, 0x2E),
            border_subtle: rgb_hex(0x2B3242),
            border_focus: rgb_hex(0x3B82F6),
            text_primary: rgb_hex(0xE6EDF3),
            text_secondary: rgb_hex(0x9BA7B8),
            text_muted: rgb_hex(0x637083),
            accent_primary: rgb_hex(0x38BDF8),
            accent_secondary: rgb_hex(0xA78BFA),
            added: rgb_hex(0x3FB950),
            added_bg: rgba_hex(0x2EA043, 0x22),
            added_word_bg: rgba_hex(0x2EA043, 0x52),
            deleted: rgb_hex(0xF85149),
            deleted_bg: rgba_hex(0xF85149, 0x22),
            deleted_word_bg: rgba_hex(0xF85149, 0x52),
            modified: rgb_hex(0xE3B341),
            conflicted: rgb_hex(0xFB923C),
            conflicted_bg: rgba_hex(0xEA580C, 0x2A),
            warning: rgb_hex(0xFBBF24),
            error: rgb_hex(0xEF4444),
            wip_node: rgb_hex(0xF59E0B),
            head_accent: rgb_hex(0x22D3EE),
        }
    }

    /// Returns the built-in light theme palette.
    pub const fn light() -> Self {
        Self {
            appearance: ThemeAppearance::Light,
            bg_base: rgb_hex(0xF8FAFC),
            bg_surface: rgb_hex(0xF1F5F9),
            bg_elevated: rgb_hex(0xFFFFFF),
            bg_hover: rgba_hex(0x0F172A, 0x0D),
            bg_selected: rgba_hex(0x2563EB, 0x1E),
            border_subtle: rgb_hex(0xCBD5E1),
            border_focus: rgb_hex(0x2563EB),
            text_primary: rgb_hex(0x0F172A),
            text_secondary: rgb_hex(0x475569),
            text_muted: rgb_hex(0x94A3B8),
            accent_primary: rgb_hex(0x0284C7),
            accent_secondary: rgb_hex(0x7C3AED),
            added: rgb_hex(0x16A34A),
            added_bg: rgba_hex(0x22C55E, 0x22),
            added_word_bg: rgba_hex(0x22C55E, 0x4C),
            deleted: rgb_hex(0xDC2626),
            deleted_bg: rgba_hex(0xEF4444, 0x20),
            deleted_word_bg: rgba_hex(0xEF4444, 0x48),
            modified: rgb_hex(0xD97706),
            conflicted: rgb_hex(0xEA580C),
            conflicted_bg: rgba_hex(0xFB923C, 0x26),
            warning: rgb_hex(0xCA8A04),
            error: rgb_hex(0xDC2626),
            wip_node: rgb_hex(0xD97706),
            head_accent: rgb_hex(0x0891B2),
        }
    }

    /// Constructs a theme matching `appearance`.
    #[inline]
    pub const fn for_appearance(appearance: ThemeAppearance) -> Self {
        match appearance {
            ThemeAppearance::Dark => Self::dark(),
            ThemeAppearance::Light => Self::light(),
        }
    }

    /// Maps a [`SyntaxTokenKind`] emitted by `tree-sitter` to a packed `0xRRGGBBAA` color.
    pub const fn syntax_token_rgba(&self, kind: SyntaxTokenKind) -> u32 {
        match self.appearance {
            ThemeAppearance::Dark => match kind {
                SyntaxTokenKind::Keyword => rgb_hex(0xFF7B72),
                SyntaxTokenKind::Function => rgb_hex(0xD2A8FF),
                SyntaxTokenKind::Type => rgb_hex(0xFFA657),
                SyntaxTokenKind::Variable => rgb_hex(0xE6EDF3),
                SyntaxTokenKind::Property => rgb_hex(0x7EE787),
                SyntaxTokenKind::String => rgb_hex(0xA5D6FF),
                SyntaxTokenKind::Number => rgb_hex(0x79C0FF),
                SyntaxTokenKind::Comment => rgb_hex(0x8B949E),
                SyntaxTokenKind::Operator => rgb_hex(0xFF7B72),
                SyntaxTokenKind::Punctuation => rgb_hex(0x9BA7B8),
                SyntaxTokenKind::Macro => rgb_hex(0xFFA657),
                SyntaxTokenKind::Attribute => rgb_hex(0xD2A8FF),
                SyntaxTokenKind::Constant => rgb_hex(0x79C0FF),
            },
            ThemeAppearance::Light => match kind {
                SyntaxTokenKind::Keyword => rgb_hex(0xCF222E),
                SyntaxTokenKind::Function => rgb_hex(0x8250DF),
                SyntaxTokenKind::Type => rgb_hex(0x953800),
                SyntaxTokenKind::Variable => rgb_hex(0x24292F),
                SyntaxTokenKind::Property => rgb_hex(0x116329),
                SyntaxTokenKind::String => rgb_hex(0x0A3069),
                SyntaxTokenKind::Number => rgb_hex(0x0550AE),
                SyntaxTokenKind::Comment => rgb_hex(0x6E7781),
                SyntaxTokenKind::Operator => rgb_hex(0xCF222E),
                SyntaxTokenKind::Punctuation => rgb_hex(0x57606A),
                SyntaxTokenKind::Macro => rgb_hex(0x953800),
                SyntaxTokenKind::Attribute => rgb_hex(0x8250DF),
                SyntaxTokenKind::Constant => rgb_hex(0x0550AE),
            },
        }
    }

    /// Maps a [`DeltaStatus`] into a badge glyph and packed `0xRRGGBBAA` color.
    pub const fn delta_status_badge(&self, status: DeltaStatus) -> (&'static str, u32) {
        match status {
            DeltaStatus::Added => ("A", self.added),
            DeltaStatus::Deleted => ("D", self.deleted),
            DeltaStatus::Modified => ("M", self.modified),
            DeltaStatus::Renamed { .. } => ("R", self.accent_primary),
            DeltaStatus::Copied { .. } => ("C", self.accent_secondary),
            DeltaStatus::TypeChanged => ("T", self.warning),
            DeltaStatus::Untracked => ("?", self.added),
            DeltaStatus::Conflicted(_) => ("!", self.conflicted),
            DeltaStatus::Ignored => ("I", self.text_muted),
        }
    }

    /// Resolves a deterministic Metro [`DeterministicColor`] to packed `0xRRGGBBAA`.
    #[inline]
    pub const fn metro_lane_rgba(&self, color: DeterministicColor) -> u32 {
        color.to_rgba_u32()
    }

    /// Resolves a compact `palette_slot` (`0..16`) via [`graph_layout::palette_color`] to packed `0xRRGGBBAA`.
    #[inline]
    pub fn metro_slot_rgba(&self, palette_slot: u8) -> u32 {
        graph_layout::palette_color(palette_slot).to_rgba_u32()
    }
}

impl Default for BranvioTheme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_themes_have_distinct_surfaces() {
        let dark = BranvioTheme::dark();
        let light = BranvioTheme::light();
        assert_ne!(dark.bg_base, light.bg_base);
        assert_ne!(dark.text_primary, light.text_primary);
        assert_eq!(ThemeAppearance::Dark.toggled(), ThemeAppearance::Light);
        assert_eq!(
            BranvioTheme::for_appearance(ThemeAppearance::Light).appearance,
            ThemeAppearance::Light
        );
    }

    #[test]
    fn syntax_and_status_helpers_cover_all_variants() {
        let dark = BranvioTheme::dark();
        let kinds = [
            SyntaxTokenKind::Keyword,
            SyntaxTokenKind::Function,
            SyntaxTokenKind::Type,
            SyntaxTokenKind::Variable,
            SyntaxTokenKind::Property,
            SyntaxTokenKind::String,
            SyntaxTokenKind::Number,
            SyntaxTokenKind::Comment,
            SyntaxTokenKind::Operator,
            SyntaxTokenKind::Punctuation,
            SyntaxTokenKind::Macro,
            SyntaxTokenKind::Attribute,
            SyntaxTokenKind::Constant,
        ];
        for kind in kinds {
            let color = dark.syntax_token_rgba(kind);
            assert_eq!(color & 0xFF, 0xFF);
        }

        let (glyph, _) = dark.delta_status_badge(DeltaStatus::Renamed { similarity_pct: 95 });
        assert_eq!(glyph, "R");
        assert_eq!(dark.metro_slot_rgba(0) & 0xFF, 0xFF);
    }
}
