//! Cross-platform OS adaptations for environment hydration, shortcuts, and display scaling.

pub mod env_hydration;
pub mod keymap;

pub use env_hydration::{
    HydratedGitEnv, SshAgentKind, SubprocessEnvOptions, hydrate_git_environment,
};
pub use keymap::{DisplayScalePolicy, PlatformShortcutModifier, ShortcutChord};
