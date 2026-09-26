//! Development stand-in for the phone app: pairs with a host and runs sessions from a
//! terminal, printing speech and simulating playback.
//!
//! ```text
//! rconnect-dev-controller pair '<rconnect://pair?... URI>'
//! rconnect-dev-controller hosts
//! rconnect-dev-controller session [host name]
//! ```
//!
//! State lives in `$RCONNECT_CONTROLLER_DIR` (default
//! `~/.local/share/rconnect-dev-controller`). In a session, type commands on stdin:
//! `k 30` taps a key (evdev code), `k 29 46` presses a chord, `d 30` / `u 30` press and
//! release, `t text` types lowercase letters, digits and spaces, `end` ends the session.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use rconnect_core::{Controller, Host, SessionEnd, SessionOutput};
use rconnect_protocol::messages::{KeyMessage, SpeechCommand, SpeechEvent, Utterance};
use rconnect_protocol::ticket::PairingTicket;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

const DEVICE_NAME: &str = "Dev controller";
const MS_PER_CHAR: u64 = 50;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let controller = Controller::new(load_secret_key(&dir)?, DEVICE_NAME.into()).await?;

    match args.first().map(String::as_str) {
        Some("pair") => {
            let uri = args.get(1).context("usage: pair <URI>")?;
            pair(&controller, &dir, uri).await?;
        }
        Some("hosts") => {
            for host in load_hosts(&dir)? {
                println!("{}: {}", host.name, host.fingerprint().join(" "));
            }
        }
        Some("session") => {
            let hosts = load_hosts(&dir)?;
            let host = match args.get(1) {
                Some(name) => hosts.into_iter().find(|host| &host.name == name),
                None => hosts.into_iter().next(),
            }
            .context("no such paired host")?;
            session(&controller, &host).await?;
        }
        _ => anyhow::bail!("usage: rconnect-dev-controller pair <URI> | hosts | session [host]"),
    }
    controller.shutdown().await;
    // The stdin reader blocks a runtime thread; exiting avoids waiting for it.
    std::process::exit(0);
}

async fn pair(controller: &Controller, dir: &std::path::Path, uri: &str) -> anyhow::Result<()> {
    let ticket = PairingTicket::from_uri(uri)?;
    println!(
        "Verification words: {}",
        controller.pairing_words(&ticket.host_id).join(", ")
    );
    println!("Confirm on the computer...");
    let host = controller.pair(&ticket).await?;
    println!("Paired with {}.", host.name);
    let mut hosts = load_hosts(dir)?;
    hosts.retain(|existing| existing.id != host.id);
    hosts.push(host);
    std::fs::write(dir.join("hosts.json"), serde_json::to_vec_pretty(&hosts)?)?;
    Ok(())
}

async fn session(controller: &Controller, host: &Host) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut session = controller.start_session(host).await?;
    println!(
        "Session with {} started in {:?}.",
        session.host_name,
        started.elapsed()
    );
    let mut outputs = session.take_outputs().expect("first call");

    let (line_tx, mut lines) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let mut playback: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            output = outputs.recv() => match output {
                Some(SessionOutput::Speech(command)) => match command {
                    SpeechCommand::Speak(utterance) => {
                        println!("> {}", utterance.text);
                        if let Some(previous) = playback.take() {
                            previous.abort();
                        }
                        playback = Some(tokio::spawn(play(utterance, event_tx.clone())));
                    }
                    SpeechCommand::Stop { .. } | SpeechCommand::Pause { .. } => {
                        if let Some(previous) = playback.take() {
                            previous.abort();
                        }
                    }
                    SpeechCommand::Settings(settings) => println!("(settings: {settings:?})"),
                },
                Some(SessionOutput::Ended(end)) => {
                    println!("Session ended: {end:?}");
                    return Ok(());
                }
                None => {
                    println!("Session ended: {:?}", SessionEnd::ConnectionLost);
                    return Ok(());
                }
            },
            Some(event) = events.recv() => session.send_speech_event(event).await?,
            line = lines.recv() => {
                let Some(line) = line else { break };
                match parse_command(&line) {
                    Some(Command::Keys(messages)) => {
                        for message in messages {
                            session.send_key(message).await?;
                        }
                    }
                    Some(Command::End) => break,
                    None => println!("? commands: k <codes>, d <code>, u <code>, t <text>, end"),
                }
            }
        }
    }
    session.end().await;
    println!("Session ended.");
    Ok(())
}

async fn play(utterance: Utterance, events: mpsc::UnboundedSender<SpeechEvent>) {
    let start = Instant::now();
    let per_char = Duration::from_millis(MS_PER_CHAR);
    for mark in &utterance.marks {
        tokio::time::sleep_until(start + per_char * mark.offset).await;
        let _ = events.send(SpeechEvent::Mark {
            id: utterance.id,
            index: mark.index,
        });
    }
    tokio::time::sleep_until(start + per_char * utterance.text.chars().count() as u32).await;
    let _ = events.send(SpeechEvent::End { id: utterance.id });
}

enum Command {
    Keys(Vec<KeyMessage>),
    End,
}

fn parse_command(line: &str) -> Option<Command> {
    let (command, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    let codes =
        || -> Option<Vec<u16>> { rest.split_whitespace().map(|c| c.parse().ok()).collect() };
    Some(match command {
        "end" => Command::End,
        "d" => Command::Keys(codes()?.into_iter().map(KeyMessage::Down).collect()),
        "u" => Command::Keys(codes()?.into_iter().map(KeyMessage::Up).collect()),
        "k" => {
            let codes = codes()?;
            let mut messages: Vec<KeyMessage> =
                codes.iter().copied().map(KeyMessage::Down).collect();
            messages.extend(codes.iter().rev().copied().map(KeyMessage::Up));
            Command::Keys(messages)
        }
        "t" => {
            let mut messages = Vec::new();
            for c in rest.chars() {
                let code = key_code(c)?;
                messages.extend([KeyMessage::Down(code), KeyMessage::Up(code)]);
            }
            Command::Keys(messages)
        }
        _ => return None,
    })
}

/// evdev key code of a character on a US layout.
fn key_code(c: char) -> Option<u16> {
    const ROWS: [(&str, u16); 4] = [
        ("1234567890", 2),
        ("qwertyuiop", 16),
        ("asdfghjkl", 30),
        ("zxcvbnm", 44),
    ];
    if c == ' ' {
        return Some(57);
    }
    ROWS.iter()
        .find_map(|(row, first)| row.find(c).map(|i| first + i as u16))
}

fn data_dir() -> PathBuf {
    std::env::var_os("RCONNECT_CONTROLLER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/share/rconnect-dev-controller")
        })
}

fn load_secret_key(dir: &std::path::Path) -> anyhow::Result<[u8; 32]> {
    let path = dir.join("secret.key");
    match std::fs::read(&path) {
        Ok(bytes) => bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("corrupted {}", path.display())),
        Err(_) => {
            let key = Controller::generate_secret_key();
            std::fs::write(&path, key)?;
            Ok(key)
        }
    }
}

fn load_hosts(dir: &std::path::Path) -> anyhow::Result<Vec<Host>> {
    match std::fs::read(dir.join("hosts.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(_) => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_codes() {
        assert_eq!(key_code('1'), Some(2));
        assert_eq!(key_code('0'), Some(11));
        assert_eq!(key_code('q'), Some(16));
        assert_eq!(key_code('a'), Some(30));
        assert_eq!(key_code('m'), Some(50));
        assert_eq!(key_code(' '), Some(57));
        assert_eq!(key_code('A'), None);
    }
}
