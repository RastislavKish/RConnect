# RConnect
Use a full fledged desktop environment on your Android device

RConnect lets a blind user control a Linux desktop from an Android phone with a keyboard
attached. Keys go from the phone to the computer, and the text the screen reader speaks
comes back to the phone, where it is synthesized locally. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

Status: the desktop side and the Android app work. `rconnect-dev-controller` can stand
in for the app during development.

## Desktop installation

The reference setup is GNOME with Orca and speech-dispatcher.

1. Build and install the binary. It needs the PipeWire and Opus development files, as
   well as clang (on openSUSE `pipewire-devel`, `libopus-devel` and `clang`; on Debian
   and Ubuntu `libpipewire-0.3-dev`, `libopus-dev` and `libclang-dev`):

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

6. If the computer is on Wi-Fi, turn off Wi-Fi power saving, or speech can lag by up to
   about 100 ms. See [Latency](#latency).

## Configuration

`~/.config/rconnect/config.toml`:

```toml
# Name shown on the phone (default: the host name).
name = "My laptop"
# Keep speaking on this computer during a session (default: true).
local_speech = true
# How the phone's audio stream is captured (default: "streams"). "streams" taps each
# application's playback, leaving out speech-dispatcher; "monitor" captures everything
# the speakers play and turns local speech off while it streams; "off" disables the
# audio stream.
audio = "streams"
# Keep playing audio on this computer while it streams to the phone (default: true).
# false mutes the computer's output while the stream is on, local speech included, and
# unmutes it afterwards, even after a crash. It needs audio = "streams".
local_audio = true
# Offer the phone's microphone as a microphone on this computer (default: true).
microphone = true
```

## Audio from the computer

Besides speech, the phone can play the computer's own audio: videos, audio CAPTCHAs,
Orca's progress beeps. There are three modes:

- **Off** (default): no audio from the computer, and no cost at all.
- **Auto:** audio streams only while something is playing.
- **Always:** audio streams all the time, silence included.

In the app's settings, check the modes you want and put them in order, by dragging a
mode after a long press or with the Move up and Move down accessibility actions. A session starts with the first checked mode, and CapsLock+F10 switches to
the next checked one. With only Off and Auto checked, for example, the shortcut toggles
between those two.

The settings also have a volume for the computer's audio, and whether to turn it down
while speech plays. The computer's audio reaches the phone about 50 to 120 ms after it
plays, more with Bluetooth headphones. Muting the computer does not mute the stream, so
the computer can stay silent while the phone plays its audio; `local_audio = false` does
that for you whenever the stream is on.

## Microphone

The phone's microphone can be a microphone on the computer, for calls and dictation. It
shows up in applications as "RConnect microphone (your phone's name)". There are four
modes:

- **Off:** no microphone on the computer.
- **Muted:** the computer has the microphone, but it stays silent.
- **Unmuted:** the phone's microphone is sent all the time.
- **VAD:** voice activity detection; the phone's microphone is sent only while someone
  speaks.

As with the computer's audio, check the modes you want in the app's settings and put
them in order. A session starts in the first checked mode, and CapsLock+F9 moves to the
next one. By default a session starts with Off, the first press creates the microphone
muted, and then the shortcut switches between Muted and Unmuted.

Once the computer has the microphone, CapsLock+F9 skips Off, so a call does not lose it
and quietly switch to the computer's own microphone. A setting allows removing it during
a session. It is always removed when the session ends.

The app always records from the phone's own microphone, even with a headset connected,
and asks for the microphone permission when you first check Unmuted or VAD; without it,
the computer's microphone stays silent. Use headphones, or the computer will hear what
the phone plays.

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
NG, sonic and Opus submodules first:

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

## Latency

The Ping button on the phone's session screen shows the round trip time to the computer
and whether the connection is direct or goes through a relay server. On a local network
it should be a few milliseconds.

If pings jump between a few milliseconds and about 100 ms, Wi-Fi power saving is the
usual cause. A sleeping Wi-Fi radio wakes up only at the router's beacons, about every
100 ms, and whatever is sent to it waits at the router until then. The phone turns power
saving off during sessions by itself, but a laptop running RConnect has to be configured.
GNOME and KDE do not offer this setting, so use NetworkManager directly.

Check the current state (find the interface name with `iw dev`):

```sh
iw dev wlan0 get power_save
```

Turn it off for one network (`2` is off, `3` on, `0` the global default):

```sh
nmcli connection modify "<network name>" 802-11-wireless.powersave 2
nmcli connection up "<network name>"
```

Or for all networks, in `/etc/NetworkManager/conf.d/wifi-powersave.conf`, followed by
`sudo systemctl restart NetworkManager`:

```ini
[connection]
wifi.powersave = 2
```

If TLP is installed, it sets Wi-Fi power saving itself when switching between mains and
battery; set `WIFI_PWR_ON_AC=off` and `WIFI_PWR_ON_BAT=off` in `/etc/tlp.conf` too. The
cost is some battery life while on battery.

While the computer's audio streams, the Wi-Fi radios stay awake, which hides power saving:
pings can look fine in Always mode, or while something plays in Auto, and still jump
once the stream is quiet.

A relayed connection is slower than a direct one, since everything passes through a
server on the internet; it is used when the two devices cannot reach each other directly.

## Development

- `rconnect speech-monitor [--mirror]` stands in for the daemon and the phone, printing
  what the speech module sends.
- `rconnect-dev-controller pair <link> | hosts | session` (from `mobile/core`) stands in
  for the phone app. In a session, `audio off|auto|always` switches the audio stream and
  `mic off|muted|unmuted|vad` the microphone (`RCONNECT_DEV_MIC=tone` or `noise` sends a
  test signal instead of the default input);
  `RUST_LOG=rconnect_core=debug` shows when bursts start and end.
- The daemon's log: `journalctl --user -u rconnect` (it is a user service). For more
  detail, add `Environment=RUST_LOG=rconnect=debug` to the unit with
  `systemctl --user edit rconnect`. The app's log: `adb logcat -s RConnect`.
- `RCONNECT_RUNTIME_DIR`, `RCONNECT_DATA_DIR`, `RCONNECT_CONFIG_DIR` and
  `RCONNECT_CONTROLLER_DIR` move all state, so tests do not touch a real installation.
