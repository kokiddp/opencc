//! Platform paths, atomic file writes and the proxy session registry.
//!
//! The registry layout matches the old bash script so a user switching
//! versions keeps working state: `sessions/<pid>.sess` files containing
//! `port|mode`, `proxy.pid` and `proxy.log` in `~/.local/state/opencc/<backend>/`
//! (or `%LOCALAPPDATA%\opencc\<backend>\` on Windows).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const STATE_DIR_NAME: &str = "opencc";

/// Root of the opencc state: `$XDG_STATE_HOME || ~/.local/state` on unix
/// (matches the bash layout), `%LOCALAPPDATA%\opencc` on Windows.
pub fn state_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join(STATE_DIR_NAME);
        }
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("LOCALAPPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join(STATE_DIR_NAME);
            }
        }
    }
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("state").join(STATE_DIR_NAME)
}

/// The user's home directory.
///
/// Checks the `HOME` (Unix) or `USERPROFILE` (Windows) environment variable
/// first so that tests and tools can redirect the home directory by setting
/// the variable in the spawned process's environment.  Falls back to the
/// platform API via `dirs::home_dir()` when the variable is absent or empty.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    if let Ok(p) = std::env::var("HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    #[cfg(windows)]
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::home_dir()
}

/// `~/.codex/auth.json` — the Codex CLI OAuth token (same path the codex CLI
/// itself uses on every platform).
pub fn codex_auth_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("auth.json")
}

/// `~/.codex/models_cache.json` — the Codex CLI model cache.
pub fn codex_models_cache_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("models_cache.json")
}

/// `~/.local/share/opencode/auth.json` — written by `opencode login`.
pub fn opencode_auth_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("opencode")
        .join("auth.json")
}

/// Per-backend state directory: `~/.local/state/opencc/<backend>`.
pub fn backend_dir(backend: &str) -> PathBuf {
    state_root().join(backend)
}

/// Atomically replaces `path` with `content`: write to a temp file in the same
/// directory, then rename over (unix: atomic rename; windows:
/// MoveFileExW REPLACE_EXISTING). On unix the file is created 0600.
pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
    }
    replace_file(&tmp, path)?;
    Ok(())
}

/// Renames `from` over `to`, replacing an existing destination on every
/// platform (std::fs::rename alone fails on Windows when the target exists).
pub(crate) fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
        let from_wide: Vec<u16> = from
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let to_wide: Vec<u16> = to
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let ok = unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

/// Writes a text file atomically (used for the TSV cache and the session
/// registry files).
pub fn write_atomic_text(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    atomic_write(path, content)
}

/// True if a process with this PID exists.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // kill(pid, 0) probes existence without sending a signal.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        unsafe { CloseHandle(handle) };
        true
    }
}

/// Sends a termination request to a PID (unix: `kill`; windows: `taskkill`).
pub fn kill_process(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ))
        }
    }
}

// ── Session registry ───────────────────────────────────────────────────────────

fn sessions_dir(backend: &str) -> PathBuf {
    backend_dir(backend).join("sessions")
}

/// Removes session files whose owning PID is dead. Returns how many were
/// removed.
pub fn sweep_stale_sessions(backend: &str) -> u32 {
    let dir = sessions_dir(backend);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".sess") {
            continue;
        }
        let pid = name.trim_end_matches(".sess");
        if let Ok(pid) = pid.parse::<u32>() {
            if !process_alive(pid) {
                let _ = fs::remove_file(entry.path());
                removed += 1;
            }
        }
    }
    removed
}

/// Number of live sessions registered on the same proxy (port|mode pair).
pub fn sessions_on_proxy(backend: &str, port: u16, mode: &str) -> u32 {
    let dir = sessions_dir(backend);
    let expected = format!("{port}|{mode}");
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().ends_with(".sess") {
            continue;
        }
        if let Ok(content) = fs::read_to_string(entry.path()) {
            if content.trim() == expected {
                n += 1;
            }
        }
    }
    n
}

/// Registers the current process on a proxy (port|mode pair).
pub fn register_session(backend: &str, pid: u32, port: u16, mode: &str) -> std::io::Result<()> {
    let dir = sessions_dir(backend);
    fs::create_dir_all(&dir)?;
    write_atomic_text(
        &dir.join(format!("{pid}.sess")),
        &format!("{port}|{mode}\n"),
    )
}

/// Removes the current process from the registry.
pub fn unregister_session(backend: &str, pid: u32) {
    let _ = fs::remove_file(sessions_dir(backend).join(format!("{pid}.sess")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join(format!("opencc-state-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file.txt");
        atomic_write(&path, "first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        atomic_write(&path, "second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        // No temp leftovers.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_registry_counts_and_sweeps() {
        let dir = backend_dir("test-backend");
        // Hermetic: the state root is the real one — clean leftovers from
        // previous (possibly failed) runs.
        let _ = fs::remove_dir_all(&dir);
        let sess = dir.join("sessions");
        fs::create_dir_all(&sess).unwrap();

        // The registry is one file per invoking pid: register our (live) pid
        // on one proxy, simulate another session with a fake pid file.
        let my_pid = std::process::id();
        register_session("test-backend", my_pid, 3199, "opencode").unwrap();
        fs::write(sess.join("4294967292.sess"), "3200|apikey\n").unwrap();
        assert_eq!(sessions_on_proxy("test-backend", 3199, "opencode"), 1);
        assert_eq!(sessions_on_proxy("test-backend", 3200, "apikey"), 1);
        assert_eq!(sessions_on_proxy("test-backend", 3199, "apikey"), 0);

        // Dead pids are swept; live ones (our own) survive.
        fs::write(sess.join("4294967294.sess"), "3199|opencode\n").unwrap();
        assert_eq!(sweep_stale_sessions("test-backend"), 2);
        assert_eq!(sessions_on_proxy("test-backend", 3199, "opencode"), 1);

        unregister_session("test-backend", my_pid);
        assert_eq!(sessions_on_proxy("test-backend", 3199, "opencode"), 0);
        assert_eq!(sessions_on_proxy("test-backend", 3200, "apikey"), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
