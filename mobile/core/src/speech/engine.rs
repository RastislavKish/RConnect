//! The speech engine: plays the host's speech commands with eSpeak NG.
//!
//! One thread owns eSpeak NG and the audio stream. [`SpeechEngine::speak`] and
//! [`SpeechEngine::stop`] act immediately from the caller's thread: they bump a generation
//! counter and clear the playback buffer, so audio stops within one audio callback and a
//! synthesis still running notices at its next chunk and aborts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use rconnect_protocol::messages::{
    CapitalLetters, Punctuation, SpeechEvent, SpeechKind, SpeechSettings, Utterance,
};

use super::audio::Output;
use super::espeak::{Espeak, Parameters, SynthSink};
use super::playback::Playback;

/// Length of the chunks eSpeak NG synthesizes at a time. Shorter chunks start playback
/// sooner and let a stop abort synthesis sooner.
const CHUNK_MS: u32 = 20;

enum Command {
    Speak {
        generation: u64,
        utterance: Utterance,
    },
    Settings(SpeechSettings),
    Voice(VoiceSettings),
    Shutdown,
}

/// The phone's own speech settings. They decide the voice, rate, pitch and volume; the
/// computer's settings still decide punctuation and capital letters, which follow the
/// screen reader's behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSettings {
    /// Language voice, e.g. `en-us` (see [`super::voices::list_voices`]).
    pub voice: String,
    /// Variant, e.g. `klatt` (see [`super::voices::list_variants`]).
    pub variant: Option<String>,
    /// 0..=100, mapped to 80..=450 words per minute like NVDA does.
    pub rate: u8,
    /// Triples the rate, like NVDA's rate boost; speeds above 450 use libsonic.
    pub rate_boost: bool,
    /// 0..=100, 50 is eSpeak NG's normal pitch.
    pub pitch: u8,
    /// 0..=200, 100 is eSpeak NG's normal volume.
    pub volume: u8,
}

impl Default for VoiceSettings {
    fn default() -> VoiceSettings {
        VoiceSettings {
            voice: "en-us".into(),
            variant: None,
            // NVDA's default for eSpeak NG, 265 words per minute.
            rate: 50,
            rate_boost: false,
            pitch: 50,
            volume: 100,
        }
    }
}

impl VoiceSettings {
    pub fn words_per_minute(&self) -> u32 {
        const MINIMUM: u32 = 80;
        const MAXIMUM: u32 = 450;
        let wpm = MINIMUM + self.rate.min(100) as u32 * (MAXIMUM - MINIMUM) / 100;
        if self.rate_boost { wpm * 3 } else { wpm }
    }

    /// The name eSpeak NG selects the voice by, e.g. `sk+klatt`.
    fn espeak_voice(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}+{variant}", self.voice),
            None => self.voice.clone(),
        }
    }
}

pub struct SpeechEngine {
    commands: Sender<Command>,
    generation: Arc<AtomicU64>,
    playback: Arc<Mutex<Playback>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpeechEngine {
    /// Starts the engine. `data_dir` contains `espeak-ng-data`. Playback events arrive on
    /// the returned receiver as the audio is played.
    pub fn start(
        data_dir: PathBuf,
        voice: VoiceSettings,
        keep_alive: bool,
    ) -> anyhow::Result<(SpeechEngine, UnboundedReceiver<SpeechEvent>)> {
        let (events_tx, events_rx) = unbounded_channel();
        let mut playback = Playback::new(events_tx);
        playback.keep_alive = keep_alive;
        let playback = Arc::new(Mutex::new(playback));
        let generation = Arc::new(AtomicU64::new(0));
        let (commands_tx, commands_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let thread = {
            let playback = playback.clone();
            let generation = generation.clone();
            std::thread::Builder::new()
                .name("speech".into())
                .spawn(move || {
                    let worker = match Worker::new(&data_dir, voice, playback, generation) {
                        Ok(worker) => {
                            let _ = ready_tx.send(Ok(()));
                            worker
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    worker.run(commands_rx);
                })?
        };
        ready_rx.recv()??;

        Ok((
            SpeechEngine {
                commands: commands_tx,
                generation,
                playback,
                thread: Some(thread),
            },
            events_rx,
        ))
    }

    /// Speaks an utterance, cancelling anything still playing.
    pub fn speak(&self, utterance: Utterance) {
        let generation = self.cancel();
        let _ = self.commands.send(Command::Speak {
            generation,
            utterance,
        });
    }

    /// Stops speaking immediately.
    pub fn stop(&self) {
        self.cancel();
    }

    /// Stops speaking and returns the last mark of `id` that was played.
    pub fn pause(&self, id: u32) -> Option<u16> {
        self.cancel();
        self.playback.lock().unwrap().last_mark(id)
    }

    /// The computer's speech settings.
    pub fn set_settings(&self, settings: SpeechSettings) {
        let _ = self.commands.send(Command::Settings(settings));
    }

    /// The phone's speech settings.
    pub fn set_voice(&self, voice: VoiceSettings) {
        let _ = self.commands.send(Command::Voice(voice));
    }

    pub fn set_keep_alive(&self, keep_alive: bool) {
        self.playback.lock().unwrap().keep_alive = keep_alive;
    }

    fn cancel(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.playback.lock().unwrap().clear();
        generation
    }
}

impl Drop for SpeechEngine {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    espeak: Espeak,
    output: Output,
    playback: Arc<Mutex<Playback>>,
    generation: Arc<AtomicU64>,
    voice: VoiceSettings,
    host: SpeechSettings,
}

impl Worker {
    fn new(
        data_dir: &std::path::Path,
        voice: VoiceSettings,
        playback: Arc<Mutex<Playback>>,
        generation: Arc<AtomicU64>,
    ) -> anyhow::Result<Worker> {
        let espeak = Espeak::new(data_dir, CHUNK_MS)?;
        let output = Output::open(espeak.sample_rate(), playback.clone())?;
        let mut worker = Worker {
            espeak,
            output,
            playback,
            generation,
            voice: VoiceSettings::default(),
            host: SpeechSettings::default(),
        };
        worker.set_voice(voice);
        Ok(worker)
    }

    fn run(mut self, commands: Receiver<Command>) {
        loop {
            let command = match commands.recv_timeout(Duration::from_secs(1)) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.check_output();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            match command {
                Command::Speak {
                    generation,
                    utterance,
                } => self.speak(generation, utterance),
                Command::Settings(settings) => {
                    self.host = settings;
                    self.apply_parameters();
                }
                Command::Voice(voice) => self.set_voice(voice),
                Command::Shutdown => return,
            }
        }
    }

    /// Reopens the audio stream after it broke, e.g. when the output device changed.
    fn check_output(&mut self) {
        if self.output.is_broken() {
            match Output::open(self.espeak.sample_rate(), self.playback.clone()) {
                Ok(output) => {
                    tracing::info!("audio output reopened");
                    self.output = output;
                }
                Err(e) => tracing::warn!("cannot reopen audio output: {e:#}"),
            }
        }
    }

    fn set_voice(&mut self, voice: VoiceSettings) {
        let name = voice.espeak_voice();
        if let Err(e) = self.espeak.set_voice(&name) {
            tracing::warn!("cannot select voice {name:?}: {e}");
            // Keep the language when only the variant is missing.
            let _ = self.espeak.set_voice(&voice.voice);
        }
        self.voice = voice;
        // Selecting a voice resets the parameters.
        self.apply_parameters();
    }

    fn apply_parameters(&mut self) {
        if let Err(e) = self
            .espeak
            .set_parameters(&parameters(&self.voice, &self.host))
        {
            tracing::warn!("cannot apply speech settings: {e}");
        }
    }

    fn speak(&mut self, generation: u64, utterance: Utterance) {
        self.check_output();
        if self.generation.load(Ordering::SeqCst) != generation {
            return; // Cancelled before it started.
        }
        let start = self.playback.lock().unwrap().end_position();
        let mut sink = Sink {
            playback: &self.playback,
            generation: &self.generation,
            expected: generation,
            id: utterance.id,
            start,
            sample_rate: self.espeak.sample_rate() as u64,
        };

        let result = match utterance.kind {
            SpeechKind::Text => self.espeak.speak_ssml(&to_ssml(&utterance), &mut sink),
            SpeechKind::Char => match single_char(&utterance.text) {
                Some(c) => self.espeak.speak_character(c, &mut sink),
                None => self.espeak.speak_ssml(&escape(&utterance.text), &mut sink),
            },
            SpeechKind::Key => self.espeak.speak_key(&utterance.text, &mut sink),
            // Sound icons have no sound on the controller yet.
            SpeechKind::SoundIcon => Ok(true),
        };
        match result {
            Ok(true) => {
                let mut playback = self.playback.lock().unwrap();
                if self.generation.load(Ordering::SeqCst) == generation {
                    let end = playback.end_position();
                    playback.push_cue(end, SpeechEvent::End { id: utterance.id });
                }
            }
            Ok(false) => {} // Aborted by a newer command.
            Err(e) => {
                tracing::warn!("synthesis failed: {e}");
                // Report the end anyway, so the host is not left waiting.
                let mut playback = self.playback.lock().unwrap();
                let end = playback.end_position();
                playback.push_cue(end, SpeechEvent::End { id: utterance.id });
            }
        }
    }
}

/// Feeds one utterance's synthesis into the playback buffer.
struct Sink<'a> {
    playback: &'a Mutex<Playback>,
    generation: &'a AtomicU64,
    expected: u64,
    id: u32,
    start: u64,
    sample_rate: u64,
}

impl SynthSink for Sink<'_> {
    fn samples(&mut self, samples: &[i16]) -> bool {
        let mut playback = self.playback.lock().unwrap();
        // Checked under the lock: a cancel clears the buffer while holding it.
        if self.generation.load(Ordering::SeqCst) != self.expected {
            return false;
        }
        playback.push_samples(samples);
        true
    }

    fn mark(&mut self, name: &str, position_ms: u32) {
        let Ok(index) = name.parse::<u16>() else {
            return;
        };
        let mut playback = self.playback.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) == self.expected {
            let position = self.start + position_ms as u64 * self.sample_rate / 1000;
            playback.push_cue(position, SpeechEvent::Mark { id: self.id, index });
        }
    }
}

/// Combines the phone's settings with the computer's into eSpeak NG's parameters.
pub fn parameters(voice: &VoiceSettings, host: &SpeechSettings) -> Parameters {
    Parameters {
        rate: voice.words_per_minute() as i32,
        volume: voice.volume.min(200) as i32,
        pitch: voice.pitch.min(100) as i32,
        range: 50,
        punctuation: match host.punctuation {
            Punctuation::None => 0,
            Punctuation::All => 1,
            Punctuation::Some | Punctuation::Most => 2,
        },
        capitals: match host.capital_letters {
            CapitalLetters::None => 0,
            CapitalLetters::Icon => 1,
            CapitalLetters::Spell => 2,
        },
    }
}

/// Rebuilds SSML with numbered marks from an utterance.
pub fn to_ssml(utterance: &Utterance) -> String {
    let mut ssml = String::from("<speak>");
    let mut marks = utterance.marks.iter().peekable();
    for (offset, c) in utterance.text.chars().enumerate() {
        while let Some(mark) = marks.next_if(|mark| mark.offset as usize <= offset) {
            ssml.push_str(&format!("<mark name=\"{}\"/>", mark.index));
        }
        push_escaped(&mut ssml, c);
    }
    for mark in marks {
        ssml.push_str(&format!("<mark name=\"{}\"/>", mark.index));
    }
    ssml.push_str("</speak>");
    ssml
}

fn escape(text: &str) -> String {
    let mut ssml = String::from("<speak>");
    text.chars().for_each(|c| push_escaped(&mut ssml, c));
    ssml.push_str("</speak>");
    ssml
}

fn push_escaped(ssml: &mut String, c: char) {
    match c {
        '<' => ssml.push_str("&lt;"),
        '>' => ssml.push_str("&gt;"),
        '&' => ssml.push_str("&amp;"),
        c => ssml.push(c),
    }
}

fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconnect_protocol::messages::Mark;

    #[test]
    fn ssml_with_marks() {
        let utterance = Utterance {
            id: 1,
            kind: SpeechKind::Text,
            text: "a<b & č".into(),
            marks: vec![
                Mark {
                    offset: 0,
                    index: 0,
                },
                Mark {
                    offset: 2,
                    index: 1,
                },
                Mark {
                    offset: 7,
                    index: 2,
                },
            ],
        };
        assert_eq!(
            to_ssml(&utterance),
            "<speak><mark name=\"0\"/>a&lt;<mark name=\"1\"/>b &amp; č<mark name=\"2\"/></speak>"
        );
    }

    #[test]
    fn parameter_mapping() {
        let host = SpeechSettings {
            capital_letters: CapitalLetters::Icon,
            ..Default::default()
        };
        let mut voice = VoiceSettings::default();
        assert_eq!(voice.words_per_minute(), 265);
        voice.rate = 0;
        assert_eq!(voice.words_per_minute(), 80);
        voice.rate = 100;
        assert_eq!(voice.words_per_minute(), 450);
        voice.rate_boost = true;
        assert_eq!(voice.words_per_minute(), 1350);
        voice.rate = 50;
        voice.pitch = 70;
        voice.volume = 150;
        let parameters = parameters(&voice, &host);
        assert_eq!(parameters.rate, 795);
        assert_eq!(parameters.pitch, 70);
        assert_eq!(parameters.volume, 150);
        assert_eq!(parameters.capitals, 1);
    }

    #[test]
    fn espeak_voice_names() {
        let mut voice = VoiceSettings {
            voice: "sk".into(),
            ..Default::default()
        };
        assert_eq!(voice.espeak_voice(), "sk");
        voice.variant = Some("klatt".into());
        assert_eq!(voice.espeak_voice(), "sk+klatt");
    }
}
