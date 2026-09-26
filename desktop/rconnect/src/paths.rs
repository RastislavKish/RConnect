//! Filesystem locations.

use std::path::PathBuf;

/// Per-user runtime directory for sockets, e.g. `/run/user/1000/rconnect`.
/// `RCONNECT_RUNTIME_DIR` overrides it, which lets tests run without touching a real
/// daemon.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RCONNECT_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("rconnect"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
            std::env::temp_dir().join(format!("rconnect-{user}"))
        }
    }
}

/// Socket on which the daemon accepts the speech module's connection.
pub fn speech_socket() -> PathBuf {
    runtime_dir().join("speech.sock")
}

/// Socket on which the daemon accepts commands from the `rconnect` CLI.
pub fn control_socket() -> PathBuf {
    runtime_dir().join("control.sock")
}

/// Persistent data: the identity key and the paired devices. `RCONNECT_DATA_DIR`
/// overrides it.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RCONNECT_DATA_DIR") {
        return PathBuf::from(dir);
    }
    xdg_dir("XDG_DATA_HOME", ".local/share").join("rconnect")
}

/// User configuration. `RCONNECT_CONFIG_DIR` overrides it.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RCONNECT_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    xdg_dir("XDG_CONFIG_HOME", ".config").join("rconnect")
}

fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))
        .unwrap_or_else(|| PathBuf::from(fallback))
}

/// Creates a directory accessible only by the current user.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Creates the runtime directory, accessible only by the current user.
pub fn create_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    create_private_dir(&dir)?;
    Ok(dir)
}
