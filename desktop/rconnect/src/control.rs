//! The `rconnect` CLI's link to the daemon, over [`paths::control_socket`], and the CLI
//! commands built on it.
//!
//! Each CLI invocation opens one connection and sends one [`ControlRequest`]. Most requests
//! get a single [`ControlResponse`]; `Pair` keeps the connection open for the whole
//! pairing window.

use anyhow::Context;
use qrcode::QrCode;
use qrcode::render::unicode;
use rconnect_protocol::framing;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::paths;
use crate::store::PairedDevice;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    Status,
    ListDevices,
    /// Removes the devices matching a name or an id prefix.
    Revoke(String),
    /// Opens a pairing window; the connection then carries the pairing dialog.
    Pair,
    /// The user's answer to a [`ControlResponse::PairingRequest`].
    PairingDecision {
        accept: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    Status(Status),
    Devices(Vec<PairedDevice>),
    Revoked(Vec<PairedDevice>),
    PairingOpened {
        /// Pairing ticket URI, shown as a QR code.
        uri: String,
        /// How long the window stays open.
        timeout_secs: u64,
    },
    PairingRequest {
        device_name: String,
        /// Verification words the phone shows as well.
        words: Vec<String>,
    },
    PairingFinished(PairingResult),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    /// Hex encoded endpoint id.
    pub id: String,
    pub fingerprint: Vec<String>,
    pub speech_module_connected: bool,
    pub keyboard_available: bool,
    /// Name of the controller in an active session.
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingResult {
    Paired { device_name: String },
    TimedOut,
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect() -> anyhow::Result<Client> {
        let socket = paths::control_socket();
        let stream = UnixStream::connect(&socket).await.with_context(|| {
            format!(
                "cannot reach the RConnect daemon at {}. Is `rconnect daemon` running?",
                socket.display()
            )
        })?;
        let (reader, writer) = stream.into_split();
        Ok(Client {
            reader: BufReader::new(reader),
            writer,
        })
    }

    async fn send(&mut self, request: &ControlRequest) -> anyhow::Result<()> {
        framing::write_message(&mut self.writer, request).await?;
        Ok(())
    }

    async fn receive(&mut self) -> anyhow::Result<ControlResponse> {
        match framing::read_message(&mut self.reader).await? {
            Some(ControlResponse::Error(message)) => anyhow::bail!(message),
            Some(response) => Ok(response),
            None => anyhow::bail!("the daemon closed the connection"),
        }
    }

    async fn request(request: ControlRequest) -> anyhow::Result<ControlResponse> {
        let mut client = Client::connect().await?;
        client.send(&request).await?;
        client.receive().await
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

pub fn status() -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let ControlResponse::Status(status) = Client::request(ControlRequest::Status).await? else {
            anyhow::bail!("unexpected response");
        };
        println!("Name: {}", status.name);
        println!("Fingerprint: {}", status.fingerprint.join(" "));
        println!("Id: {}", status.id);
        println!(
            "Speech module: {}",
            if status.speech_module_connected {
                "connected"
            } else {
                "not connected"
            }
        );
        println!(
            "Keyboard: {}",
            if status.keyboard_available {
                "available"
            } else {
                "unavailable, check access to /dev/uinput"
            }
        );
        match status.session {
            Some(name) => println!("Session: active, controlled by {name}"),
            None => println!("Session: none"),
        }
        Ok(())
    })
}

pub fn list() -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let ControlResponse::Devices(devices) =
            Client::request(ControlRequest::ListDevices).await?
        else {
            anyhow::bail!("unexpected response");
        };
        if devices.is_empty() {
            println!("No paired devices. Run `rconnect pair` to pair a phone.");
        }
        for device in devices {
            let fingerprint = device
                .device_id()
                .map(|id| rconnect_protocol::words::fingerprint_words(&id).join(" "))
                .unwrap_or_default();
            println!("{}: {} ({})", device.name, fingerprint, &device.id[..16]);
        }
        Ok(())
    })
}

pub fn revoke(device: String) -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let ControlResponse::Revoked(removed) =
            Client::request(ControlRequest::Revoke(device.clone())).await?
        else {
            anyhow::bail!("unexpected response");
        };
        if removed.is_empty() {
            anyhow::bail!("no paired device matches {device:?}");
        }
        for device in removed {
            println!("Removed {}.", device.name);
        }
        Ok(())
    })
}

pub fn pair() -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let mut client = Client::connect().await?;
        client.send(&ControlRequest::Pair).await?;
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();

        loop {
            match client.receive().await? {
                ControlResponse::PairingOpened { uri, timeout_secs } => {
                    let code = QrCode::new(uri.as_bytes())?;
                    let image = code
                        .render::<unicode::Dense1x2>()
                        .dark_color(unicode::Dense1x2::Light)
                        .light_color(unicode::Dense1x2::Dark)
                        .quiet_zone(true)
                        .build();
                    println!("{image}");
                    println!("Pairing ticket: {uri}");
                    println!();
                    println!(
                        "Scan the QR code above with RConnect on your phone, or find this \
                         computer in the phone's list of computers on the local network."
                    );
                    println!(
                        "Pairing stays open for {} minutes. Press Ctrl+C to cancel.",
                        timeout_secs / 60
                    );
                }
                ControlResponse::PairingRequest { device_name, words } => {
                    println!();
                    println!("Pairing request from the phone {device_name:?}.");
                    println!("Verification words: {}.", words.join(", "));
                    println!(
                        "Pair only if the phone shows the same words. Pair with {device_name}? [y/N]"
                    );
                    let answer = stdin.next_line().await?.unwrap_or_default();
                    let accept = matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES");
                    client.send(&ControlRequest::PairingDecision { accept }).await?;
                    if !accept {
                        println!("Declined. Still waiting for a phone.");
                    }
                }
                ControlResponse::PairingFinished(PairingResult::Paired { device_name }) => {
                    println!("Paired with {device_name}.");
                    return Ok(());
                }
                ControlResponse::PairingFinished(PairingResult::TimedOut) => {
                    anyhow::bail!("pairing timed out");
                }
                other => anyhow::bail!("unexpected response {other:?}"),
            }
        }
    })
}
