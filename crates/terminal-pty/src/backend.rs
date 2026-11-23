//! Cross-platform PTY backend driven by `portable-pty` and `alacritty_terminal`.

use crate::traits::{
    PtyCellAttr, PtyGridCell, PtySize, ShellProfile, TerminalBackend, TerminalDamageSnapshot,
    TerminalError, TerminalResult, TerminalSession,
};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use async_trait::async_trait;
use branvio_core::paths::HostPath;
use branvio_core::platform::{HydratedGitEnv, SubprocessEnvOptions};
use compact_str::CompactString;
use portable_pty::{
    ChildKiller, CommandBuilder, MasterPty, PtySize as PortablePtySize, native_pty_system,
};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const DEFAULT_FG_RGBA: u32 = 0xE2E8_F0FF;
const DEFAULT_BG_RGBA: u32 = 0x0F17_2AFF;

#[derive(Debug, Clone, Copy)]
struct GridDimensions {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Clone)]
struct AlacrittyEventProxy {
    title: Arc<Mutex<CompactString>>,
    damage_tx: mpsc::Sender<()>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

impl EventListener for AlacrittyEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(new_title) => {
                if let Ok(mut guard) = self.title.lock() {
                    *guard = CompactString::from(new_title);
                }
                let _ = self.damage_tx.try_send(());
            }
            Event::ResetTitle => {
                if let Ok(mut guard) = self.title.lock() {
                    *guard = CompactString::const_new("Terminal");
                }
                let _ = self.damage_tx.try_send(());
            }
            Event::PtyWrite(response) => {
                if let Ok(mut guard) = self.writer.lock()
                    && let Some(ref mut w) = *guard
                {
                    let _ = w.write_all(response.as_bytes());
                    let _ = w.flush();
                }
            }
            Event::Wakeup => {
                let _ = self.damage_tx.try_send(());
            }
            _ => {}
        }
    }
}

/// Live PTY session backed by `portable-pty` child process and `alacritty_terminal::Term`.
pub struct AlacrittyPtySession {
    term: Arc<Mutex<Term<AlacrittyEventProxy>>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    killer: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    alive: Arc<AtomicBool>,
    title: Arc<Mutex<CompactString>>,
    size: Arc<Mutex<PtySize>>,
}

impl AlacrittyPtySession {
    /// Returns `true` while the spawned shell process has not yet exited or been killed.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn terminate_sync(&self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut w_guard) = self.writer.lock() {
            *w_guard = None;
        }
        if let Ok(mut k_guard) = self.killer.lock()
            && let Some(ref mut killer) = *k_guard
        {
            let _ = killer.kill();
        }
        if let Ok(mut m_guard) = self.master.lock() {
            *m_guard = None;
        }
    }
}

impl Drop for AlacrittyPtySession {
    fn drop(&mut self) {
        self.terminate_sync();
    }
}

#[async_trait]
impl TerminalSession for AlacrittyPtySession {
    async fn write_bytes(&self, bytes: &[u8]) -> TerminalResult<()> {
        if !self.is_alive() {
            return Err(TerminalError::SessionClosed);
        }
        let data = bytes.to_vec();
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            let mut guard = writer.lock().map_err(|_| TerminalError::SessionClosed)?;
            let Some(ref mut w) = *guard else {
                return Err(TerminalError::SessionClosed);
            };
            w.write_all(&data)
                .map_err(|_| TerminalError::SessionClosed)?;
            w.flush().map_err(|_| TerminalError::SessionClosed)?;
            Ok(())
        })
        .await
        .map_err(|_| TerminalError::SessionClosed)?
    }

    async fn resize(&self, size: PtySize) -> TerminalResult<()> {
        if !self.is_alive() {
            return Err(TerminalError::SessionClosed);
        }
        let cols = size.cols.max(1);
        let rows = size.rows.max(1);
        let portable_size = PortablePtySize {
            rows,
            cols,
            pixel_width: cols.saturating_mul(size.cell_width_px.max(1)),
            pixel_height: rows.saturating_mul(size.cell_height_px.max(1)),
        };

        if let Ok(mut m_guard) = self.master.lock()
            && let Some(ref mut master) = *m_guard
        {
            master
                .resize(portable_size)
                .map_err(|_| TerminalError::SessionClosed)?;
        }
        if let Ok(mut t_guard) = self.term.lock() {
            t_guard.resize(GridDimensions {
                columns: usize::from(cols),
                screen_lines: usize::from(rows),
            });
        }
        if let Ok(mut s_guard) = self.size.lock() {
            *s_guard = PtySize { cols, rows, ..size };
        }
        Ok(())
    }

    fn snapshot_viewport(&self) -> TerminalResult<TerminalDamageSnapshot> {
        let size = *self.size.lock().map_err(|_| TerminalError::SessionClosed)?;
        let cols = usize::from(size.cols.max(1));
        let rows = usize::from(size.rows.max(1));
        let mut cells = vec![
            PtyGridCell {
                ch: ' ',
                attr: PtyCellAttr {
                    fg_rgba: DEFAULT_FG_RGBA,
                    bg_rgba: DEFAULT_BG_RGBA,
                    bold: false,
                    italic: false,
                    underline: false,
                },
            };
            cols * rows
        ];

        let cursor = {
            let term = self.term.lock().map_err(|_| TerminalError::SessionClosed)?;
            let content = term.renderable_content();
            let offset_i32 = i32::try_from(content.display_offset).unwrap_or(0);

            for indexed in content.display_iter {
                let vis_row = indexed.point.line.0 + offset_i32;
                let vis_col = indexed.point.column.0;
                if vis_row < 0 {
                    continue;
                }
                let Ok(r_idx) = usize::try_from(vis_row) else {
                    continue;
                };
                if r_idx >= rows || vis_col >= cols {
                    continue;
                }

                let mut fg_rgba = resolve_color(indexed.cell.fg, content.colors, true);
                let mut bg_rgba = resolve_color(indexed.cell.bg, content.colors, false);
                if indexed.cell.flags.contains(CellFlags::INVERSE) {
                    std::mem::swap(&mut fg_rgba, &mut bg_rgba);
                }

                let ch = if indexed.cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    ' '
                } else {
                    indexed.cell.c
                };

                cells[r_idx * cols + vis_col] = PtyGridCell {
                    ch,
                    attr: PtyCellAttr {
                        fg_rgba,
                        bg_rgba,
                        bold: indexed.cell.flags.intersects(CellFlags::BOLD),
                        italic: indexed.cell.flags.contains(CellFlags::ITALIC),
                        underline: indexed.cell.flags.intersects(CellFlags::ALL_UNDERLINES),
                    },
                };
            }

            if content.cursor.shape == CursorShape::Hidden {
                None
            } else {
                let c_row = content.cursor.point.line.0 + offset_i32;
                let c_col = content.cursor.point.column.0;
                match (u16::try_from(c_col), u16::try_from(c_row)) {
                    (Ok(cx), Ok(cy)) if usize::from(cx) < cols && usize::from(cy) < rows => {
                        Some((cx, cy))
                    }
                    _ => None,
                }
            }
        };

        let title = self
            .title
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| CompactString::const_new("Terminal"));

        Ok(TerminalDamageSnapshot {
            size,
            cells,
            cursor,
            title,
        })
    }

    async fn close(&self) -> TerminalResult<()> {
        self.terminate_sync();
        Ok(())
    }
}

/// Cross-platform [`TerminalBackend`] spawning OS native shells (`zsh`, `bash`, `fish`, `pwsh`, `cmd`, `wsl`)
/// with inherited SSH/GPG/PATH environment from [`HydratedGitEnv`].
pub struct PortablePtyBackend {
    git_env: Arc<HydratedGitEnv>,
}

impl PortablePtyBackend {
    /// Creates a [`PortablePtyBackend`] propagating hydrated PATH/SSH/GPG settings to spawned shells.
    pub fn new(git_env: Arc<HydratedGitEnv>) -> Self {
        Self { git_env }
    }
}

#[async_trait]
impl TerminalBackend for PortablePtyBackend {
    fn detect_available_shells(&self, default_cwd: &HostPath) -> Vec<ShellProfile> {
        detect_shells_for_os(&self.git_env, default_cwd)
    }

    async fn spawn_session(
        &self,
        profile: ShellProfile,
        initial_size: PtySize,
    ) -> TerminalResult<(Box<dyn TerminalSession>, mpsc::Receiver<()>)> {
        let cols = initial_size.cols.max(2);
        let rows = initial_size.rows.max(2);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PortablePtySize {
                rows,
                cols,
                pixel_width: cols.saturating_mul(initial_size.cell_width_px.max(1)),
                pixel_height: rows.saturating_mul(initial_size.cell_height_px.max(1)),
            })
            .map_err(|e| TerminalError::SpawnFailed {
                shell: profile.program.clone(),
                reason: e.to_string(),
            })?;

        let mut cmd = CommandBuilder::new(&profile.program);
        for arg in &profile.args {
            cmd.arg(arg);
        }
        cmd.cwd(profile.working_dir.as_path());

        let env_map = self
            .git_env
            .build_subprocess_env(SubprocessEnvOptions::default());
        for (k, v) in env_map {
            // Interactive shells allow normal terminal prompting (unlike automated background Git CLI).
            if k == "GIT_TERMINAL_PROMPT" {
                continue;
            }
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Branvio");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::SpawnFailed {
                shell: profile.program.clone(),
                reason: e.to_string(),
            })?;
        // Drop slave side in parent so EOF triggers deterministically when shell closes.
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader =
            pair.master
                .try_clone_reader()
                .map_err(|e| TerminalError::SpawnFailed {
                    shell: profile.program.clone(),
                    reason: e.to_string(),
                })?;
        let raw_writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::SpawnFailed {
                shell: profile.program.clone(),
                reason: e.to_string(),
            })?;

        let (damage_tx, damage_rx) = mpsc::channel(64);
        let title = Arc::new(Mutex::new(profile.label.clone()));
        let writer = Arc::new(Mutex::new(Some(raw_writer)));
        let alive = Arc::new(AtomicBool::new(true));

        let listener = AlacrittyEventProxy {
            title: Arc::clone(&title),
            damage_tx: damage_tx.clone(),
            writer: Arc::clone(&writer),
        };
        let dims = GridDimensions {
            columns: usize::from(cols),
            screen_lines: usize::from(rows),
        };
        let term = Arc::new(Mutex::new(Term::new(
            TermConfig::default(),
            &dims,
            listener,
        )));

        let reader_term = Arc::clone(&term);
        let reader_alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("branvio-pty-reader".to_string())
            .spawn(move || {
                let mut parser: Processor = Processor::new();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut t_guard) = reader_term.lock() {
                                parser.advance(&mut *t_guard, &buf[..n]);
                            }
                            let _ = damage_tx.try_send(());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = child.wait();
                reader_alive.store(false, Ordering::Release);
                let _ = damage_tx.try_send(());
            })
            .map_err(|e| TerminalError::SpawnFailed {
                shell: profile.program.clone(),
                reason: format!("failed to spawn PTY reader thread: {e}"),
            })?;

        let session = AlacrittyPtySession {
            term,
            master: Arc::new(Mutex::new(Some(pair.master))),
            writer,
            killer: Arc::new(Mutex::new(Some(killer))),
            alive,
            title,
            size: Arc::new(Mutex::new(PtySize {
                cols,
                rows,
                ..initial_size
            })),
        };
        Ok((Box::new(session), damage_rx))
    }
}

fn detect_shells_for_os(git_env: &HydratedGitEnv, default_cwd: &HostPath) -> Vec<ShellProfile> {
    let mut profiles = Vec::new();
    let mut seen = HashSet::new();

    #[cfg(windows)]
    {
        let _ = git_env;
        for (label, prog, args) in [
            ("PowerShell 7", "pwsh.exe", vec!["-NoLogo".to_string()]),
            (
                "Windows PowerShell",
                "powershell.exe",
                vec!["-NoLogo".to_string()],
            ),
            ("Command Prompt", "cmd.exe", Vec::new()),
            ("WSL", "wsl.exe", Vec::new()),
        ] {
            if seen.insert(prog.to_string()) {
                profiles.push(ShellProfile {
                    label: CompactString::from(label),
                    program: prog.to_string(),
                    args,
                    working_dir: default_cwd.clone(),
                });
            }
        }
    }

    #[cfg(unix)]
    {
        if let Some(user_shell) = git_env.get("SHELL") {
            let p = PathBuf::from(user_shell);
            if p.is_file() {
                let s = p.to_string_lossy().into_owned();
                let label = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Shell".to_string());
                if seen.insert(s.clone()) {
                    profiles.push(ShellProfile {
                        label: CompactString::from(label),
                        program: s,
                        args: Vec::new(),
                        working_dir: default_cwd.clone(),
                    });
                }
            }
        }

        for candidate in [
            "/bin/zsh",
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/fish",
            "/usr/bin/fish",
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/sh",
        ] {
            let path = PathBuf::from(candidate);
            if path.is_file() && seen.insert(candidate.to_string()) {
                let label = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sh".to_string());
                profiles.push(ShellProfile {
                    label: CompactString::from(label),
                    program: candidate.to_string(),
                    args: Vec::new(),
                    working_dir: default_cwd.clone(),
                });
            }
        }
    }

    if profiles.is_empty() {
        profiles.push(ShellProfile {
            label: CompactString::const_new("sh"),
            program: "/bin/sh".to_string(),
            args: Vec::new(),
            working_dir: default_cwd.clone(),
        });
    }

    profiles
}

fn pack_rgb(rgb: Rgb) -> u32 {
    (u32::from(rgb.r) << 24) | (u32::from(rgb.g) << 16) | (u32::from(rgb.b) << 8) | 0xFF
}

fn resolve_color(
    color: Color,
    palette: &alacritty_terminal::term::color::Colors,
    is_fg: bool,
) -> u32 {
    match color {
        Color::Spec(rgb) => pack_rgb(rgb),
        Color::Named(named) => {
            if let Some(rgb) = palette[named] {
                return pack_rgb(rgb);
            }
            default_named_rgba(named, is_fg)
        }
        Color::Indexed(idx) => {
            if let Some(rgb) = palette[usize::from(idx)] {
                return pack_rgb(rgb);
            }
            default_indexed_rgba(idx)
        }
    }
}

fn default_named_rgba(named: NamedColor, is_fg: bool) -> u32 {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG_RGBA,
        NamedColor::Background => DEFAULT_BG_RGBA,
        NamedColor::Black | NamedColor::DimBlack => 0x1E29_3BFF,
        NamedColor::Red | NamedColor::DimRed => 0xF871_71FF,
        NamedColor::Green | NamedColor::DimGreen => 0x4ADE_80FF,
        NamedColor::Yellow | NamedColor::DimYellow => 0xFACC_15FF,
        NamedColor::Blue | NamedColor::DimBlue => 0x60A5_FAFF,
        NamedColor::Magenta | NamedColor::DimMagenta => 0xC084_FCFF,
        NamedColor::Cyan | NamedColor::DimCyan => 0x22D3_EEFF,
        NamedColor::White | NamedColor::DimWhite => 0xCBD5_E1FF,
        NamedColor::BrightBlack => 0x4755_69FF,
        NamedColor::BrightRed => 0xFCA5_A5FF,
        NamedColor::BrightGreen => 0x86EF_ACFF,
        NamedColor::BrightYellow => 0xFDE0_47FF,
        NamedColor::BrightBlue => 0x93C5_FDFF,
        NamedColor::BrightMagenta => 0xD8B4_FEFF,
        NamedColor::BrightCyan => 0x67E8_F9FF,
        NamedColor::BrightWhite => 0xF8FA_FCFF,
        NamedColor::Cursor => DEFAULT_FG_RGBA,
        #[allow(unreachable_patterns)]
        _ => {
            if is_fg {
                DEFAULT_FG_RGBA
            } else {
                DEFAULT_BG_RGBA
            }
        }
    }
}

fn default_indexed_rgba(idx: u8) -> u32 {
    match idx {
        0 => 0x1E29_3BFF,
        1 => 0xF871_71FF,
        2 => 0x4ADE_80FF,
        3 => 0xFACC_15FF,
        4 => 0x60A5_FAFF,
        5 => 0xC084_FCFF,
        6 => 0x22D3_EEFF,
        7 => 0xCBD5_E1FF,
        8 => 0x4755_69FF,
        9 => 0xFCA5_A5FF,
        10 => 0x86EF_ACFF,
        11 => 0xFDE0_47FF,
        12 => 0x93C5_FDFF,
        13 => 0xD8B4_FEFF,
        14 => 0x67E8_F9FF,
        15 => 0xF8FA_FCFF,
        16..=231 => {
            let n = idx - 16;
            let b_lvl = n % 6;
            let g_lvl = (n / 6) % 6;
            let r_lvl = n / 36;
            let map = |v: u8| if v == 0 { 0u32 } else { 55 + u32::from(v) * 40 };
            (map(r_lvl) << 24) | (map(g_lvl) << 16) | (map(b_lvl) << 8) | 0xFF
        }
        232..=255 => {
            let gray = 8 + u32::from(idx - 232) * 10;
            (gray << 24) | (gray << 16) | (gray << 8) | 0xFF
        }
    }
}
