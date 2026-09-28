//! Development stand-in for the phone app: pairs with a host and runs sessions from a
//! terminal, speaking through eSpeak NG like the app does and printing the text.
//!
//! ```text
//! rconnect-dev-controller pair '<rconnect://pair?... URI>'
//! rconnect-dev-controller discover
//! rconnect-dev-controller pair-local <name>
//! rconnect-dev-controller hosts
//! rconnect-dev-controller session [host name]
//! ```
//!
//! State lives in `$RCONNECT_CONTROLLER_DIR` (default
//! `~/.local/share/rconnect-dev-controller`). In a session, type commands on stdin:
//! `k 30` taps a key (evdev code), `k 29 46` presses a chord, `d 30` / `u 30` press and
//! release, `t text` types lowercase letters, digits and spaces, `end` ends the session.

use std::path::PathBuf;

use anyhow::Context;
use rconnect_core::speech::VoiceSettings;
use std::time::Duration;

use rconnect_core::{
    Controller, Discovery, Host, SessionInput, SessionObserver, SpeechConfig, drive_session,
};
use rconnect_protocol::messages::KeyMessage;
use rconnect_protocol::ticket::PairingTicket;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

const DEVICE_NAME: &str = "Dev controller";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rconnect_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let controller = Controller::new(load_secret_key(&dir)?, DEVICE_NAME.into()).await?;

    match args.first().map(String::as_str) {
        Some("pair") => {
            let uri = args.get(1).context("usage: pair <URI>")?;
            pair(&controller, &dir, uri).await?;
        }
        Some("discover") => {
            for (_, name) in discover(&controller, Duration::from_secs(5)).await {
                println!("{name}");
            }
        }
        Some("pair-local") => {
            let name = args.get(1).context("usage: pair-local <name or hex id>")?;
            // A hex id skips discovery, which firewalls blocking multicast can prevent.
            let id = match rconnect_protocol::hex::decode::<32>(name) {
                Some(id) => {
                    println!(
                        "{} answers as {:?}.",
                        name,
                        controller.query_name(&id).await?
                    );
                    id
                }
                None => {
                    let found = discover(&controller, Duration::from_secs(5)).await;
                    found
                        .into_iter()
                        .find(|(_, found)| found == name)
                        .context("no such computer on the local network")?
                        .0
                }
            };
            println!(
                "Verification words: {}",
                controller.pairing_words(&id).join(", ")
            );
            println!("Confirm on the computer...");
            let host = controller.pair_discovered(id).await?;
            save_host(&dir, host)?;
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
    save_host(dir, host)
}

fn save_host(dir: &std::path::Path, host: Host) -> anyhow::Result<()> {
    println!("Paired with {}.", host.name);
    let mut hosts = load_hosts(dir)?;
    hosts.retain(|existing| existing.id != host.id);
    hosts.push(host);
    std::fs::write(dir.join("hosts.json"), serde_json::to_vec_pretty(&hosts)?)?;
    Ok(())
}

/// Computers found on the local network within `duration`.
async fn discover(
    controller: &Controller,
    duration: Duration,
) -> Vec<(rconnect_protocol::DeviceId, String)> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn({
        let controller = controller.clone();
        async move { controller.discover(tx).await }
    });
    let mut found = Vec::new();
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(Discovery::Found { id, name }) => found.push((id, name)),
                Some(Discovery::Lost { id }) => found.retain(|(known, _)| *known != id),
                None => break,
            },
            _ = &mut deadline => break,
        }
    }
    task.abort();
    found
}

async fn session(controller: &Controller, host: &Host) -> anyhow::Result<()> {
    let started = Instant::now();
    let session = controller.start_session(host).await?;
    println!(
        "Session with {} started in {:?}.",
        session.host_name,
        started.elapsed()
    );

    let data_dir = rconnect_core::speech::espeak::dev_data()
        .context("no eSpeak NG data; build natively")?
        .to_path_buf();
    let (input_tx, inputs) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin.next_line().await {
            match parse_command(&line) {
                Some(Command::Keys(keys)) => {
                    for key in keys {
                        let _ = input_tx.send(SessionInput::Key(key));
                    }
                }
                Some(Command::Ping) => {
                    let _ = input_tx.send(SessionInput::Ping);
                }
                Some(Command::End) => break,
                None => println!("? commands: k <codes>, d <code>, u <code>, t <text>, ping, end"),
            }
        }
        let _ = input_tx.send(SessionInput::End);
    });

    let speech = SpeechConfig {
        data_dir,
        voice: VoiceSettings::default(),
        keep_alive: false,
    };
    let end = drive_session(session, inputs, speech, &Printer).await;
    println!("Session ended: {end:?}");
    Ok(())
}

struct Printer;

impl SessionObserver for Printer {
    fn speech(&self, text: &str) {
        println!("> {text}");
    }

    fn ping(&self, round_trip: Option<Duration>) {
        match round_trip {
            Some(round_trip) => println!("Ping: {round_trip:?}"),
            None => println!("Ping: no answer"),
        }
    }
}

enum Command {
    Keys(Vec<KeyMessage>),
    Ping,
    End,
}

fn parse_command(line: &str) -> Option<Command> {
    let (command, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    let codes =
        || -> Option<Vec<u16>> { rest.split_whitespace().map(|c| c.parse().ok()).collect() };
    Some(match command {
        "end" => Command::End,
        "ping" => Command::Ping,
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
