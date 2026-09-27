//! The `rconnect` CLI's link to the daemon, over [`paths::control_socket`], and the CLI
//! commands built on it.
//!
//! Each CLI invocation opens one connection and sends one [`ControlRequest`]. Most requests
//! get a single [`ControlResponse`]; `Pair` keeps the connection open for the whole
//! pairing window.

use anyhow::Context;
use std::io::IsTerminal;

use qrcode::render::unicode;
use qrcode::{Color, EcLevel, QrCode};
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

pub fn pair(window: bool) -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let mut client = Client::connect().await?;
        client.send(&ControlRequest::Pair).await?;
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        let mut qr_window = None;

        loop {
            match client.receive().await? {
                ControlResponse::PairingOpened { uri, timeout_secs } => {
                    // Low error correction: a screen has no smudges, and it keeps the code
                    // small enough for a standard terminal.
                    let code = QrCode::with_error_correction_level(uri.as_bytes(), EcLevel::L)?;
                    println!("Pairing link: {uri}");
                    println!(
                        "In RConnect on your phone, pick this computer from the computers on \
                         the local network, or scan the QR code with a QR code scanner app. \
                         Pairing stays open for {} minutes; press Ctrl+C to cancel.",
                        timeout_secs / 60
                    );
                    if window {
                        qr_window = Some(show_in_window(&code)?);
                    } else if !std::io::stdout().is_terminal() {
                        println!("The QR code is shown only in a terminal, or with --window.");
                    } else if fits_in_terminal(&code) {
                        // Printed last, so the whole code stays visible at the bottom.
                        print!("{}", terminal_qr(&code));
                    } else {
                        println!(
                            "The terminal is too small for the QR code, so it opens in an \
                             image viewer."
                        );
                        qr_window = Some(show_in_window(&code)?);
                    }
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
                    if let Some(path) = qr_window {
                        let _ = std::fs::remove_file(path);
                    }
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

/// Image viewers tried before the desktop's default application for PNG images.
const IMAGE_VIEWERS: &[&str] = &["loupe", "eog", "gwenview", "ristretto", "gthumb", "feh"];

/// Modules of blank border around a QR code, as scanners expect.
const QUIET_ZONE: usize = 4;

/// Size of [`terminal_qr`]'s output in columns and lines.
fn terminal_qr_size(code: &QrCode) -> (usize, usize) {
    let modules = code.width() + 2 * QUIET_ZONE;
    // Each character cell shows two modules stacked vertically.
    (modules, modules.div_ceil(2))
}

fn fits_in_terminal(code: &QrCode) -> bool {
    let Some((terminal_size::Width(columns), terminal_size::Height(lines))) =
        terminal_size::terminal_size()
    else {
        return false;
    };
    let (width, height) = terminal_qr_size(code);
    // One more line for the cursor below the code.
    width <= columns as usize && height < lines as usize
}

/// Renders the code with half blocks in black on white, set explicitly so it scans the same
/// on light and dark terminal themes.
fn terminal_qr(code: &QrCode) -> String {
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .quiet_zone(true)
        .build();
    image
        .lines()
        .map(|line| format!("\x1b[30;107m{line}\x1b[0m\n"))
        .collect()
}

/// Opens the code as a PNG image in the desktop's image viewer and returns its path.
fn show_in_window(code: &QrCode) -> anyhow::Result<std::path::PathBuf> {
    let path = paths::runtime_dir().join("pairing-qr.png");
    std::fs::write(&path, qr_png(code, 16)?)?;
    // Image viewers first: the default application for images may be an editor.
    let opened = IMAGE_VIEWERS
        .iter()
        .chain(["xdg-open"].iter())
        .any(|program| {
            std::process::Command::new(program)
                .arg(&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
        });
    if !opened {
        anyhow::bail!(
            "cannot open the QR code in an image viewer; it is in {}",
            path.display()
        );
    }
    Ok(path)
}

/// Encodes the code as a black on white grayscale PNG, `scale` pixels per module.
fn qr_png(code: &QrCode, scale: usize) -> anyhow::Result<Vec<u8>> {
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QUIET_ZONE) * scale;
    let mut pixels = vec![255u8; side * side];
    for (index, color) in colors.iter().enumerate() {
        if *color == Color::Dark {
            let x = (index % modules + QUIET_ZONE) * scale;
            let y = (index / modules + QUIET_ZONE) * scale;
            for row in y..y + scale {
                pixels[row * side + x..row * side + x + scale].fill(0);
            }
        }
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, side as u32, side as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ticket as the daemon creates it fits a standard 80x24 terminal.
    #[test]
    fn ticket_qr_fits_a_standard_terminal() {
        let ticket = rconnect_protocol::ticket::PairingTicket {
            host_id: [0xab; 32],
            secret: rconnect_protocol::ticket::PairingSecret::generate(),
            relay_url: None,
            direct_addrs: Vec::new(),
        };
        let uri = ticket.to_uri();
        let code = QrCode::with_error_correction_level(uri.as_bytes(), EcLevel::L).unwrap();
        let (width, height) = terminal_qr_size(&code);
        assert!(
            width <= 80 && height < 24,
            "{width}x{height} for {} characters",
            uri.len()
        );

        let rendered = terminal_qr(&code);
        assert_eq!(rendered.lines().count(), height);

        let png = qr_png(&code, 4).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
    }
}
