//! Audio shared by the host and the controller: [`StreamPlayer`] plays the Opus datagrams
//! of either direction, the host's audio on the phone (`docs/ARCHITECTURE.md`, §5.6) and
//! the phone's microphone on the host (§5.7).

mod player;

pub use player::StreamPlayer;
