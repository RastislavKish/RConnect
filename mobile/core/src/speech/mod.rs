//! Speech on the controller: eSpeak NG synthesis played through a single, always open
//! audio stream.

mod audio;
pub mod engine;
pub mod espeak;
mod playback;

pub use engine::SpeechEngine;
