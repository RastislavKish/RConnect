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

/// Creates the runtime directory, accessible only by the current user.
pub fn create_runtime_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let dir = runtime_dir();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}
