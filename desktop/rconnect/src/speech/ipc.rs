//! Messages between the speech module and the daemon, over a Unix socket.
//!
//! The module connects to [`crate::paths::speech_socket`] and sends
//! [`ModuleMessage::Hello`]. Both directions use the framing of
//! [`rconnect_protocol::framing`]. Until the daemon sends [`DaemonMessage::SetMode`], the
//! module stays in [`OutputMode::Local`]; when the connection drops it returns to it.

use rconnect_protocol::messages::{SpeechCommand, SpeechEvent};
use serde::{Deserialize, Serialize};

/// Bumped on incompatible changes. Module and daemon come from the same binary, so this
/// only matters while an old module process is still running after an upgrade.
pub const IPC_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputMode {
    /// Speech goes only to the local synthesizer.
    Local,
    /// Speech goes only to the controller; the local synthesizer is silent.
    Remote,
    /// Speech goes to both. The local synthesizer drives speech-dispatcher's events.
    Mirror,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleMessage {
    Hello { version: u16 },
    Speech(SpeechCommand),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonMessage {
    SetMode(OutputMode),
    Event(SpeechEvent),
}
