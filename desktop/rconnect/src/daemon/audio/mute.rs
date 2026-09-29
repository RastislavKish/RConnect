//! Keeps the computer silent while its audio streams to the phone (`local_audio = false`).
//!
//! The default sink is muted through `wpctl`, which applies the mute where WirePlumber
//! expects it (the device's route for sound cards). The taps take the audio before the
//! sink's volume, so the phone still hears everything. Only a sink RConnect muted itself
//! is unmuted again, and a marker file lets the next daemon start unmute it after a crash.

use std::path::PathBuf;
use std::process::Command;

use crate::paths;

/// The sink RConnect muted; unmuted again when dropped.
pub struct LocalMute {
    id: String,
    name: String,
}

impl LocalMute {
    /// Mutes the default sink. Returns `None` if it was already muted, or muting failed.
    pub fn mute_default_sink() -> Option<LocalMute> {
        let (id, name) = match default_sink() {
            Ok(sink) => sink,
            Err(e) => {
                tracing::warn!("cannot keep the computer silent: {e:#}");
                return None;
            }
        };
        match is_muted(&id) {
            Ok(true) => return None,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("cannot keep the computer silent: {e:#}");
                return None;
            }
        }
        // Written first: a crash between muting and writing would leave it muted.
        if let Err(e) = std::fs::write(marker(), format!("{id}\n{name}\n")) {
            tracing::warn!("cannot keep the computer silent: {e}");
            return None;
        }
        if let Err(e) = set_mute(&id, true) {
            tracing::warn!("cannot keep the computer silent: {e:#}");
            let _ = std::fs::remove_file(marker());
            return None;
        }
        tracing::info!("muted {name} while audio streams to the phone");
        Some(LocalMute { id, name })
    }
}

impl Drop for LocalMute {
    fn drop(&mut self) {
        unmute(&self.id, &self.name);
    }
}

/// Unmutes a sink left muted by a daemon that did not stop cleanly.
pub fn restore_after_crash() {
    let Ok(text) = std::fs::read_to_string(marker()) else {
        return;
    };
    let mut lines = text.lines();
    if let (Some(id), Some(name)) = (lines.next(), lines.next()) {
        // Ids are reused after PipeWire restarts; only unmute the same sink.
        match sink_name(id) {
            Ok(current) if current == name => unmute(id, name),
            _ => tracing::info!("{name}, muted by an earlier run, is gone"),
        }
    }
    let _ = std::fs::remove_file(marker());
}

fn unmute(id: &str, name: &str) {
    match set_mute(id, false) {
        Ok(()) => tracing::info!("unmuted {name}"),
        Err(e) => tracing::warn!("cannot unmute {name}: {e:#}"),
    }
    let _ = std::fs::remove_file(marker());
}

fn marker() -> PathBuf {
    paths::data_dir().join("muted-sink")
}

fn wpctl(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("wpctl").args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "wpctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Id and `node.name` of the default sink.
fn default_sink() -> anyhow::Result<(String, String)> {
    let text = wpctl(&["inspect", "@DEFAULT_AUDIO_SINK@"])?;
    parse_inspect(&text).ok_or_else(|| anyhow::anyhow!("unexpected output of wpctl inspect"))
}

fn sink_name(id: &str) -> anyhow::Result<String> {
    let text = wpctl(&["inspect", id])?;
    parse_inspect(&text)
        .map(|(_, name)| name)
        .ok_or_else(|| anyhow::anyhow!("unexpected output of wpctl inspect"))
}

/// Parses `id 53, type PipeWire:Interface:Node` and the `node.name = "…"` line.
fn parse_inspect(text: &str) -> Option<(String, String)> {
    let id = text
        .lines()
        .next()?
        .strip_prefix("id ")?
        .split(',')
        .next()?
        .trim()
        .to_owned();
    let name = text.lines().find_map(|line| {
        let value = line
            .trim_start_matches([' ', '*'])
            .strip_prefix("node.name = ")?;
        Some(value.trim().trim_matches('"').to_owned())
    })?;
    Some((id, name))
}

fn is_muted(id: &str) -> anyhow::Result<bool> {
    Ok(wpctl(&["get-volume", id])?.contains("[MUTED]"))
}

fn set_mute(id: &str, mute: bool) -> anyhow::Result<()> {
    wpctl(&["set-mute", id, if mute { "1" } else { "0" }])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wpctl_inspect() {
        let text = "id 53, type PipeWire:Interface:Node\n    alsa.card = \"0\"\n  \
                    * node.name = \"alsa_output.pci-0000_00_07.0.analog-stereo\"\n";
        assert_eq!(
            parse_inspect(text),
            Some((
                "53".into(),
                "alsa_output.pci-0000_00_07.0.analog-stereo".into()
            ))
        );
        assert_eq!(parse_inspect("nothing"), None);
    }
}
