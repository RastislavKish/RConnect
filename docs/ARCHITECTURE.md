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

All three can be one binary with subcommands. `sd_rconnect` is then a small wrapper
installed in `~/.local/libexec/speech-dispatcher-modules/`. The control socket lives in
`$XDG_RUNTIME_DIR/rconnect/`.

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

`sd_rconnect` does **not** copy bytes between speechd and the real module. It fully
parses both sides (the protocol is small) and acts as a speechd *server* towards the
real module:

1. On start, it reads its own config, which names the real module binary and that
   module's config (for example `sd_espeak-ng`), and spawns it as a child.
2. **Configuration and query commands** (`INIT`, `AUDIO`, `SET`, `LIST VOICES`,
   `LOGLEVEL`, `DEBUG`, `QUIT`, **(verify)** the full list against speechd's
   `src/modules/module_main.c`) are always forwarded to the real module, and its
   replies are relayed. The real module stays configured and warm at all times. `SET`
   values are also cached and sent to the phone.
3. **Speech commands** (`SPEAK`, `CHAR`, `KEY`, `SOUND_ICON`, `STOP`, `PAUSE`) go to
   exactly one *sink* depending on the current mode:
   - **Local:** forwarded to the real module. Its events (`700` marks, `701 BEGIN`,
     `702 END`, `703 STOP`, `704 PAUSE`, `705 AUDIO`, `706 ICON`) are relayed unchanged.
   - **Remote:** sent to `rconnectd`, which forwards them to the phone. The events are
     generated by `sd_rconnect` (see below). Optionally a *mirror* setting also forwards
     speech to the real module, with the real module's events discarded.
4. **Mode switches** happen only between messages. If a message is active when the mode
   changes, the active sink is sent `STOP`, and its terminating event is the one
   reported to speechd.
5. If `rconnectd` is not running or the socket drops, the module falls back to Local.
   The user never loses speech.

Invariant to keep: an event must never be written between a synchronous command and its
reply. A single writer task that owns stdout and serializes replies and events enforces
this.

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

`sd_rconnect` parses the small SSML subset that occurs in practice (`speak`, `mark`,
entities, and passthrough of unknown elements as text) into a neutral form: text plus a
list of `(char_offset, mark_index)`. The mark names stay on the desktop in a
per-utterance table, and the phone only reports indexes. The phone rebuilds eSpeak NG
SSML with numeric marks and uses eSpeak's mark callbacks. This keeps the wire format
independent of SSML and of the phone's TTS engine.

#### Installation

- Install the module binary and a config naming the real module.
- Add `AddModule "rconnect" "sd_rconnect" "rconnect.conf"` to
  `~/.config/speech-dispatcher/speechd.conf` and make it the default module, or select
  it in Orca.
- Voices: `LIST VOICES` is answered by the real module. When that is eSpeak NG, the
  voice list matches what the phone can synthesize.

### 2.4 Power management

During a session `rconnectd` holds a logind inhibitor lock (`sleep:idle:handle-lid-switch`,
mode `block`) over D-Bus, so the laptop does not suspend while being used remotely. The
lock is released when the session ends.

### 2.5 Known limitations

- The GDM login screen runs its own Orca as the `gdm` user and does not use our module.
  Remote use after a reboot therefore needs automatic login, or is out of scope. The lock
  screen works, because it belongs to the user's session.
- Sounds that do not go through speech-dispatcher (Orca progress beeps, system sounds)
  are only heard through the optional audio stream (§5.6).

## 3. Android side

### 3.1 Components

- **Kotlin UI (Compose):** list of paired devices, pairing (QR scan or LAN discovery),
  settings.
- **Accessibility service:** filters key events (`flagRequestFilterKeyEvents`,
  `onKeyEvent`). This runs before most system shortcut handling, so it captures more
  keys than an Activity can. Home and Power stay out of reach. Outside a session it
  passes every key through. Future: a global shortcut that opens a session overlay from
  anywhere, like NVDA Remote.
- **Foreground service:** owns the session, keeps the connection and audio alive, and
  holds a screen-on wake lock.
- **Rust core** (via UniFFI, built with cargo-ndk): Iroh endpoint, protocol, session
  state, eSpeak NG, and the audio output.

F-Droid constraints: no Google Play Services and no ML Kit. Camera: CameraX. QR decoding:
ZXing or zxing-cpp.

### 3.2 Keys

- Forward `KeyEvent.getScanCode()`, which on Android is the Linux evdev code. Fall back
  to a `KEYCODE_* → evdev` table when the scan code is 0.
- Only forward transitions: drop events with `repeatCount > 0`. Key repeat happens on
  the desktop.
- **End-session shortcut**, default `CapsLock+F11` and configurable. The modifier has
  already been forwarded by the time the second key arrives. The second key is
  swallowed, `ReleaseAll` is sent, and the session ends.
- The screen must be on and the phone unlocked. **(verify)** whether an accessibility
  service still receives key events from an external keyboard while the device is
  locked or the screen is off. If it does, the "phone in pocket" setup becomes possible.

### 3.3 Speech and audio output

- eSpeak NG is compiled with the NDK through the Rust core's build script. Voice data
  is bundled and unpacked on first run. It is GPLv3, like RConnect.
- **One output stream stays open for the whole session** (cpal on AAudio, low-latency
  performance mode). eSpeak writes PCM into a ring buffer, and the audio callback plays
  silence whenever the buffer is empty. Benefits:
  - no stream start-up cost per utterance;
  - cancelling is just clearing the buffer;
  - Android does not put the Bluetooth link in standby.
- **Bluetooth keep-alive:** some headsets turn off on pure digital silence. An option
  plays inaudible noise (around −60 dBFS) instead of zeros.
- Speech settings (`rate`, `pitch`, `voice`, `language`, `punctuation`, …) come from the
  desktop's `SET` values and are mapped to eSpeak NG parameters. The phone may override
  them locally.

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
   - **QR:** the terminal shows a QR code with
     `rconnect://pair?id=<EndpointId>&secret=<one-time secret>&relay=…&addr=…`.
     The one-time secret stops strangers who only know the EndpointId from sending
     pairing requests.
   - **LAN discovery:** the phone lists laptops found through mDNS
     (`iroh-mdns-address-lookup`, `MdnsAddressLookup::subscribe()`). There is no
     secret in this case, so the verification code below is the only protection.
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
AudioRequest { direction: AudioDir }    // desktop→phone, phone→desktop, duplex
AudioStarted { direction: AudioDir, codec: Codec, sample_rate: u32, frame_ms: u8 }
AudioStop
```
The capabilities bitset contains `BRAILLE`, `AUDIO_OUT`, `AUDIO_IN`, and future bits.

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
- **Start:** `Hello`/`Welcome` → `rconnectd` switches `sd_rconnect` to Remote, creates
  the logind inhibitor, and enables key injection.
- **End:** the user shortcut, the phone leaving the session screen or overlay,
  `EndSession`, or connection loss. Then: release all keys, switch the module back to
  Local, and drop the inhibitor.

### 5.6 Optional audio stream

This is for audio CAPTCHAs, videos and similar.

- Desktop → phone: capture the default sink's monitor through PipeWire, Opus at
  10–20 ms frames, one frame per datagram (`seq: u16`, `timestamp: u32`, payload). The
  phone runs a small jitter buffer with Opus packet loss concealment, and the result is
  mixed into the same output stream as speech.
- While audio is streaming, `sd_rconnect` must not mirror speech locally. Otherwise the
  captured audio would repeat speech the phone has already spoken.
- Phone → desktop (duplex, later): microphone → Opus → a PipeWire virtual source.
- Late datagrams are dropped, never retransmitted.

## 6. Code layout (proposed)

```
protocol/                 rconnect-protocol crate (messages, framing, pairing URI, verification words)
desktop/rconnect/         rconnectd + CLI + sd_rconnect (one binary, subcommands)
mobile/core/              Rust core for Android (UniFFI), espeak-ng build, audio
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

- [ ] Full list of speechd module commands (`module_main.c`), including `INIT` and
      `LIST VOICES` reply formats.
- [ ] Whether `705 AUDIO` (server-side audio) from the real module passes through the
      proxy correctly. It should, as a relayed event.
- [ ] Whether the accessibility service receives external key events while the phone is
      locked or the screen is off.
- [ ] Which key combinations Android still intercepts before accessibility key filtering,
      on the Android versions we target.
- [ ] Whether `iroh-mdns-address-lookup` can advertise a display name, or whether the
      phone must connect on the pairing ALPN to learn it.
- [ ] How Iroh 1.x exposes idle timeout and keep-alive settings.
