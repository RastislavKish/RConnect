//! Persistent state: the host's identity, the paired controllers and the configuration.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use iroh::{EndpointId, SecretKey};
use rconnect_protocol::{DeviceId, hex};
use serde::{Deserialize, Serialize};

use crate::paths;

/// Loads the host's secret key, creating it on first run.
pub fn load_or_create_secret_key() -> anyhow::Result<SecretKey> {
    let dir = paths::data_dir();
    let path = dir.join("secret.key");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("{} is corrupted", path.display()))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            paths::create_private_dir(&dir)?;
            write_private(&path, &key.to_bytes())?;
            tracing::info!("created a new identity in {}", path.display());
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Writes a file readable only by the current user, replacing it atomically.
fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    /// Hex encoded endpoint id.
    pub id: String,
    pub name: String,
    /// Seconds since the Unix epoch.
    pub paired_at: u64,
}

impl PairedDevice {
    pub fn new(id: &EndpointId, name: String) -> PairedDevice {
        let paired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        PairedDevice {
            id: hex::encode(id.as_bytes()),
            name,
            paired_at,
        }
    }

    pub fn device_id(&self) -> Option<DeviceId> {
        hex::decode(&self.id)
    }
}

/// The controllers allowed to start sessions, stored as JSON.
pub struct Devices {
    path: PathBuf,
    devices: Vec<PairedDevice>,
}

impl Devices {
    pub fn load() -> anyhow::Result<Devices> {
        let path = paths::data_dir().join("devices.json");
        let devices = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is corrupted", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(Devices { path, devices })
    }

    pub fn list(&self) -> &[PairedDevice] {
        &self.devices
    }

    pub fn find(&self, id: &EndpointId) -> Option<&PairedDevice> {
        let id = hex::encode(id.as_bytes());
        self.devices.iter().find(|device| device.id == id)
    }

    /// Adds a device, replacing an earlier pairing of the same device.
    pub fn add(&mut self, device: PairedDevice) -> anyhow::Result<()> {
        self.devices.retain(|existing| existing.id != device.id);
        self.devices.push(device);
        self.save()
    }

    /// Removes the devices matching `query`: a full name (case insensitive) or a prefix of
    /// the hex id of at least 8 characters.
    pub fn remove(&mut self, query: &str) -> anyhow::Result<Vec<PairedDevice>> {
        let query = query.trim();
        let matches = |device: &PairedDevice| {
            device.name.eq_ignore_ascii_case(query)
                || (query.len() >= 8 && device.id.starts_with(&query.to_ascii_lowercase()))
        };
        let (removed, kept) = self.devices.drain(..).partition(|device| matches(device));
        self.devices = kept;
        if !removed.is_empty() {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            paths::create_private_dir(dir)?;
        }
        write_private(&self.path, &serde_json::to_vec_pretty(&self.devices)?)
    }
}

/// `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name shown on the phone. Defaults to the host name.
    pub name: String,
    /// Keep speaking on this computer during a session.
    pub local_speech: bool,
    /// How audio is captured for the phone's audio stream.
    pub audio: AudioCapture,
    /// Keep playing audio on this computer while it streams to the phone. When false,
    /// the default output is muted meanwhile, local speech included.
    pub local_audio: bool,
    /// Offer the phone's microphone as a microphone on this computer.
    pub microphone: bool,
}

/// How audio is captured when the phone asks for it (see `docs/ARCHITECTURE.md`, §5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCapture {
    /// Tap each application's stream, leaving out speech-dispatcher.
    Streams,
    /// Capture the default sink's monitor. Local speech is turned off while it runs.
    Monitor,
    /// Never stream audio.
    Off,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            name: hostname(),
            local_speech: true,
            audio: AudioCapture::Streams,
            local_audio: true,
            microphone: true,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        let path = paths::config_dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Linux computer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices() -> Devices {
        let dir = std::env::temp_dir().join(format!("rconnect-test-{}", std::process::id()));
        let path = dir.join(format!("devices-{:?}.json", std::thread::current().id()));
        let _ = std::fs::remove_file(&path);
        Devices {
            path,
            devices: Vec::new(),
        }
    }

    #[test]
    fn add_find_remove() {
        let mut devices = devices();
        let phone = SecretKey::generate().public();
        let tablet = SecretKey::generate().public();
        devices
            .add(PairedDevice::new(&phone, "Pixel".into()))
            .unwrap();
        devices
            .add(PairedDevice::new(&tablet, "Tablet".into()))
            .unwrap();
        devices
            .add(PairedDevice::new(&phone, "Pixel 8".into()))
            .unwrap();
        assert_eq!(devices.list().len(), 2);
        assert_eq!(devices.find(&phone).unwrap().name, "Pixel 8");

        let saved: Vec<PairedDevice> =
            serde_json::from_slice(&std::fs::read(&devices.path).unwrap()).unwrap();
        assert_eq!(saved, devices.list());

        assert!(devices.remove("pixel").unwrap().is_empty());
        assert!(
            devices
                .remove(&hex::encode(tablet.as_bytes())[..4])
                .unwrap()
                .is_empty()
        );
        assert_eq!(devices.remove("PIXEL 8").unwrap().len(), 1);
        assert_eq!(
            devices
                .remove(&hex::encode(tablet.as_bytes())[..8])
                .unwrap()
                .len(),
            1
        );
        assert!(devices.list().is_empty());
        assert!(devices.find(&phone).is_none());
    }
}
