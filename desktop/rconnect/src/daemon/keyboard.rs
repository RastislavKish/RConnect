//! Virtual keyboard through `/dev/uinput`.

use std::collections::BTreeSet;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, KeyCode, KeyEvent};
use rconnect_protocol::messages::KeyMessage;

const DEVICE_NAME: &str = "RConnect virtual keyboard";

/// Whether a key code may be injected: keyboard keys only, no mouse, joystick or other
/// `BTN_*` buttons.
fn allowed(code: u16) -> bool {
    matches!(code, 1..=0xff | 0x160..=0x2ff)
}

pub struct Keyboard {
    device: Option<VirtualDevice>,
    /// Keys pressed through RConnect, released when the session ends.
    pressed: BTreeSet<u16>,
}

impl Keyboard {
    pub fn new() -> Keyboard {
        let mut keyboard = Keyboard {
            device: None,
            pressed: BTreeSet::new(),
        };
        keyboard.ensure_device();
        keyboard
    }

    pub fn is_available(&self) -> bool {
        self.device.is_some()
    }

    /// Creates the virtual device if it does not exist yet. Retried at every session
    /// start, so fixing the permissions does not require restarting the daemon.
    pub fn ensure_device(&mut self) -> bool {
        if self.device.is_none() {
            match create_device() {
                Ok(device) => {
                    tracing::info!("created the virtual keyboard");
                    self.device = Some(device);
                }
                Err(e) => tracing::warn!(
                    "cannot create the virtual keyboard: {e}. Keys from the phone will be \
                     ignored; grant access to /dev/uinput (see the README)"
                ),
            }
        }
        self.device.is_some()
    }

    pub fn apply(&mut self, message: KeyMessage) {
        match message {
            KeyMessage::Down(code) if allowed(code) => {
                self.pressed.insert(code);
                self.emit(code, 1);
            }
            KeyMessage::Up(code) if allowed(code) => {
                self.pressed.remove(&code);
                self.emit(code, 0);
            }
            KeyMessage::ReleaseAll => self.release_all(),
            KeyMessage::Down(code) | KeyMessage::Up(code) => {
                tracing::debug!("ignoring key code {code}");
            }
        }
    }

    pub fn release_all(&mut self) {
        if !self.pressed.is_empty() {
            tracing::debug!("releasing keys {:?}", self.pressed);
        }
        for code in std::mem::take(&mut self.pressed) {
            self.emit(code, 0);
        }
    }

    fn emit(&mut self, code: u16, value: i32) {
        let Some(device) = &mut self.device else {
            return;
        };
        if let Err(e) = device.emit(&[*KeyEvent::new(KeyCode::new(code), value)]) {
            tracing::warn!("cannot inject key {code}: {e}");
        }
    }
}

fn create_device() -> std::io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in (0..=0x2ff).filter(|&code| allowed(code)) {
        keys.insert(KeyCode::new(code));
    }
    VirtualDevice::builder()?
        .name(DEVICE_NAME)
        .with_keys(&keys)?
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_keyboard_keys_are_allowed() {
        assert!(allowed(1)); // KEY_ESC
        assert!(allowed(58)); // KEY_CAPSLOCK
        assert!(allowed(125)); // KEY_LEFTMETA
        assert!(allowed(0x1d2)); // KEY_FN_ESC
        assert!(!allowed(0)); // KEY_RESERVED
        assert!(!allowed(0x110)); // BTN_LEFT
        assert!(!allowed(0x300));
    }

    #[test]
    fn tracks_pressed_keys_without_a_device() {
        let mut keyboard = Keyboard {
            device: None,
            pressed: BTreeSet::new(),
        };
        keyboard.apply(KeyMessage::Down(58));
        keyboard.apply(KeyMessage::Down(30));
        keyboard.apply(KeyMessage::Down(0x110));
        keyboard.apply(KeyMessage::Up(30));
        assert_eq!(keyboard.pressed, BTreeSet::from([58]));
        keyboard.apply(KeyMessage::ReleaseAll);
        assert!(keyboard.pressed.is_empty());
    }
}
