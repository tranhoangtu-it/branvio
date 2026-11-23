#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use branvio_core::paths::HostPath;
use branvio_core::platform::HydratedGitEnv;
#[cfg(unix)]
use compact_str::CompactString;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
#[cfg(unix)]
use terminal_pty::ShellProfile;
use terminal_pty::{PortablePtyBackend, PtySize, TerminalBackend};

#[tokio::test]
async fn spawns_real_pty_executes_command_renders_ansi_colors_resizes_and_closes() {
    let cwd = TempDir::new().unwrap();
    let cwd_hp = HostPath::canonicalize_existing(cwd.path()).unwrap();
    let env = Arc::new(HydratedGitEnv::from_current_process());
    let backend = PortablePtyBackend::new(env);

    let shells = backend.detect_available_shells(&cwd_hp);
    assert!(!shells.is_empty(), "at least one OS shell must be detected");

    #[cfg(unix)]
    let test_profile = ShellProfile {
        label: CompactString::const_new("sh-test"),
        program: "/bin/sh".to_string(),
        args: Vec::new(),
        working_dir: cwd_hp,
    };
    #[cfg(windows)]
    let test_profile = shells[0].clone();

    let (session, mut damage_rx) = backend
        .spawn_session(
            test_profile,
            PtySize {
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
            },
        )
        .await
        .unwrap();

    // Send a marker command printing colored ANSI text (split string prevents matching typed command echo)
    #[cfg(unix)]
    let cmd = "printf '\\033[31mBRANVIO_ANSI_%s\\033[0m\\n' 'OK'\n";
    // Windows: the default profile is PowerShell (`pwsh.exe`/`powershell.exe`), which has no
    // `printf`; ConPTY submits a line on CR (the Enter key), not on a bare LF.
    #[cfg(windows)]
    let cmd = "Write-Output (\"$([char]27)[31mBRANVIO_ANSI_{0}$([char]27)[0m\" -f 'OK')\r";
    session.write_bytes(cmd.as_bytes()).await.unwrap();

    let mut found_text = false;
    let mut found_red_fg = false;
    let needle = "BRANVIO_ANSI_OK";
    // Generous: a cold PowerShell start on CI runners can take several seconds.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(250), damage_rx.recv()).await;
        let snap = session.snapshot_viewport().unwrap();
        assert_eq!(snap.cells.len(), 80 * 24);
        let text: String = snap.cells.iter().map(|c| c.ch).collect();
        for (byte_idx, _) in text.match_indices(needle) {
            let char_idx = text[..byte_idx].chars().count();
            found_text = true;
            if snap
                .cells
                .get(char_idx)
                .is_some_and(|c| c.attr.fg_rgba == 0xF871_71FF)
            {
                found_red_fg = true;
                break;
            }
        }
        if found_red_fg {
            break;
        }
    }
    assert!(
        found_text,
        "shell output should appear inside alacritty viewport grid"
    );
    assert!(
        found_red_fg,
        "ANSI red color escape must populate cell fg_rgba"
    );

    // Test dynamic resize
    session
        .resize(PtySize {
            cols: 100,
            rows: 30,
            cell_width_px: 8,
            cell_height_px: 16,
        })
        .await
        .unwrap();
    let resized = session.snapshot_viewport().unwrap();
    assert_eq!(resized.size.cols, 100);
    assert_eq!(resized.size.rows, 30);
    assert_eq!(resized.cells.len(), 100 * 30);

    // Close cleanly terminates child
    session.close().await.unwrap();
}
