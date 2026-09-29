//! Speech on the controller: eSpeak NG synthesis played through a single, always open
//! audio stream, with the host's audio mixed in.

mod audio;
pub mod engine;
pub mod espeak;
mod playback;
pub mod voices;

pub use engine::{SpeechEngine, VoiceSettings};
pub use rconnect_audio::StreamPlayer;
