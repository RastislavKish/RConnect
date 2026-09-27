# RConnect
Use a full fledged desktop environment on your Android device

RConnect lets a blind user control a Linux desktop from an Android phone with a keyboard
attached. Keys go from the phone to the computer, and the text the screen reader speaks
comes back to the phone, where it is synthesized locally. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

Status: the desktop side works. The Android app builds but is not tested on a device yet.
`rconnect-dev-controller` stands in for it during development.

## Desktop installation

The reference setup is GNOME with Orca and speech-dispatcher.

1. Build and install the binary:

   ```sh
   cargo install --path desktop/rconnect
   ```

2. Allow typing through `/dev/uinput` (needs root once):

   ```sh
   sudo cp desktop/rconnect/dist/70-rconnect-uinput.rules /etc/udev/rules.d/
   sudo udevadm control --reload && sudo udevadm trigger --name-match=uinput
   ```

   Make sure the `uinput` kernel module is loaded (`ls /dev/uinput`); if not, add
   `uinput` to `/etc/modules-load.d/uinput.conf`.

3. Install the speech-dispatcher module. It passes speech through to the usual
   synthesizer and, during a session, to the phone:

   ```sh
   mkdir -p ~/.local/libexec/speech-dispatcher-modules ~/.config/speech-dispatcher/modules
   ln -s ~/.cargo/bin/rconnect ~/.local/libexec/speech-dispatcher-modules/sd_rconnect
   cp desktop/rconnect/dist/rconnect.conf ~/.config/speech-dispatcher/modules/
   ```

   Then add this line to `~/.config/speech-dispatcher/speechd.conf` (copy the file from
   `/etc/speech-dispatcher/` first if you do not have one), and select the `rconnect`
   synthesizer in Orca's preferences:

   ```
   AddModule "rconnect" "sd_rconnect" "rconnect.conf"
   ```

   Keep espeak-ng available as well, so you can switch back from Orca's preferences if
   anything goes wrong.

4. Start the daemon with your session:

   ```sh
   mkdir -p ~/.config/systemd/user
   cp desktop/rconnect/dist/rconnect.service ~/.config/systemd/user/
   systemctl --user enable --now rconnect
   ```

5. Pair a phone with `rconnect pair`, and check the state with `rconnect status`.

## Configuration

`~/.config/rconnect/config.toml`:

```toml
# Name shown on the phone (default: the host name).
name = "My laptop"
# Keep speaking on this computer during a session (default: true).
local_speech = true
```

## Commands

- `rconnect pair`: pair a phone. While it runs, the phone lists this computer under
  "Computers on this network"; it also shows a QR code and the pairing link. It then
  asks you to confirm the verification words shown on the phone. `--window` shows the
  QR code in an image viewer, which also happens when the terminal is too small.
- `rconnect list`: list paired phones with their fingerprints.
- `rconnect revoke <name>`: unpair a phone, ending its session if one is active.
- `rconnect status`: show this computer's fingerprint and what is connected.

## Building the Android app

Requirements: the Android SDK and NDK, Rust with the `aarch64-linux-android` and
`x86_64-linux-android` targets, `cargo-ndk`, CMake and a C compiler. Check out the eSpeak
NG and sonic submodules first:

```sh
git submodule update --init
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
cd mobile/RConnect && ./gradlew assembleDebug
```

Gradle runs the scripts in `mobile/scripts` to build the Rust core, generate its Kotlin
bindings and compile eSpeak NG's voice data. The first build takes a while.

After installing, turn on the RConnect key capture service in Android's accessibility
settings, so that keys the system handles itself (Alt+Tab, the Meta key) reach the
computer too. To pair, run `rconnect pair` on the computer, then in RConnect choose
"Pair a computer" and pick the computer from the local network list, scan the QR code
with any QR code scanner app (which opens RConnect), or paste the pairing link.

## Development

- `rconnect speech-monitor [--mirror]` stands in for the daemon and the phone, printing
  what the speech module sends.
- `rconnect-dev-controller pair <link> | hosts | session` (from `mobile/core`) stands in
  for the phone app.
- `RCONNECT_RUNTIME_DIR`, `RCONNECT_DATA_DIR`, `RCONNECT_CONFIG_DIR` and
  `RCONNECT_CONTROLLER_DIR` move all state, so tests do not touch a real installation.
