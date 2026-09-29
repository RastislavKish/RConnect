# RConnect Architecture

RConnect lets a blind user operate a Linux desktop from an Android phone with a
physical keyboard attached. Instead of streaming the screen, it streams what the
screen reader *says*: keystrokes go from the phone to the desktop, and the text
the screen reader wants spoken goes back to the phone, where it is synthesized
locally with eSpeak NG. Text and key events are tiny, so the network adds very
little latency.

This document describes the components, the network protocol and the reasoning
behind the main decisions. Items marked **(verify)** are assumptions that must be
confirmed before or during implementation.

Reference environment: GNOME on Wayland, Orca, speech-dispatcher, Android with a
Bluetooth or USB keyboard.

---

## 1. Overview

```
 Android phone                                         Linux desktop
┌──────────────────────────────┐                     ┌──────────────────────────────────┐
│ Kotlin UI (device list,      │                     │ rconnectd (user service)         │
│   pairing, settings)         │                     │   iroh endpoint, allowlist       │
│ Accessibility service        │   iroh / QUIC       │   session manager                │
│   (key filter)  ─────────────┼──── keys ──────────▶│   uinput virtual keyboard ──▶ compositor
│ Foreground service           │                     │   logind sleep inhibitor         │
│ Rust core (UniFFI)           │◀─── speech ─────────┼── control socket                 │
│   iroh endpoint              │──── speech events ─▶│        ▲                         │
│   protocol, session          │                     │        │ Unix socket             │
│   eSpeak NG ─▶ audio ring ─▶ │                     │ sd_rconnect (speechd module)     │
│   AAudio output stream       │                     │   proxy ⇄ real module (sd_espeak-ng)
└──────────────────────────────┘                     │        ▲                         │
                                                     │ speech-dispatcher ◀── Orca       │
                                                     │ rconnect CLI (pair/list/revoke)  │
                                                     └──────────────────────────────────┘
```

## 2. Desktop side

### 2.1 Components

| Component | Kind | Responsibility |
|---|---|---|
| `rconnectd` | systemd user service | Iroh endpoint, authentication, sessions, uinput, sleep inhibitor, control socket |
| `rconnect` | CLI | `pair`, `list`, `revoke`, `status`, settings; talks to `rconnectd` over the control socket |
| `sd_rconnect` | speech-dispatcher output module | Intercepts speech and forwards it to `rconnectd` or to the real module |

All three are one binary, `rconnect`: `rconnectd` is `rconnect daemon`, and the binary
acts as `sd_rconnect` when started through a symlink of that name in
`~/.local/libexec/speech-dispatcher-modules/`. The control and speech sockets live in
`$XDG_RUNTIME_DIR/rconnect/`. The identity key and paired devices are kept in
`~/.local/share/rconnect/` (files readable only by the user), and settings in
`~/.config/rconnect/config.toml`. Installation steps are in the README.

### 2.2 Keyboard injection: uinput

`rconnectd` creates one virtual keyboard through `/dev/uinput` that declares every
`KEY_*` code. The compositor treats it as a real keyboard, so the user's XKB layout,
key repeat and Orca's modifier handling all work unchanged.

- **Permissions:** `/dev/uinput` is root-only by default. Ship a udev rule, e.g.
  `KERNEL=="uinput", TAG+="uaccess"` (access for the user on the active seat), or a
  dedicated group.
- **Stuck keys:** the daemon tracks which keys are down. On `ReleaseAll`, session end,
  or connection loss it releases all of them.
- **Validation:** only key codes from an allowlist are accepted, and only while a
  session is active.

Rejected alternative: the RemoteDesktop portal with libei. It needs no permissions, but
it shows a consent dialog, depends on the compositor, and adds little over uinput.

### 2.3 Capturing speech: the `sd_rconnect` proxy module

Orca speaks through speech-dispatcher. speech-dispatcher starts each output module as a
child process (`module <configfile>`) and talks to it over stdin/stdout using a line
protocol based on SSIP. `sd_rconnect` is such a module.

#### Why a module instead of an Orca plugin

- It relies on a documented protocol that has been stable for years. An Orca plugin
  depends on Orca's Python internals, which change often.
- speech-dispatcher resolves priorities and interruptions before the module sees
  anything. The module only receives a clean sequence of `SPEAK`, `STOP` and `SET`.
- It is written in Rust, has no Python in the loop, and works with any speechd client.

#### Proxy design

Implemented in `desktop/rconnect/src/speech/module.rs`. `sd_rconnect` does **not** copy
bytes between speechd and the real module. It fully parses both sides (the protocol is
small) and acts as a speechd *server* towards the real module:

1. On start, it reads its own config, which names the real module binary and that
   module's config (for example `sd_espeak-ng`), and spawns it as a child.
2. **Configuration and query commands** are always forwarded to the real module, and
   its replies are relayed. The real module stays configured and ready at all times. The
   full command set, per speechd 0.12's `src/modules/module_main.c` and
   `module_process.c`, is `INIT` (always first), `SET`, `AUDIO`, `LOGLEVEL`,
   `LIST VOICES`, `DEBUG` and `QUIT`, plus the speech commands below. `SET` values are
   also recorded and sent to the phone. speechd repeats `SET` before every message, so
   only changes are forwarded.
3. **Speech commands** (`SPEAK`, `CHAR`, `KEY`, `SOUND_ICON`, `STOP`, `PAUSE`) are routed
   according to the output mode the daemon selects:
   - **Local:** forwarded to the real module. Its events (`700` marks, `701 BEGIN`,
     `702 END`, `703 STOP`, `704 PAUSE`, `705 AUDIO`, `706 ICON`) are relayed unchanged.
   - **Remote:** sent to `rconnectd`, which forwards them to the phone. The events are
     generated by `sd_rconnect` (see below), and the laptop is silent.
   - **Mirror:** sent to both. The real module drives the events, except that its `END`
     is held back until the phone has finished too (see below).
4. **Mode switches** take effect for the next message. If a message is playing on a
   sink the new mode doesn't use, that sink is sent `STOP`, and its terminating event is
   the one reported to speechd.
5. If `rconnectd` is not running or the socket drops, the module falls back to Local,
   and any utterance still playing remotely is reported as stopped. The user never loses
   speech. The module reconnects every second.

Everything runs in one task that handles inputs strictly in arrival order. The real
module always writes its reply before the events that follow it, and handling its output
in that order means an event can never overtake the reply it follows. speechd drops
events that arrive while it waits for a reply.

**speechd 0.12 plays module audio itself.** At startup speechd offers
`AUDIO audio_output_method=server`. The real module then returns synthesized audio as
`705 AUDIO` events, reports `702 END` as soon as synthesis is done, and speechd plays the
audio from its own queue. Two consequences:
- Once the module has sent `END`, speechd handles an interruption by flushing its queue,
  without sending `STOP` to the module. In Mirror mode the proxy therefore holds back the
  real module's `END` until the phone reports `End`. As long as speechd hasn't seen
  `END`, it sends `STOP`, which the proxy passes on to the phone. If `STOP` arrives while
  an `END` is held, the proxy releases the held `END` right away. A side effect is that
  in Mirror mode each message waits for the slower of the two devices.
- Declining server audio is not an option: at least the openSUSE build of `sd_espeak-ng`
  0.12 accepts only `audio_output_method=server`, and speechd aborts when no module
  initializes.

#### Development and testing

`rconnect speech-monitor [--mirror] [--ms-per-char N]` stands in for the daemon and the
phone: it listens on the speech socket, prints everything the module sends, and simulates
playback, returning marks and `End` at a fixed speed. `RCONNECT_RUNTIME_DIR` moves the
socket, so the monitor and module can run against an isolated speechd instance
(`speech-dispatcher -s -C <conf> -m <modules> -S <socket>`) without touching the user's
real session. Don't override `XDG_RUNTIME_DIR` for this, because speechd needs it to
find PipeWire. Without `--mirror`, the monitor silences the laptop's speech while
connected.

#### Events in Remote mode, and why they matter for latency

speechd waits for a terminating event (`END`, `STOP` or `PAUSE`) before it sends the
next command. Orca's Say All relies on `BEGIN`, `END` and index marks to move through
the document. So in Remote mode:

- `701 BEGIN` is emitted **locally** as soon as `SPEAK` is acknowledged.
- `703 STOP` is emitted **locally** right after `STOP` is forwarded. Waiting for the
  phone to confirm would add one full round trip to every interruption, which happens
  on nearly every keystroke. The phone treats a new `Speak` as cancelling anything
  still playing, and events for stale utterance ids are dropped.
- `702 END` and `700` index marks come **from the phone**, when the audio actually
  plays. This keeps Say All in sync with what the user hears.
- Optimization: the phone may report `End` slightly early, once synthesis is finished
  and the remaining buffered audio is shorter than the current RTT estimate. This hides
  the round-trip gap between queued messages.

#### SSML

Orca **does** send SSML: `speechdispatcherfactory.py` sets `DataMode.SSML`, and
`ssml.py` wraps text in `<speak>` with word-offset marks `<mark name="start:end"/>`
that are used to track Say All progress. speechd adds its own `__spd_id_N` marks.
Stripping the markup is therefore not enough, because the marks must survive.

speechd also preprocesses the text before the module sees it. Symbols and punctuation
are already spoken as words (`Tom & Jerry <3` arrives as `Tom  and  Jerry  less 3`), and
the module is sent `punctuation_mode=none`. So the phone needs no punctuation handling of
its own. `CHAR`, `KEY` and `SOUND_ICON` carry a single plain line, not SSML.

`sd_rconnect` parses the small SSML subset that occurs in practice (`speak`, `mark`,
entities, and passthrough of unknown elements as text) into a neutral form: text plus a
list of `(char_offset, mark_index)`. The mark names stay on the desktop in a
per-utterance table, and the phone only reports indexes. The phone rebuilds eSpeak NG
SSML with numeric marks and uses eSpeak's mark callbacks. This keeps the wire format
independent of SSML and of the phone's TTS engine.

#### Installation

- Symlink the `rconnect` binary as `~/.local/libexec/speech-dispatcher-modules/sd_rconnect`.
  When started under that name, the binary runs as the module.
- Create `~/.config/speech-dispatcher/modules/rconnect.conf` with
  `RealModule "sd_espeak-ng"` and `RealModuleConfig "espeak-ng.conf"` (these are the
  defaults). Relative names are looked up in the usual speechd directories.
- Add `AddModule "rconnect" "sd_rconnect" "rconnect.conf"` to
  `~/.config/speech-dispatcher/speechd.conf` and make it the default module, or select
  it in Orca.
- Voices: `LIST VOICES` is answered by the real module. When that is eSpeak NG, the
  voice list matches what the phone can synthesize.

### 2.4 Power management

During a session `rconnectd` holds logind inhibitor locks for `sleep` and `idle` (mode
`block`) over D-Bus, so the laptop does not suspend while being used remotely. The locks
are released when the session ends. They are requested separately, because polkit may
grant one and not the other; a denied lock is logged and the session continues.
`handle-lid-switch` is not requested, because polkit normally reserves it for privileged
callers. It isn't needed on GNOME, which handles the lid itself and suspends through
logind, so the `sleep` lock blocks it too.

### 2.5 Known limitations

- The GDM login screen runs its own Orca as the `gdm` user and does not use our module.
  Remote use after a reboot therefore needs automatic login, or is out of scope. The lock
  screen works, because it belongs to the user's session.
- Sounds that do not go through speech-dispatcher (Orca progress beeps, system sounds)
  are only heard through the optional audio stream (§5.6).

## 3. Android side

### 3.1 Components

- **Kotlin UI (Compose)** in `mobile/RConnect`:
  - `MainActivity`: the list of computers, pairing, settings, and the key capture
    service's state.
  - `PairActivity`: takes a pasted link, and opens `rconnect://pair` links, so any FOSS
    QR scanner can pair until built-in scanning is added.
  - `SessionActivity`: a running session.
- **Accessibility service** (`KeyCaptureService`): filters key events
  (`flagRequestFilterKeyEvents`, `onKeyEvent`). This runs before most system shortcut
  handling, so it captures more keys than an Activity can; Home and Power stay out of
  reach. Outside a session it passes every key through. The session screen's own
  `dispatchKeyEvent` covers the case where the service is off. Both feed `KeyRouter`.
  Future: a global shortcut that opens a session overlay from anywhere, like NVDA Remote.
- **Rust core** (`mobile/core`): pairing, sessions, the speech engine (§3.3), and
  `drive_session`, which connects them. `mobile/ffi` exposes it to Kotlin with UniFFI
  (`RConnect`, `SessionHandle`, `SessionListener`). Gradle builds it with `cargo-ndk`
  through `mobile/scripts/`.
- The session ends when the session screen stops (the user left it or turned the screen
  off), on CapsLock+F11, or with the End button. The screen stays on during a session.
  There is no foreground service yet.

F-Droid constraints: no Google Play Services and no ML Kit. Kotlin to Rust calls use JNA,
as UniFFI requires. The identity key never leaves the device: backups and device
transfers are disabled.

### 3.2 Keys

- Forward `KeyEvent.getScanCode()`, which on Android is the Linux evdev code. Fall back
  to a `KEYCODE_* → evdev` table when the scan code is 0 (`KeyCodes.kt`).
- Only forward transitions: drop events with `repeatCount > 0`. Key repeat happens on
  the desktop. A release is sent only for keys whose press was sent.
- **End-session shortcut**, CapsLock+F11 (configurable later). The modifier has already
  been forwarded by the time the second key arrives. The second key is swallowed,
  `ReleaseAll` is sent, and the session ends.
- The screen must be on and the phone unlocked. **(verify)** whether an accessibility
  service still receives key events from an external keyboard while the device is
  locked or the screen is off. If it does, the "phone in pocket" setup becomes possible.

### 3.3 Speech and audio output

Implemented in `mobile/core/src/speech/`:

- **eSpeak NG 1.52.0** is a git submodule (`mobile/third_party/espeak-ng`). `build.rs`
  builds it with CMake as a static C library: no speech-player (C++), async mode, audio
  output or MBROLA. It is driven through a dozen hand-written FFI declarations.
  **libsonic** (`mobile/third_party/sonic`, Apache 2.0, pinned to the commit eSpeak NG's
  CMake uses) makes rates above 450 wpm possible. eSpeak NG's CMake would otherwise
  download sonic while configuring; `FETCHCONTENT_SOURCE_DIR_SONIC-GIT` and
  `FETCHCONTENT_FULLY_DISCONNECTED` make every build use the vendored copy, without
  network access, as F-Droid requires. Cross builds build only the library target, because upstream's default
  target compiles voice data with the just-built binary, which can't run on the build
  machine. `mobile/scripts/build-espeak-data.sh` compiles the voice data (19 MB, 8 MB of
  it Russian) natively. The app ships it as assets and copies it to app storage on first
  run and after updates. eSpeak NG 1.52 reports `EINVAL` for the punctuation and
  capitals parameters even though it applies them; that status is ignored.
- **One thread owns eSpeak NG and the audio stream.** Synthesis runs in 20 ms chunks
  into a playback buffer. Marks are placed at their sample positions, and `End` at the
  end of the utterance. The audio callback fires each cue once the samples before it
  have been handed to the device, so marks and ends follow what the user hears.
- **Stop and new utterances act immediately** from the caller's thread. They bump a
  generation counter and clear the buffer, so audio stops within one audio callback and
  a running synthesis aborts at its next chunk.
- **One output stream stays open for the whole session:** AAudio in low-latency mode
  with the accessibility usage on Android (API 28, hence `minSdk` 28), cpal elsewhere. A
  linear resampler adapts eSpeak NG's 22.05 kHz to the device rate when needed. A stream
  that breaks, e.g. when headphones connect, is reopened. Benefits:
  - no stream start-up cost per utterance;
  - cancelling is just clearing the buffer;
  - Android does not put the Bluetooth link in standby.
- **Bluetooth keep-alive:** some headsets turn off on pure digital silence. An option
  plays noise at about -69 dBFS instead of zeros.
- **Speech settings are the phone's own** (settings screen, `VoiceSettings`):
  - **Voice and variant:** chosen from lists read from the voice files in
    `espeak-ng-data` (141 languages, 104 variants), and selected as `language+variant`.
  - **Rate:** 0–100 %, mapped like NVDA to 80–450 wpm. **Rate boost** triples it, to
    240–1350 wpm; eSpeak NG hands speeds of 450 wpm and more to sonic.
  - **Pitch:** 0–100, where 50 is normal. **Volume:** 0–100, where 100 is eSpeak NG's
    normal volume. All three scales run 0–100 in 5 % steps, as in NVDA and Orca.
  - **From the computer:** punctuation and capital letters still come from its settings,
    because they follow the screen reader's behaviour. Its rate, pitch, volume and
    language are ignored, like NVDA Remote uses the controlling machine's synthesizer
    settings. As a result, Orca's automatic language switching has no effect yet.
  - **Preview:** a Test button speaks a sample with the settings being edited. eSpeak NG
    runs once per process, so the preview's engine is shut down whenever the settings
    screen pauses, before a session can start its own.
- Measured with the dev controller: through the whole chain, an utterance ends when its
  audio ends (1.68 s for a sentence that takes 1.69 s locally), all marks arrive, and a
  cancel reaches speech-dispatcher in 2 ms.

## 4. Identity and pairing

### 4.1 Identities

Each side creates an Iroh secret key on first run and stores it: in app-private storage
on Android, and in `$XDG_DATA_HOME/rconnect/` on the desktop. The public key is the
device's **EndpointId**. QUIC/TLS proves both identities on every connection
(`Connection::remote_id()`), so the rest of the protocol needs no passwords.

`rconnectd` keeps an allowlist `{endpoint_id, name, paired_at}`. The phone keeps a
device list `{endpoint_id, name, last known addresses}`.

### 4.2 ALPNs

| ALPN | Who may connect | Purpose |
|---|---|---|
| `rconnect/pair/1` | anyone, **only while a pairing window is open** | pairing |
| `rconnect/session/1` | allowlisted EndpointIds only; others are closed immediately | sessions |

### 4.3 Pairing flow

1. The user runs `rconnect pair` in a terminal. This opens a pairing window (for
   example 5 minutes) in the daemon, and the prompt is shown in that terminal, where
   Orca reads it.
2. The phone reaches the laptop through one of two paths:
   - **LAN discovery:** the phone's pairing screen lists laptops found through mDNS
     (`iroh-mdns-address-lookup` with the service name `rconnect`,
     `MdnsAddressLookup::subscribe()`), and asks each one for its name with
     `PairRequest::Query`. The host answers that query at any time, not only while a
     pairing window is open. There is no secret in this case, so the verification words
     below are the only protection. On Android the pairing screen holds a Wi-Fi
     multicast lock while it is shown, which Android requires for receiving mDNS.
   - **QR code or link:** `rconnect://pair?id=<base32 EndpointId>&s=<base32 secret>`,
     about 90 characters. The one-time 80-bit secret stops strangers who only know the
     EndpointId from sending pairing requests. The link carries no addresses: the phone
     finds the host by its id, through mDNS on the LAN or iroh's DNS address lookup
     elsewhere. (`relay` and `addr` parameters are still accepted as optional hints.)
     Parameter names and values are case-insensitive, in case a scanner changes the
     case. With low error correction the QR code is 37×37 modules. It is drawn with
     half-block characters in explicit black-on-white colours, so it scans on light and
     dark terminal themes, and at 45×23 characters it fits a standard 80×24 terminal. It
     is printed last, so it stays fully visible. If the terminal is smaller, or with
     `rconnect pair --window`, it opens as a PNG in an image viewer (Loupe, Eye of GNOME
     and similar are tried before the desktop's default, which may be an image editor).
     Tests decode both renderings with zxing-cpp.
3. The phone sends `PairRequest{secret?, phone_name}`.
4. **Verification words:** both devices show the same word sequence derived from
   `hash(desktop_id ‖ phone_id)`. Each word encodes 11 bits as an index into a fixed
   2048-word English list (for example the BIP39 list, which was chosen so that words
   are distinct in their first four letters). The default is 4 words (44 bits),
   compared with about 20 bits for 6 digits. Words are easier to compare by ear with a
   screen reader, and it takes much more work to find a key whose words collide.
   Because TLS authenticates both IDs, matching words prove that the right two devices
   are talking. One combined sequence means the user compares once, instead of
   checking each device's fingerprint separately.
5. The desktop asks: *"Pair phone 'Pixel 8'? Verification words: orbit, candle,
   meadow, tunnel. [y/N]"*. Only an explicit yes adds the phone to the allowlist. The
   phone saves the laptop after receiving `PairAccepted{desktop_name}`.

Each device's own EndpointId can also be shown as words (for example the first 6 words
of its hash) in `rconnect list` and in the phone's device details, so a pairing can be
checked again later.

`rconnect list` and `rconnect revoke <name|id>` manage the allowlist. Revoking closes any
session that phone has open.

**Firewalls:** hosts send mDNS announcements repeatedly (about once a second), and
outgoing packets pass typical desktop firewalls. So the phone finds the host even when
the host's firewall drops incoming mDNS, as openSUSE's firewalld does by default; the
host then just doesn't answer the phone's queries, and the phone waits for the next
announcement. Connections work through the relay and hole punching either way.

## 5. Protocol

### 5.1 Transport

Iroh 1.x gives QUIC connections between EndpointIds. Connections start through the
home relay, then hole punching moves them to a direct path when possible, and they stay
on the relay only if that fails. With mDNS the LAN address is known in advance, so LAN
connections go direct almost immediately. Relays are configurable, so users can host
their own.

Iroh exposes the QUIC primitives that this design needs: `open_bi`, `open_uni`,
`accept_bi`, `accept_uni`, stream priorities, `send_datagram`, `read_datagram`,
`max_datagram_size`, `rtt()` and `paths()`. Iroh's QUIC implementation is `noq`.

Settings: a short idle timeout (about 2–3 s) with keep-alive, so a dead link is noticed
quickly and stuck keys are released.

### 5.2 Framing and encoding

- Each stream carries a sequence of frames: `varint length` followed by a `postcard`-encoded
  message.
- Each stream has its own message enum. Unknown enum tags produce a decode error that
  is logged and skipped. Compatibility is handled by the handshake, not by guessing.
- The message types live in a shared crate, `rconnect-protocol`, used by both the
  desktop and the Android core.

### 5.3 Streams

Streams are opened in a fixed order right after the connection is established, so each
side knows which is which without extra tags.

| # | Stream | Direction | Opened by | Priority |
|---|---|---|---|---|
| 1 | Control | bi | phone | high |
| 2 | Keys | uni, phone → desktop | phone | highest |
| 3 | Speech | bi (commands down, events up) | desktop | highest |
| 4 | Braille | bi | reserved, opened only when both sides announce the capability | high |
| – | Audio | datagrams | – | best effort |

Keys and speech use separate streams, so a lost speech packet never delays a key
press, and the reverse.

### 5.4 Messages

**Control**
```
Hello   { protocol_version: u16, app_version: String, device_name: String, capabilities: u32 }
Welcome { protocol_version: u16, app_version: String, device_name: String, capabilities: u32 }
Reject  { reason: RejectReason }        // Unsupported version, NotPaired, …
EndSession { reason: EndReason }        // user shortcut, desktop shutdown, replaced, …
AudioRequest { direction: AudioDir, mode: AudioMode }   // AudioMode = Auto | Always; also switches modes
AudioStarted { direction: AudioDir, mode: AudioMode, codec: Codec, sample_rate: u32, channels: u8, frame_ms: u8 }
AudioStop                               // back to Off
Ping(u32)                               // phone measures the round trip time
Pong(u32)                               // desktop's answer, same number
```
The capabilities bitset contains `BRAILLE`, `AUDIO_TO_CONTROLLER`, `AUDIO_TO_HOST`, and
future bits.

**Keys** (phone → desktop)
```
Key { code: u16, down: bool }   // ≈ 3–4 bytes on the wire
ReleaseAll
```

**Speech: desktop → phone**
```
Settings { rate: i8, pitch: i8, pitch_range: i8, voice: Option<String>, language: Option<String>,
           punctuation: Punct, spelling: bool, cap_let_recogn: CapMode }   // only on change
Speak { id: u32, kind: SpeechKind, text: String, marks: Vec<(u32 /*char offset*/, u16 /*mark idx*/)> }
        // SpeechKind = Text | Char | Key | SoundIcon; a new Speak cancels anything still playing
Stop  { id: u32 }
Pause { id: u32 }
```
**Speech: phone → desktop**
```
Mark   { id: u32, mark: u16 }
End    { id: u32 }
Paused { id: u32, last_mark: Option<u16> }
```
Events for an id other than the current one are ignored by the desktop.

**Braille (reserved)**, only a sketch:
```
desktop → phone: Cells { cells: Vec<u8>, cursor: Option<u16> }
phone → desktop: BrailleKey { keys: u32 }, Routing { cell: u16 }
```
On the desktop, the likely route is for `rconnectd` to act as a BrlAPI server that Orca
connects to. On the phone, how to reach the display is still open (BRLTTY for Android,
or the platform braille APIs).

### 5.5 Sessions

- **One active session.** A new session connection from an allowlisted phone replaces
  the existing one. This handles reconnecting after a network change before the old
  connection times out. A different phone is refused while a session is active, unless
  the user takes over explicitly (future option).
- **Start:** `Hello`/`Welcome` → `rconnectd` switches `sd_rconnect` to Remote (or Mirror, if the user keeps local speech on), creates
  the logind inhibitor, and enables key injection.
- **End:** the user shortcut, the phone leaving the session screen or overlay,
  `EndSession`, or connection loss. Then: release all keys, switch the module back to
  Local, and drop the inhibitor.

### 5.6 Optional audio stream

This is for audio CAPTCHAs, videos, and sounds that do not go through speech-dispatcher,
such as Orca's progress beeps. It goes from the desktop to the phone. The other
direction (microphone → Opus → a PipeWire virtual source) comes later. Implemented in
`desktop/rconnect/src/daemon/audio/` and `mobile/core/src/speech/host_audio.rs`.

#### Modes

The user picks the mode on the phone: in the settings, and during a session with
CapsLock+F10, which cycles Off, Auto and Always and is announced like the end of a
session. The phone swallows the F10, like the F11 of CapsLock+F11.

- **Off** (default): no PipeWire connection, no capture thread, no datagrams. The phone's
  audio callback only checks that there is no host audio. Keys and speech work exactly as
  without the feature.
- **Auto:** the host captures, but sends only while something audible plays. In Remote
  mode the laptop is silent while the screen reader speaks, so nothing is sent then.
- **Always:** a continuous stream, silence included. It avoids the gate's edge cases (clipped
  onsets, the jitter buffer refilling at every burst) at the cost of phone battery and
  some bandwidth. With VBR, silent Opus frames are only a few bytes.

`AudioRequest` starts the stream or switches its mode, and `AudioStop` returns to Off. The
host starts the capture on the request and tears it down on stop, so Off costs nothing
even in the middle of a session. The host answers every request with `AudioStarted`, or
with `AudioStop` when it cannot stream. It announces `AUDIO_TO_CONTROLLER` unless
`audio = "off"` is set in its configuration.

#### Capture: tapping application streams

The daemon connects to PipeWire as a client and creates a capture stream,
`rconnect-audio`, with `node.passive = true` and `node.autoconnect = false`. It watches the
registry and links the output ports of every application playback stream
(`Stream/Output/Audio`) to this node, **in addition to** the stream's existing link to its
sink. A port can feed several inputs, so nothing is rerouted. Vencord's venmic captures
per-application audio for Discord the same way. The links are created with
`link.passive = true`, so they never keep the graph running either.

- The user's setup is not changed: sinks, the default sink, profiles and volumes stay as
  they are. When the user moves a stream to another sink, the tap stays, because it hangs
  on the stream's ports.
- The links are not lingering (`object.linger` unset), so they belong to the daemon's
  connection. They disappear when the stream ends, the audio stops, or the daemon exits or
  crashes. Nothing is written to WirePlumber's state or the user's configuration.
- **speech-dispatcher's stream is not tapped.** This is what makes streaming compatible
  with Mirror mode: local speech keeps playing on the laptop and is not captured and
  repeated on the phone. It also keeps local speech from opening the Auto gate.
  speech-dispatcher's streams carry no `application.name`; they are recognised by their
  `node.name`, `speech-dispatcher-<module>`.
- **Loopback and filter streams are not tapped** (EasyEffects, `module-loopback`, filter
  chains). They carry audio that the applications behind them already produced, so
  tapping them would duplicate it. `pw-loopback`'s playback stream has `node.link-group`
  and `node.virtual = true`; streams with either, or with `stream.monitor = true`, are
  skipped. These properties are not in the registry's global properties, so each
  playback stream is bound to read its full ones; only info events that change the
  properties count, because state changes arrive with empty ones. **(verify)** with
  EasyEffects.
- RConnect's own processes are not tapped either (any of `node.name`,
  `application.name` or `application.process.binary` containing `rconnect`). Otherwise the
  dev controller, playing the stream on the same computer, would hear itself.
- The tapped audio comes before the sink's volume: with the sink muted, a tone still
  opened the Auto gate and played on the controller. So the laptop can be muted or turned
  down while the phone still hears everything. Each stream's own volume still applies
  **(verify)** where PipeWire applies stream volume.
- Output channels are folded to the capture's FL and FR: left channels to FL, right ones
  to FR, the rest (mono, centre, aux) to both, LFE to neither. PipeWire mixes several
  links into one input port.
- The node asks for a latency of about 10 ms (`node.latency`). The quantum is shared by
  the whole graph, so asking for less makes all of the host's audio wake up more often.
- Applications that open ALSA hardware directly bypass PipeWire and are not captured.
- WirePlumber 0.5.15 left the tap links alone for as long as the streams played.

**Fallback**, `audio = "monitor"` in the host's configuration: capture the default sink's
monitor instead. The monitor contains local speech, so while audio streams, `sd_rconnect`
is switched from Mirror to Remote. The capture is passive too, so it does not keep the
sink running; **(verify)** whether it follows the sink's volume and mute. The daemon
needs PipeWire either way: without it, audio requests are answered with `AudioStop`.

#### The Auto gate

A voice activity detector (such as the Earshot crate) is the wrong tool here. It tells
speech from everything else, so it would drop beeps, notifications, music and the
non-speech parts of an audio CAPTCHA. The gate only asks whether anything audible plays:

1. While no tapped stream is running, the passive node gets no data, and there is nothing
   to analyse. When data stops arriving for 60 ms while the gate is open, the burst is
   ended with a silent frame.
2. Otherwise, each frame's peak is compared with a threshold of -60 dBFS, because
   browsers and games often keep streams open that play silence. The gate opens on the
   first frame above it, and closes after 400 ms below it, so quiet passages and
   fading sounds don't make it flap.

In Always mode, the encoder makes up silent frames at the frame rate while no data
arrives, so the stream stays continuous. They start from the moment they are needed: a
gap that already passed, e.g. before a switch from Auto, is not made up in a burst,
which would fill the phone's buffer with a second of silence.

- **Pre-roll:** the last one or two frames are kept and sent first when the gate opens,
  so onsets are not clipped. The gate decides per frame without lookahead, so it adds no
  latency. The Opus encoder is reset when the gate opens.
- **End of burst:** the last datagram of a burst carries an end flag, so the phone stops
  instead of running packet loss concealment into a gap it would take for loss. A lost
  end flag is caught by the underrun handling: at most three concealed, fading frames.
- Opus DTX was considered. It is designed for voice and still sends a packet every few
  hundred milliseconds of silence; the gate is simpler and does exactly this.

#### Wire format

- Opus, 48 kHz stereo, restricted low-delay mode (about 5 ms algorithmic delay), VBR at
  64 kbps, 20 ms frames (10 ms is possible at twice the packet rate). Silent frames are a
  few bytes.
- One frame per datagram, after a 7-byte header (`protocol/src/audio.rs`): `seq: u16`,
  `timestamp: u32`, flags (end of burst), then the payload. Late datagrams are dropped,
  never retransmitted.
- Datagrams share the connection's congestion control with keys and speech. The host's
  datagram send buffer is 2 KB (quinn's default is 1 MB), so when the congestion window
  is full, stale audio is dropped instead of queued ahead of speech. On a relayed
  connection, checked every second, the bitrate drops to 32 kbps; **(verify)** the
  relays' rate limits.

#### Playback on the phone

- A jitter buffer with Opus packet loss concealment. Playback starts once 2 packets
  (40 ms) are buffered; each underrun raises that by one, up to 6, and 30 s without one
  lowers it again. A missing packet with later ones waiting is concealed; with nothing
  waiting, up to three frames are concealed, fading out, before playback stops and
  buffers again. Packets whose time has passed are dropped. In Auto it refills at the
  start of every burst, and the decoder is reset with the encoder. Each new stream on
  the host numbers its packets from zero, so `AudioStarted` resets the jitter buffer; a
  jump of more than 100 packets does too, in case the datagrams overtake that message.
- **Clock drift:** the laptop's sound card and the phone's DAC differ by up to about
  100 ppm, so an hour of continuous audio drifts by about 360 ms. The resampler that
  converts 48 kHz to the device's rate runs up to 0.4 % faster or slower, in proportion to
  how far the smoothed buffer level is from its target. Gaps in Auto reset the drift, but
  long bursts (a whole video) need the correction as well.
- **Mixing happens in the audio callback**, from a separate buffer. Host audio is never
  appended to `Playback`, whose marks and ends are tied to speech sample positions. So
  speech latency does not change, and `Stop` clears only speech. Host audio is ducked to
  30 % while speech plays (a setting, on by default), and has its own volume setting,
  because the output stream follows the accessibility volume. Gain changes are ramped
  across a buffer to avoid clicks.
- The output stream runs at the device's native rate in stereo; no rate is requested.
  Speech is upsampled from eSpeak NG's 22.05 kHz with the existing resampler. Android
  recommends not requesting a sample rate for the low-latency path, so this may help
  speech latency too **(verify)** on a device.
- libopus 1.6.1 is vendored as a submodule (`mobile/third_party/opus`) and built with
  CMake, like eSpeak NG, for F-Droid. `rconnect-opus` (`opus/`) wraps it with a handful of
  hand-written declarations. The desktop links the system's libopus when pkg-config finds
  it, and builds the vendored copy otherwise; it also needs libpipewire.

#### Latency

Host audio, end to end (estimates):

| Stage | Typical |
|---|---|
| PipeWire quantum | 5–21 ms (1024 samples at 48 kHz by default) |
| Opus frame + algorithmic delay | 10–20 ms + about 5 ms |
| Network (LAN) | 2–5 ms |
| Jitter buffer | 20–60 ms |
| AAudio output | 5–20 ms |
| Bluetooth headphones | +100–200 ms |

About 50–120 ms without Bluetooth: fine for videos and CAPTCHAs. Speech is synthesized on
the phone and so arrives 50–100 ms earlier than host audio, so an Orca beep may be heard
slightly out of order with the speech around it.

The stream barely affects keys and speech: keys travel the other way, and the stream is
small on a LAN. With the dev controller on the same machine, Ping gave 2.9 ms while
streaming. One side effect: a continuous stream keeps both Wi-Fi radios awake. Always
therefore hides the power-saving delay described in the README, while in Auto key latency
changes depending on whether audio happens to play, which makes Ping results confusing.

## 6. Code layout

```
protocol/                 rconnect-protocol crate (messages, framing, pairing URI, verification words)
desktop/rconnect/         rconnectd + CLI + sd_rconnect (one binary, subcommands)
  src/daemon/             endpoint, pairing, sessions, uinput, speech hub, sleep inhibitor
  src/daemon/audio/       audio stream: PipeWire tap, Auto gate, Opus encoder
  src/speech/             sd_rconnect module, its IPC with the daemon, speech-monitor
  dist/                   udev rule, systemd user unit, module config
opus/                     rconnect-opus crate: minimal libopus bindings for both sides
mobile/core/              Rust core for Android: pairing, sessions, eSpeak NG speech and
                          the host audio stream; rconnect-dev-controller test binary
mobile/RConnect/          Android app (Kotlin)
docs/                     this document
```

## 7. Out of scope for now

Low-priority experiments for later:
- Mouse and explore-by-touch: absolute pointer through uinput, and touch-to-screen
  scaling.
- Clipboard sync on Wayland (`wlr-data-control` / portal) and file transfer.
- Virtual braille keyboard on the touch screen: it is translated to key events on the
  phone and reuses the Keys stream.
- Several simultaneous controllers.

## 8. Open questions and verification list

- [x] Full list of speechd module commands: see §2.3.
- [x] `705 AUDIO` passes through the proxy as a relayed event. Tested with speechd 0.12.1
      and `sd_espeak-ng`.
- [x] Test `sd_rconnect` with Orca itself, not only a Python client mimicking Orca: works
      with Orca and `spd-say`.
- [ ] Test the Android app on a device: AAudio latency, key capture with and without the
      accessibility service, and how TalkBack's own key filtering interacts with it.
- [ ] Whether the accessibility service receives external key events while the phone is
      locked or the screen is off.
- [ ] Which key combinations Android still intercepts before accessibility key filtering,
      on the Android versions we target.
- [x] Whether `iroh-mdns-address-lookup` can advertise a display name: it can, as the
      endpoint's "user data", but that data is also published through iroh's DNS
      (pkarr) address lookup. To avoid publishing the computer's name on the internet,
      it isn't set. The phone asks a discovered host for its name with
      `PairRequest::Query`.
- [x] How Iroh 1.x exposes idle timeout and keep-alive settings: through
      `QuicTransportConfig` (`max_idle_timeout`, `keep_alive_interval`). Both sides use a
      3 s timeout and a 1 s keep-alive. In testing, a key held when the phone vanished
      was released 3.1 s later.
- [x] Audio stream (§5.6), tested with PipeWire 1.6.9, WirePlumber 0.5.15 and the dev
      controller: taps are made next to the sink links and removed with the stream,
      speech-dispatcher and `pw-loopback` streams are left out, and a muted sink still
      streams.
- [ ] Audio stream (§5.6): EasyEffects and filter chains, where stream volume is applied
      relative to the tap, whether the monitor fallback follows the sink's mute, the
      relays' rate limits, and playback on an Android device.
