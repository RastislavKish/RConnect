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
    Shutdown,
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
                    let worker = match Worker::new(&data_dir, playback, generation) {
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

    pub fn set_settings(&self, settings: SpeechSettings) {
        let _ = self.commands.send(Command::Settings(settings));
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
    voice: String,
}

impl Worker {
    fn new(
        data_dir: &std::path::Path,
        playback: Arc<Mutex<Playback>>,
        generation: Arc<AtomicU64>,
    ) -> anyhow::Result<Worker> {
        let espeak = Espeak::new(data_dir, CHUNK_MS)?;
        espeak.set_parameters(&parameters(&SpeechSettings::default()))?;
        let output = Output::open(espeak.sample_rate(), playback.clone())?;
        Ok(Worker {
            espeak,
            output,
            playback,
            generation,
            voice: "en".into(),
        })
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
                Command::Settings(settings) => self.apply_settings(&settings),
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

    fn apply_settings(&mut self, settings: &SpeechSettings) {
        let voice = settings
            .synthesis_voice
            .clone()
            .or_else(|| settings.language.clone())
            .map(|voice| voice.to_ascii_lowercase())
            .unwrap_or_else(|| "en".into());
        if voice != self.voice {
            match self.espeak.set_voice(&voice) {
                Ok(()) => self.voice = voice,
                Err(e) => tracing::warn!("cannot select voice {voice:?}: {e}"),
            }
        }
        // Voice changes reset parameters, so they are always applied afterwards.
        if let Err(e) = self.espeak.set_parameters(&parameters(settings)) {
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

/// Maps speech-dispatcher's settings to eSpeak NG's parameters, the way speech-dispatcher's
/// own espeak-ng module does it in spirit: 0 is eSpeak NG's default, ±100 its limits.
pub fn parameters(settings: &SpeechSettings) -> Parameters {
    const NORMAL: i32 = 175;
    const MINIMUM: i32 = 80;
    const MAXIMUM: i32 = 450;
    let rate = settings.rate as i32;
    Parameters {
        rate: if rate < 0 {
            NORMAL + rate * (NORMAL - MINIMUM) / 100
        } else {
            NORMAL + rate * (MAXIMUM - NORMAL) / 100
        },
        volume: 100 + settings.volume as i32,
        pitch: 50 + settings.pitch as i32 / 2,
        range: 50 + settings.pitch_range as i32 / 2,
        punctuation: match settings.punctuation {
            Punctuation::None => 0,
            Punctuation::All => 1,
            Punctuation::Some | Punctuation::Most => 2,
        },
        capitals: match settings.capital_letters {
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
        let mut settings = SpeechSettings::default();
        assert_eq!(parameters(&settings).rate, 175);
        settings.rate = 100;
        assert_eq!(parameters(&settings).rate, 450);
        settings.rate = -100;
        assert_eq!(parameters(&settings).rate, 80);
        settings.rate = 50;
        settings.pitch = -100;
        settings.volume = 100;
        let parameters = parameters(&settings);
        assert_eq!(parameters.rate, 312);
        assert_eq!(parameters.pitch, 0);
        assert_eq!(parameters.volume, 200);
    }
}
