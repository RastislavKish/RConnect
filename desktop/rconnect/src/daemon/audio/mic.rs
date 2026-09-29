//! The virtual microphone: the phone's microphone as a PipeWire source (see
//! `docs/ARCHITECTURE.md`, §5.7).
//!
//! PipeWire pulls audio from the source whenever an application records from it. The
//! process callback takes it from a [`StreamPlayer`], which the session feeds with the
//! phone's datagrams, and plays silence while there are none: muted, between VAD bursts,
//! or when the phone's microphone is unavailable.
//!
//! When the source disappears, something in the desktop (GNOME's sound settings, as far
//! as testing shows) replaces the user's configured default source with the remaining
//! one, so WirePlumber would not choose it again. The daemon therefore notes, before
//! removing it, whether it was the configured default, and restores that when it
//! creates the source again.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use rconnect_audio::StreamPlayer;

use super::RATE;
use super::capture::format_pod;
use crate::paths;

/// Name of the source node. It never changes, so WirePlumber can remember it as the
/// default source.
const NODE_NAME: &str = "rconnect-microphone";
/// Frames produced when PipeWire does not say how many it wants: 10 ms.
const FALLBACK_FRAMES: usize = 480;

/// A running virtual microphone; dropping it removes the source.
pub struct VirtualMic {
    player: Arc<StreamPlayer>,
    stop: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VirtualMic {
    /// Creates the source, described as `description` in applications' device lists.
    pub fn create(description: String) -> anyhow::Result<VirtualMic> {
        let player = Arc::new(StreamPlayer::new("microphone", 100, false));
        let (stop, stop_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("microphone".into())
            .spawn({
                let player = player.clone();
                move || {
                    let ready = ready_tx.clone();
                    if let Err(e) = run(description, player, stop_rx, move || {
                        let _ = ready.send(Ok(()));
                    }) {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        ready_rx.recv().context("the microphone thread stopped")??;
        tracing::info!("created the virtual microphone");
        if default_marker().exists() {
            match set_configured_default() {
                Ok(()) => tracing::info!("made the virtual microphone the default again"),
                Err(e) => tracing::warn!("cannot make the virtual microphone the default: {e:#}"),
            }
        }
        Ok(VirtualMic {
            player,
            stop,
            thread: Some(thread),
        })
    }

    /// Where the phone's audio goes.
    pub fn player(&self) -> &Arc<StreamPlayer> {
        &self.player
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        // Read before removing it: removing it makes the desktop configure another one.
        match configured_default() {
            Ok(name) if name.as_deref() == Some(NODE_NAME) => {
                if let Err(e) = std::fs::write(default_marker(), "") {
                    tracing::warn!("cannot remember the default microphone: {e}");
                }
            }
            Ok(_) => {
                let _ = std::fs::remove_file(default_marker());
            }
            Err(e) => tracing::debug!("cannot read the default microphone: {e:#}"),
        }
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        tracing::info!("removed the virtual microphone");
    }
}

fn run(
    description: String,
    player: Arc<StreamPlayer>,
    stop: pw::channel::Receiver<()>,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("cannot start PipeWire")?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("cannot connect to PipeWire")?;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::MEDIA_CLASS => "Audio/Source",
        *pw::keys::NODE_NAME => NODE_NAME,
        *pw::keys::NODE_DESCRIPTION => description,
        // A source: applications connect to it, it connects to nothing.
        "node.autoconnect" => "false",
        // 10 ms, as for the capture.
        "node.latency" => "480/48000",
    };
    let stream = pw::stream::StreamBox::new(&core, NODE_NAME, props)?;
    let _listener = stream
        .add_local_listener_with_user_data(player)
        .process(|stream, player| fill(stream, player))
        .register()?;

    let format = format_pod(&[spa::sys::SPA_AUDIO_CHANNEL_MONO])?;
    let mut params = [spa::pod::Pod::from_bytes(&format).context("invalid audio format")?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    ready();
    mainloop.run();
    Ok(())
}

/// Present while the user's configured default source is the virtual microphone.
fn default_marker() -> PathBuf {
    paths::data_dir().join("microphone-is-default")
}

const CONFIGURED_SOURCE: &str = "default.configured.audio.source";

/// The `node.name` of the user's configured default source, from WirePlumber's metadata.
fn configured_default() -> anyhow::Result<Option<String>> {
    let output = Command::new("pw-metadata")
        .args(["-n", "default", "0", CONFIGURED_SOURCE])
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_configured(&text))
}

/// Parses `update: id:0 key:'…' value:'{ "name": "…" }' type:'Spa:String:JSON'`.
fn parse_configured(text: &str) -> Option<String> {
    let value = text.split("value:'").nth(1)?.split('\'').next()?;
    let name = value.split("\"name\"").nth(1)?.split('"').nth(1)?;
    Some(name.to_owned())
}

/// Makes the virtual microphone the configured default; WirePlumber applies and saves it.
fn set_configured_default() -> anyhow::Result<()> {
    let status = Command::new("pw-metadata")
        .args([
            "-n",
            "default",
            "0",
            CONFIGURED_SOURCE,
            &format!("{{ \"name\": \"{NODE_NAME}\" }}"),
            "Spa:String:JSON",
        ])
        .output()?
        .status;
    anyhow::ensure!(status.success(), "pw-metadata failed");
    Ok(())
}

/// Produces the frames PipeWire asks for. The safe buffer wrapper does not expose how
/// many that is, and filling the whole buffer would add latency, so this uses the raw
/// `pw_buffer`.
fn fill(stream: &pw::stream::Stream, player: &StreamPlayer) {
    // SAFETY: the buffer comes from this stream and goes back to it before returning; its
    // pointers are checked for null, and the data is written only within `maxsize`.
    unsafe {
        let raw = stream.dequeue_raw_buffer();
        if raw.is_null() {
            return;
        }
        let buffer = (*raw).buffer;
        if !buffer.is_null() && (*buffer).n_datas > 0 && !(*buffer).datas.is_null() {
            let data = &mut *(*buffer).datas;
            let capacity = data.maxsize as usize / size_of::<f32>();
            let frames = match (*raw).requested as usize {
                0 => FALLBACK_FRAMES,
                requested => requested,
            }
            .min(capacity);
            if !data.data.is_null() && !data.chunk.is_null() {
                let out = std::slice::from_raw_parts_mut(data.data.cast::<f32>(), frames);
                out.fill(0.0);
                player.mix(out, 1, RATE, false);
                let chunk = &mut *data.chunk;
                chunk.offset = 0;
                chunk.stride = size_of::<f32>() as i32;
                chunk.size = (frames * size_of::<f32>()) as u32;
            }
        }
        stream.queue_raw_buffer(raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pw_metadata() {
        let text = "Found \"default\" metadata 41\nupdate: id:0 key:'default.configured.audio.source' \
                    value:'{ \"name\": \"rconnect-microphone\" }' type:'Spa:String:JSON'\n";
        assert_eq!(parse_configured(text), Some("rconnect-microphone".into()));
        assert_eq!(parse_configured("Found \"default\" metadata 41\n"), None);
    }
}
