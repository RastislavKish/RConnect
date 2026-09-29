//! Messages exchanged on each stream.
//!
//! Terminology: the *host* is the Linux desktop being controlled, the *controller* is the
//! phone. Session streams are opened in a fixed order right after the connection is
//! established (see `docs/ARCHITECTURE.md`, section 5.3):
//!
//! 1. Control: bidirectional, opened by the controller. [`ControlMessage`] both ways.
//! 2. Keys: unidirectional, controller → host. [`KeyMessage`].
//! 3. Speech: bidirectional, opened by the host. [`SpeechCommand`] host → controller,
//!    [`SpeechEvent`] controller → host.
//! 4. Braille: reserved, opened only when both sides announce [`Capabilities::BRAILLE`].
//!
//! Audio travels in QUIC datagrams rather than on a stream; see [`crate::audio`].
//!
//! Pairing uses a single bidirectional stream opened by the controller on the pairing
//! ALPN, carrying [`PairRequest`] and [`PairResponse`].
//!
//! New variants must only be appended at the end of an enum: postcard encodes variants
//! by index.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------
// Control stream
// ---------------------------------------------------------------------------------------

/// Optional features a peer supports, exchanged in the handshake.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const NONE: Capabilities = Capabilities(0);
    /// Braille display support.
    pub const BRAILLE: Capabilities = Capabilities(1 << 0);
    /// Audio stream from the host to the controller.
    pub const AUDIO_TO_CONTROLLER: Capabilities = Capabilities(1 << 1);
    /// Audio stream from the controller to the host.
    pub const AUDIO_TO_HOST: Capabilities = Capabilities(1 << 2);

    pub fn contains(self, other: Capabilities) -> bool {
        self.0 & other.0 == other.0
    }

    /// Features supported by both peers.
    pub fn intersection(self, other: Capabilities) -> Capabilities {
        Capabilities(self.0 & other.0)
    }
}

impl std::ops::BitOr for Capabilities {
    type Output = Capabilities;

    fn bitor(self, other: Capabilities) -> Capabilities {
        Capabilities(self.0 | other.0)
    }
}

/// Contents of both `Hello` (controller → host) and `Welcome` (host → controller).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handshake {
    pub protocol_version: u16,
    /// Application version, for diagnostics only.
    pub app_version: String,
    /// Human readable device name.
    pub device_name: String,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// First message of a session, sent by the controller.
    Hello(Handshake),
    /// The host's answer when it accepts the session.
    Welcome(Handshake),
    /// The host's answer when it refuses the session. The connection is closed afterwards.
    Reject(RejectReason),
    /// Either side ends the session. The connection is closed afterwards.
    EndSession(EndReason),
    /// The controller asks for an audio stream, or for another mode of the running one.
    AudioRequest(AudioRequest),
    /// The host starts sending or expecting audio datagrams (see [`crate::audio`]). Sent
    /// in answer to every [`ControlMessage::AudioRequest`] it can serve.
    AudioStarted(AudioFormat),
    /// The controller stops the audio stream. From the host: the stream stopped, or the
    /// request cannot be served.
    AudioStop,
    /// The controller measures the round trip time; the host answers with [`Pong`] carrying
    /// the same number.
    ///
    /// [`Pong`]: ControlMessage::Pong
    Ping(u32),
    /// The host's answer to [`ControlMessage::Ping`].
    Pong(u32),
    /// The controller sets its microphone's mode (see `docs/ARCHITECTURE.md`, §5.7). In
    /// Unmuted and VAD it sends the microphone as audio datagrams.
    MicRequest(MicMode),
    /// The host's answer to [`ControlMessage::MicRequest`]: the mode now in effect.
    MicState(MicMode),
    /// The host's answer when it cannot create the virtual microphone.
    MicUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The peer's protocol version is not supported; carries the host's version.
    UnsupportedVersion(u16),
    /// The controller is not on the host's allowlist.
    NotPaired,
    /// Another controller has an active session.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndReason {
    /// The user ended the session.
    UserRequest,
    /// The host or the controller app is shutting down.
    Shutdown,
    /// A new session from the same controller replaced this one.
    Replaced,
    /// The controller's pairing was revoked on the host.
    Revoked,
    /// A protocol or internal error.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDirection {
    ToController,
    ToHost,
    Duplex,
}

/// The controller's microphone, as the host's virtual microphone sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MicMode {
    /// No virtual microphone on the host.
    Off,
    /// The virtual microphone exists and plays silence.
    Muted,
    /// The controller sends its microphone continuously.
    Unmuted,
    /// The controller sends its microphone while someone speaks (voice activity
    /// detection); the host plays silence in between.
    Vad,
}

/// When the host sends audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioMode {
    /// Only while something audible plays.
    Auto,
    /// Continuously, silence included.
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioRequest {
    pub direction: AudioDirection,
    pub mode: AudioMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub direction: AudioDirection,
    pub mode: AudioMode,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    /// Duration of one frame (one datagram) in milliseconds.
    pub frame_ms: u8,
}

// ---------------------------------------------------------------------------------------
// Keys stream
// ---------------------------------------------------------------------------------------

/// Keyboard input, controller → host. Key codes are Linux evdev codes (`KEY_*`).
///
/// Only state changes are sent; key repeat is produced by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyMessage {
    Down(u16),
    Up(u16),
    /// Release every key the host considers pressed.
    ReleaseAll,
}

// ---------------------------------------------------------------------------------------
// Speech stream
// ---------------------------------------------------------------------------------------

/// Speech commands, host → controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeechCommand {
    /// Full snapshot of the speech settings. Sent at session start and on every change.
    Settings(SpeechSettings),
    /// Speak an utterance. Cancels anything still playing.
    Speak(Utterance),
    /// Stop the given utterance immediately. The host does not wait for confirmation.
    Stop { id: u32 },
    /// Pause the given utterance at the next mark and report [`SpeechEvent::Paused`].
    Pause { id: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utterance {
    /// Identifies the utterance in [`SpeechEvent`]s. Increases with each utterance.
    pub id: u32,
    pub kind: SpeechKind,
    /// Plain text, without markup.
    pub text: String,
    /// Index marks, ordered by offset. The controller reports each one as it is played.
    pub marks: Vec<Mark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeechKind {
    /// Ordinary text.
    Text,
    /// A single character, to be spoken the way the synthesizer speaks characters.
    Char,
    /// A key name, as defined by SSIP's `KEY` command.
    Key,
    /// A sound icon name, as defined by SSIP's `SOUND_ICON` command.
    SoundIcon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Position in [`Utterance::text`], counted in Unicode scalar values.
    pub offset: u32,
    /// Mark number within the utterance. The host maps it back to the original name.
    pub index: u16,
}

/// Speech parameters, mirroring speech-dispatcher's output module `SET` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechSettings {
    /// -100..=100, 0 is the synthesizer's default.
    pub rate: i8,
    /// -100..=100, 0 is the synthesizer's default.
    pub pitch: i8,
    /// -100..=100, 0 is the synthesizer's default.
    pub pitch_range: i8,
    /// -100..=100, 0 is the synthesizer's default.
    pub volume: i8,
    pub punctuation: Punctuation,
    pub spelling: bool,
    pub capital_letters: CapitalLetters,
    /// Generic voice type, e.g. `MALE1`.
    pub voice_type: Option<String>,
    /// Synthesizer specific voice name.
    pub synthesis_voice: Option<String>,
    /// Language code, e.g. `en-US`.
    pub language: Option<String>,
}

impl Default for SpeechSettings {
    fn default() -> Self {
        SpeechSettings {
            rate: 0,
            pitch: 0,
            pitch_range: 0,
            volume: 0,
            punctuation: Punctuation::Some,
            spelling: false,
            capital_letters: CapitalLetters::None,
            voice_type: None,
            synthesis_voice: None,
            language: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Punctuation {
    None,
    Some,
    Most,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapitalLetters {
    None,
    Spell,
    Icon,
}

/// Playback progress, controller → host. Events for an id other than the host's current
/// utterance are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeechEvent {
    /// A mark was reached in the audio output.
    Mark { id: u32, index: u16 },
    /// The utterance finished playing.
    End { id: u32 },
    /// The utterance was paused after a [`SpeechCommand::Pause`].
    Paused { id: u32, last_mark: Option<u16> },
}

// ---------------------------------------------------------------------------------------
// Braille stream (reserved, not implemented yet)
// ---------------------------------------------------------------------------------------

/// Braille output, host → controller. Unstable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrailleOutput {
    /// Dot patterns (bit 0 = dot 1 … bit 7 = dot 8) and the cursor cell, if any.
    Cells { cells: Vec<u8>, cursor: Option<u16> },
}

/// Braille input, controller → host. Unstable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrailleInput {
    /// Braille keyboard chord (bit 0 = dot 1 … bit 7 = dot 8, bit 8 = space).
    Chord(u32),
    /// Cursor routing button above the given cell was pressed.
    Routing(u16),
}

// ---------------------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------------------

/// Controller → host on the pairing ALPN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairRequest {
    /// Ask for the host's name, e.g. to label a host found through local discovery.
    Query,
    /// Ask to be paired. Waits for the user's decision on the host.
    Pair {
        protocol_version: u16,
        /// Secret from the pairing ticket (QR code). Absent when the host was found
        /// through local discovery.
        secret: Option<[u8; crate::ticket::SECRET_LEN]>,
        device_name: String,
    },
}

/// Host → controller on the pairing ALPN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairResponse {
    /// Answer to [`PairRequest::Query`].
    Info {
        protocol_version: u16,
        device_name: String,
    },
    /// The user confirmed the pairing.
    Accepted {
        device_name: String,
    },
    Rejected(PairRejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairRejectReason {
    /// The user declined, or did not answer in time.
    Declined,
    /// The secret does not match the open pairing window.
    InvalidSecret,
    /// No pairing window is open on the host.
    NotPairing,
    /// Carries the host's protocol version.
    UnsupportedVersion(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities() {
        let ours = Capabilities::BRAILLE | Capabilities::AUDIO_TO_CONTROLLER;
        let theirs = Capabilities::AUDIO_TO_CONTROLLER | Capabilities::AUDIO_TO_HOST;
        let common = ours.intersection(theirs);
        assert!(common.contains(Capabilities::AUDIO_TO_CONTROLLER));
        assert!(!common.contains(Capabilities::BRAILLE));
        assert!(common.contains(Capabilities::NONE));
    }

    /// Guards the wire format: postcard encodes enum variants by index, so reordering
    /// variants silently breaks compatibility with older peers.
    #[test]
    fn variant_indexes_are_stable() {
        let tag = |bytes: Vec<u8>| bytes[0];
        assert_eq!(tag(postcard::to_allocvec(&KeyMessage::Down(1)).unwrap()), 0);
        assert_eq!(tag(postcard::to_allocvec(&KeyMessage::Up(1)).unwrap()), 1);
        assert_eq!(
            tag(postcard::to_allocvec(&KeyMessage::ReleaseAll).unwrap()),
            2
        );
        assert_eq!(
            tag(postcard::to_allocvec(&SpeechCommand::Stop { id: 1 }).unwrap()),
            2
        );
        assert_eq!(
            tag(postcard::to_allocvec(&SpeechEvent::End { id: 1 }).unwrap()),
            1
        );
        assert_eq!(
            tag(postcard::to_allocvec(&ControlMessage::AudioStop).unwrap()),
            6
        );
        assert_eq!(
            tag(postcard::to_allocvec(&ControlMessage::Pong(1)).unwrap()),
            8
        );
        assert_eq!(
            tag(postcard::to_allocvec(&ControlMessage::MicUnavailable).unwrap()),
            11
        );
        assert_eq!(tag(postcard::to_allocvec(&PairRequest::Query).unwrap()), 0);
    }
}
