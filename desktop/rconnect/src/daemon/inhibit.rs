//! Keeps the computer awake during a session through systemd-logind.

use zbus::zvariant::OwnedFd;

/// Holds logind inhibitor locks until dropped.
///
/// `sleep` blocks suspend, including GNOME's suspend on lid close, which goes through
/// logind. `idle` keeps the session from being considered idle. Each is requested
/// separately because polkit may allow one but not the other.
pub struct SleepInhibitor {
    _locks: Vec<OwnedFd>,
}

impl SleepInhibitor {
    pub async fn acquire() -> zbus::Result<SleepInhibitor> {
        let connection = zbus::Connection::system().await?;
        let mut locks = Vec::new();
        for what in ["sleep", "idle"] {
            match inhibit(&connection, what).await {
                Ok(lock) => locks.push(lock),
                Err(e) => tracing::warn!("cannot block {what} during the session: {e}"),
            }
        }
        Ok(SleepInhibitor { _locks: locks })
    }
}

async fn inhibit(connection: &zbus::Connection, what: &str) -> zbus::Result<OwnedFd> {
    let reply = connection
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(
                what,
                "RConnect",
                "A phone is controlling this computer",
                "block",
            ),
        )
        .await?;
    reply.body().deserialize()
}
