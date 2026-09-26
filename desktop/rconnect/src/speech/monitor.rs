//! `rconnect speech-monitor`: a development stand-in for the daemon and the phone.
//!
//! Listens on the speech socket like the daemon, prints every command from the speech
//! module, and simulates playback at a fixed speed so that marks and `End` events flow
//! back as they would from a real controller.

use std::time::Duration;

use rconnect_protocol::framing;
use rconnect_protocol::messages::{SpeechCommand, SpeechEvent, Utterance};
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::ipc::{DaemonMessage, ModuleMessage, OutputMode};
use crate::paths;

pub fn run(mirror: bool, ms_per_char: u64) -> anyhow::Result<()> {
    let mode = if mirror {
        OutputMode::Mirror
    } else {
        OutputMode::Remote
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(mode, Duration::from_millis(ms_per_char)))
}

async fn serve(mode: OutputMode, per_char: Duration) -> anyhow::Result<()> {
    paths::create_runtime_dir()?;
    let socket = paths::speech_socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    println!(
        "Listening on {}, mode {mode:?}. Press Ctrl+C to stop.",
        socket.display()
    );

    loop {
        let (stream, _) = listener.accept().await?;
        println!("Speech module connected.");
        let (reader, writer) = stream.into_split();
        if let Err(e) = session(BufReader::new(reader), writer, mode, per_char).await {
            println!("Connection error: {e}");
        }
        println!("Speech module disconnected.");
    }
}

/// Simulated playback of the current utterance.
struct Playback {
    utterance: Utterance,
    started: Instant,
    task: tokio::task::JoinHandle<()>,
}

impl Playback {
    fn cancel(self) {
        self.task.abort();
    }
}

async fn session(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    mode: OutputMode,
    per_char: Duration,
) -> anyhow::Result<()> {
    framing::write_message(&mut writer, &DaemonMessage::SetMode(mode)).await?;

    // Playback tasks report events here; they are written from this task only.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SpeechEvent>();
    let mut playback: Option<Playback> = None;

    loop {
        tokio::select! {
            message = framing::read_message::<_, ModuleMessage>(&mut reader) => {
                let Some(message) = message? else { return Ok(()) };
                match message {
                    ModuleMessage::Hello { version } => println!("Hello, IPC version {version}"),
                    ModuleMessage::Speech(command) => {
                        handle_command(command, &mut playback, &event_tx, &mut writer, per_char).await?
                    }
                }
            }
            Some(event) = event_rx.recv() => {
                if let SpeechEvent::End { id } = event {
                    println!("  #{id} end");
                    playback.take_if(|p| p.utterance.id == id);
                }
                framing::write_message(&mut writer, &DaemonMessage::Event(event)).await?;
            }
        }
    }
}

async fn handle_command(
    command: SpeechCommand,
    playback: &mut Option<Playback>,
    event_tx: &mpsc::UnboundedSender<SpeechEvent>,
    writer: &mut OwnedWriteHalf,
    per_char: Duration,
) -> anyhow::Result<()> {
    match command {
        SpeechCommand::Settings(settings) => println!("Settings: {settings:?}"),
        SpeechCommand::Speak(utterance) => {
            if let Some(previous) = playback.take() {
                previous.cancel();
            }
            println!(
                "#{} {:?}: {:?} ({} marks)",
                utterance.id,
                utterance.kind,
                utterance.text,
                utterance.marks.len()
            );
            let task = tokio::spawn(play(utterance.clone(), per_char, event_tx.clone()));
            *playback = Some(Playback {
                utterance,
                started: Instant::now(),
                task,
            });
        }
        SpeechCommand::Stop { id } => {
            println!("  #{id} stop");
            if let Some(current) = playback.take_if(|p| p.utterance.id == id) {
                current.cancel();
            }
        }
        SpeechCommand::Pause { id } => {
            println!("  #{id} pause");
            if let Some(current) = playback.take_if(|p| p.utterance.id == id) {
                let played = elapsed_chars(current.started.elapsed(), per_char);
                let last_mark = current
                    .utterance
                    .marks
                    .iter()
                    .take_while(|mark| mark.offset <= played)
                    .last()
                    .map(|mark| mark.index);
                current.cancel();
                let event = SpeechEvent::Paused { id, last_mark };
                framing::write_message(writer, &DaemonMessage::Event(event)).await?;
            }
        }
    }
    Ok(())
}

fn elapsed_chars(elapsed: Duration, per_char: Duration) -> u32 {
    (elapsed.as_micros() / per_char.as_micros().max(1)) as u32
}

async fn play(
    utterance: Utterance,
    per_char: Duration,
    events: mpsc::UnboundedSender<SpeechEvent>,
) {
    let started = Instant::now();
    let id = utterance.id;
    for mark in &utterance.marks {
        tokio::time::sleep_until(started + per_char * mark.offset).await;
        let _ = events.send(SpeechEvent::Mark {
            id,
            index: mark.index,
        });
    }
    let length = utterance.text.chars().count() as u32;
    tokio::time::sleep_until(started + per_char * length).await;
    let _ = events.send(SpeechEvent::End { id });
}
