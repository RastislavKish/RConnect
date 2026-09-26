//! `sd_rconnect`: a speech-dispatcher output module that sits between speech-dispatcher
//! and a real output module (e.g. `sd_espeak-ng`).
//!
//! speech-dispatcher talks to output modules over stdin/stdout with a line protocol (see
//! "Communication Protocol for Output Modules" in the speech-dispatcher manual and
//! `src/modules/module_process.c`). This module starts the real module as a child and:
//!
//! * forwards configuration commands (`INIT`, `SET`, `AUDIO`, `LIST VOICES`, ...) to it
//!   and relays its replies, so the real module is always configured and ready;
//! * sends speech (`SPEAK`, `CHAR`, `KEY`, `SOUND_ICON`, `STOP`, `PAUSE`) to the child,
//!   to the daemon, or to both, depending on the [`OutputMode`] the daemon selects.
//!
//! Everything runs in one task that handles inputs strictly in arrival order. This keeps
//! the child's replies ahead of its later events, as speech-dispatcher requires.
//!
//! In [`OutputMode::Remote`], `BEGIN` and `STOP` are reported to speech-dispatcher right
//! away, while `END` and index marks come from the controller as audio actually plays.
//! Waiting for the controller to confirm a stop would add a network round trip to every
//! interruption.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use rconnect_protocol::framing;
use rconnect_protocol::messages::{
    Mark, SpeechCommand, SpeechEvent, SpeechKind, SpeechSettings, Utterance,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::ipc::{DaemonMessage, IPC_VERSION, ModuleMessage, OutputMode};
use super::{settings, ssml};
use crate::paths;

const DEFAULT_REAL_MODULE: &str = "sd_espeak-ng";
const DEFAULT_REAL_MODULE_CONFIG: &str = "espeak-ng.conf";
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

macro_rules! log {
    ($($arg:tt)*) => { eprintln!("sd_rconnect: {}", format_args!($($arg)*)) };
}

pub fn run(config: Option<PathBuf>) -> anyhow::Result<()> {
    let config = Config::load(config.as_deref());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_async(config))
}

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

/// Contents of the module's configuration file (`rconnect.conf`):
///
/// ```text
/// RealModule "sd_espeak-ng"
/// RealModuleConfig "espeak-ng.conf"
/// ```
///
/// Relative names are looked up in the usual speech-dispatcher directories.
struct Config {
    real_module: PathBuf,
    real_module_config: PathBuf,
}

impl Config {
    fn load(path: Option<&Path>) -> Config {
        let mut module = DEFAULT_REAL_MODULE.to_string();
        let mut module_config = DEFAULT_REAL_MODULE_CONFIG.to_string();

        if let Some(path) = path {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    for line in text.lines().map(str::trim) {
                        let Some((key, value)) = line.split_once(char::is_whitespace) else {
                            continue;
                        };
                        let value = value.trim().trim_matches('"').to_string();
                        match key {
                            "RealModule" => module = value,
                            "RealModuleConfig" => module_config = value,
                            _ => {}
                        }
                    }
                }
                Err(e) => log!("cannot read {}: {e}; using defaults", path.display()),
            }
        }

        Config {
            real_module: find_file(&module, &module_dirs()),
            real_module_config: find_file(&module_config, &module_config_dirs()),
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn module_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = home()
        .map(|home| home.join(".local/libexec/speech-dispatcher-modules"))
        .into_iter()
        .collect();
    dirs.extend(
        [
            "/usr/libexec/speech-dispatcher-modules",
            "/usr/lib/speech-dispatcher-modules",
            "/usr/lib/x86_64-linux-gnu/speech-dispatcher-modules",
            "/usr/lib/aarch64-linux-gnu/speech-dispatcher-modules",
            "/usr/local/libexec/speech-dispatcher-modules",
        ]
        .map(PathBuf::from),
    );
    dirs
}

fn module_config_dirs() -> Vec<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".config")));
    let mut dirs: Vec<PathBuf> = config_home
        .map(|dir| dir.join("speech-dispatcher/modules"))
        .into_iter()
        .collect();
    dirs.push("/etc/speech-dispatcher/modules".into());
    dirs
}

fn find_file(name: &str, dirs: &[PathBuf]) -> PathBuf {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    dirs.iter()
        .map(|dir| dir.join(path))
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

// ---------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------

enum Input {
    /// One line from speech-dispatcher, including the trailing `\n`.
    Upstream(Vec<u8>),
    UpstreamClosed,
    /// One complete message (possibly several lines) from the real module.
    Child(Vec<u8>),
    ChildClosed,
    DaemonConnected(mpsc::UnboundedSender<ModuleMessage>),
    Daemon(DaemonMessage),
    DaemonDisconnected,
}

async fn run_async(config: Config) -> anyhow::Result<()> {
    log!(
        "starting {} with {}",
        config.real_module.display(),
        config.real_module_config.display()
    );
    let mut child = Command::new(&config.real_module)
        .arg(&config.real_module_config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start {}", config.real_module.display()))?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_upstream(tokio::io::stdin(), tx.clone()));
    tokio::spawn(read_child(child.stdout.take().expect("piped"), tx.clone()));
    tokio::spawn(daemon_link(tx));

    let mut proxy = Proxy::new(tokio::io::stdout(), child.stdin.take().expect("piped"));
    while let Some(input) = rx.recv().await {
        match input {
            Input::Upstream(line) => proxy.on_upstream_line(line).await?,
            Input::Child(message) => proxy.on_child_message(message).await?,
            Input::DaemonConnected(sender) => proxy.on_daemon_connected(sender),
            Input::Daemon(message) => proxy.on_daemon_message(message).await?,
            Input::DaemonDisconnected => proxy.on_daemon_disconnected().await?,
            Input::UpstreamClosed => break,
            Input::ChildClosed => {
                if !proxy.quitting {
                    anyhow::bail!("the real output module exited unexpectedly");
                }
                break;
            }
        }
    }

    // Give the real module a moment to finish after QUIT before it is killed.
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    Ok(())
}

async fn read_upstream(input: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<Input>) {
    let mut reader = BufReader::new(input);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(Input::Upstream(line)).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Input::UpstreamClosed);
}

/// Reads the real module's output as complete messages. A message ends with a line that
/// has a space after the status code (`200 OK`); `200-...` lines continue it. This is the
/// same rule speech-dispatcher uses.
async fn read_child(output: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<Input>) {
    let mut reader = BufReader::new(output);
    let mut message = Vec::new();
    loop {
        let start = message.len();
        match reader.read_until(b'\n', &mut message).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = &message[start..];
                if (line.len() < 4 || line[3] == b' ')
                    && tx.send(Input::Child(std::mem::take(&mut message))).is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Input::ChildClosed);
}

/// Keeps a connection to the daemon, reconnecting while it is not running.
async fn daemon_link(tx: mpsc::UnboundedSender<Input>) {
    let socket = paths::speech_socket();
    loop {
        if let Ok(stream) = UnixStream::connect(&socket).await {
            let (reader, mut writer) = stream.into_split();
            let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ModuleMessage>();
            let _ = out_tx.send(ModuleMessage::Hello {
                version: IPC_VERSION,
            });
            if tx.send(Input::DaemonConnected(out_tx)).is_err() {
                return;
            }

            let writer_task = tokio::spawn(async move {
                while let Some(message) = out_rx.recv().await {
                    if framing::write_message(&mut writer, &message).await.is_err() {
                        break;
                    }
                }
            });

            let mut reader = BufReader::new(reader);
            loop {
                match framing::read_message::<_, DaemonMessage>(&mut reader).await {
                    Ok(Some(message)) => {
                        if tx.send(Input::Daemon(message)).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.is_recoverable() => log!("ignoring daemon message: {e}"),
                    Ok(None) | Err(_) => break,
                }
            }
            writer_task.abort();
            if tx.send(Input::DaemonDisconnected).is_err() {
                return;
            }
        }
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------------------
// Proxy state machine
// ---------------------------------------------------------------------------------------

/// What to do with the next reply from the real module. Replies arrive in the order the
/// commands were sent, so a queue of these is enough to route them.
enum OnReply {
    /// Pass it to speech-dispatcher.
    Relay,
    /// Swallow it (the acknowledgement of a command this module already acknowledged).
    Discard,
    /// Pass it on, and if the module refused to speak, forget the utterance.
    SpeakResult,
}

enum UpstreamState {
    /// Waiting for a command.
    Command,
    /// Inside a `SET`, `AUDIO` or `LOGLEVEL` block, which goes to the real module.
    Block { is_set: bool },
    /// Receiving the text of a speech command.
    Speech {
        kind: SpeechKind,
        command: Vec<u8>,
        lines: Vec<Vec<u8>>,
    },
}

/// Who is producing events for the utterance speech-dispatcher currently waits on.
enum Owner {
    Nobody,
    /// The real module, with a copy playing on the controller in mirror mode.
    Child {
        mirror: Option<Mirror>,
    },
    /// The controller. `marks` maps mark indexes back to their names.
    Remote {
        id: u32,
        marks: Vec<String>,
    },
}

/// Copy of the current utterance playing on the controller in [`OutputMode::Mirror`].
///
/// speech-dispatcher 0.12 plays the real module's audio itself. The module reports `END`
/// as soon as it has handed over the audio, and from then on speech-dispatcher interrupts
/// playback without sending `STOP` to the module. To keep interruptions reaching the
/// controller, the real module's `END` is held back until the controller finishes too.
struct Mirror {
    id: u32,
    held_end: Option<Vec<u8>>,
}

/// The proxy state machine. `O` writes to speech-dispatcher, `C` to the real module.
struct Proxy<O, C> {
    stdout: O,
    child: C,
    pending_replies: VecDeque<OnReply>,
    upstream: UpstreamState,
    owner: Owner,
    /// Mode requested by the daemon. Only effective while the daemon is connected.
    mode: OutputMode,
    daemon: Option<mpsc::UnboundedSender<ModuleMessage>>,
    settings: SpeechSettings,
    /// Last settings sent to the daemon. speech-dispatcher repeats `SET` before every
    /// message, but only changes are forwarded.
    sent_settings: Option<SpeechSettings>,
    next_id: u32,
    quitting: bool,
}

impl<O: AsyncWrite + Unpin, C: AsyncWrite + Unpin> Proxy<O, C> {
    fn new(stdout: O, child: C) -> Proxy<O, C> {
        Proxy {
            stdout,
            child,
            pending_replies: VecDeque::new(),
            upstream: UpstreamState::Command,
            owner: Owner::Nobody,
            mode: OutputMode::Local,
            daemon: None,
            settings: SpeechSettings::default(),
            sent_settings: None,
            next_id: 0,
            quitting: false,
        }
    }

    fn effective_mode(&self) -> OutputMode {
        if self.daemon.is_some() {
            self.mode
        } else {
            OutputMode::Local
        }
    }

    async fn reply(&mut self, text: &[u8]) -> anyhow::Result<()> {
        self.stdout.write_all(text).await?;
        self.stdout.flush().await?;
        Ok(())
    }

    async fn send_to_child(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.child.write_all(data).await?;
        self.child.flush().await?;
        Ok(())
    }

    fn send_to_daemon(&mut self, command: SpeechCommand) {
        if let Some(daemon) = &self.daemon {
            let _ = daemon.send(ModuleMessage::Speech(command));
        }
    }

    // --- speech-dispatcher side ---

    async fn on_upstream_line(&mut self, line: Vec<u8>) -> anyhow::Result<()> {
        match std::mem::replace(&mut self.upstream, UpstreamState::Command) {
            UpstreamState::Command => self.on_command(line).await,

            UpstreamState::Block { is_set } => {
                self.send_to_child(&line).await?;
                if line == b".\n" {
                    self.pending_replies.push_back(OnReply::Relay);
                    if is_set {
                        self.send_settings();
                    }
                } else {
                    if is_set {
                        settings::apply(&mut self.settings, &String::from_utf8_lossy(&line));
                    }
                    self.upstream = UpstreamState::Block { is_set };
                }
                Ok(())
            }

            UpstreamState::Speech {
                kind,
                command,
                mut lines,
            } => {
                if line == b".\n" {
                    self.on_speech(kind, command, lines).await
                } else {
                    lines.push(line);
                    self.upstream = UpstreamState::Speech {
                        kind,
                        command,
                        lines,
                    };
                    Ok(())
                }
            }
        }
    }

    async fn on_command(&mut self, line: Vec<u8>) -> anyhow::Result<()> {
        let speech_kind = match line.as_slice() {
            b"SPEAK\n" => Some(SpeechKind::Text),
            b"CHAR\n" => Some(SpeechKind::Char),
            b"KEY\n" => Some(SpeechKind::Key),
            b"SOUND_ICON\n" => Some(SpeechKind::SoundIcon),
            _ => None,
        };
        if let Some(kind) = speech_kind {
            self.upstream = UpstreamState::Speech {
                kind,
                command: line,
                lines: Vec::new(),
            };
            return self.reply(b"202 OK RECEIVING MESSAGE\n").await;
        }

        match line.as_slice() {
            b"STOP\n" => self.stop().await,
            b"PAUSE\n" => self.pause().await,
            b"SET\n" | b"AUDIO\n" | b"LOGLEVEL\n" => {
                self.upstream = UpstreamState::Block {
                    is_set: line == b"SET\n",
                };
                self.send_to_child(&line).await?;
                self.pending_replies.push_back(OnReply::Relay);
                Ok(())
            }
            // INIT, LIST VOICES, DEBUG, QUIT, and anything unknown: the real module answers.
            _ => {
                if line == b"QUIT\n" {
                    self.quitting = true;
                }
                self.send_to_child(&line).await?;
                self.pending_replies.push_back(OnReply::Relay);
                Ok(())
            }
        }
    }

    async fn on_speech(
        &mut self,
        kind: SpeechKind,
        command: Vec<u8>,
        lines: Vec<Vec<u8>>,
    ) -> anyhow::Result<()> {
        if lines.is_empty() {
            return self.reply(b"301 ERROR CANT SPEAK\n").await;
        }

        let mode = self.effective_mode();
        let remote_copy = match mode {
            OutputMode::Local => None,
            OutputMode::Remote | OutputMode::Mirror => {
                let (utterance, marks) = self.utterance(kind, &lines);
                let id = utterance.id;
                self.send_to_daemon(SpeechCommand::Speak(utterance));
                Some((id, marks))
            }
        };

        match (mode, remote_copy) {
            (OutputMode::Remote, Some((id, marks))) => {
                self.owner = Owner::Remote { id, marks };
                self.reply(b"200 OK SPEAKING\n701 BEGIN\n").await
            }
            (_, mirror) => {
                // Events may only follow the reply, so the owner can be set right away.
                let mirror = mirror.map(|(id, _)| Mirror { id, held_end: None });
                self.owner = Owner::Child { mirror };
                let mut data = command;
                for line in &lines {
                    data.extend_from_slice(line);
                }
                data.extend_from_slice(b".\n");
                self.pending_replies.push_back(OnReply::Discard);
                self.pending_replies.push_back(OnReply::SpeakResult);
                self.send_to_child(&data).await
            }
        }
    }

    /// Builds the utterance sent to the controller from the raw lines of a speech command.
    fn utterance(&mut self, kind: SpeechKind, lines: &[Vec<u8>]) -> (Utterance, Vec<String>) {
        let mut raw = String::new();
        for line in lines {
            let line = String::from_utf8_lossy(line);
            // Undo the dot escaping of lines that start with a dot.
            raw.push_str(line.strip_prefix('.').unwrap_or(&line));
        }
        if raw.ends_with('\n') {
            raw.pop();
        }

        let (text, names, marks) = match kind {
            SpeechKind::Text => {
                let parsed = ssml::parse(&raw);
                let marks = (parsed.marks.iter().enumerate())
                    .map(|(index, (offset, _))| Mark {
                        offset: *offset,
                        index: index as u16,
                    })
                    .collect();
                let names = parsed.marks.into_iter().map(|(_, name)| name).collect();
                (parsed.text, names, marks)
            }
            _ => (raw, Vec::new(), Vec::new()),
        };

        self.next_id = self.next_id.wrapping_add(1);
        (
            Utterance {
                id: self.next_id,
                kind,
                text,
                marks,
            },
            names,
        )
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        match self.owner {
            Owner::Remote { id, .. } => {
                self.owner = Owner::Nobody;
                self.send_to_daemon(SpeechCommand::Stop { id });
                self.reply(b"703 STOP\n").await
            }
            Owner::Child { .. } => {
                // A held `END` means the real module is already done; releasing it ends
                // the utterance. Otherwise the child stays the owner until it reports
                // that it stopped.
                if self.drop_mirror().await? {
                    return Ok(());
                }
                self.send_to_child(b"STOP\n").await
            }
            Owner::Nobody => self.send_to_child(b"STOP\n").await,
        }
    }

    async fn pause(&mut self) -> anyhow::Result<()> {
        match self.owner {
            Owner::Remote { id, .. } => {
                self.send_to_daemon(SpeechCommand::Pause { id });
                Ok(())
            }
            _ => {
                // A mirrored copy cannot resume in sync with the real module; stop it.
                if self.drop_mirror().await? {
                    return Ok(());
                }
                self.send_to_child(b"PAUSE\n").await
            }
        }
    }

    /// Stops the mirrored copy of the current utterance, if any. Returns whether this
    /// released a held `END` and so finished the utterance.
    async fn drop_mirror(&mut self) -> anyhow::Result<bool> {
        let Owner::Child { mirror } = &mut self.owner else {
            return Ok(false);
        };
        let Some(Mirror { id, held_end }) = mirror.take() else {
            return Ok(false);
        };
        self.send_to_daemon(SpeechCommand::Stop { id });
        match held_end {
            Some(end) => {
                self.owner = Owner::Nobody;
                self.reply(&end).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // --- real module side ---

    async fn on_child_message(&mut self, message: Vec<u8>) -> anyhow::Result<()> {
        if message.first() == Some(&b'7') {
            let Owner::Child { mirror } = &mut self.owner else {
                return Ok(());
            };
            if message.starts_with(b"702")
                && let Some(mirror) = mirror
            {
                mirror.held_end = Some(message);
                return Ok(());
            }
            if is_terminal_event(&message) {
                // Stopped or paused locally: the mirrored copy has to stop as well.
                if let Some(Mirror { id, .. }) = mirror.take() {
                    self.send_to_daemon(SpeechCommand::Stop { id });
                }
                self.owner = Owner::Nobody;
            }
            return self.reply(&message).await;
        }

        match self.pending_replies.pop_front() {
            Some(OnReply::Relay) => self.reply(&message).await,
            Some(OnReply::Discard) => {
                if message.first() != Some(&b'2') {
                    log!(
                        "real module refused a command: {}",
                        String::from_utf8_lossy(&message).trim()
                    );
                }
                Ok(())
            }
            Some(OnReply::SpeakResult) => {
                if message.first() != Some(&b'2') {
                    self.drop_mirror().await?;
                    self.owner = Owner::Nobody;
                }
                self.reply(&message).await
            }
            None => {
                log!(
                    "unexpected reply from real module: {}",
                    String::from_utf8_lossy(&message).trim()
                );
                Ok(())
            }
        }
    }

    // --- daemon side ---

    fn on_daemon_connected(&mut self, sender: mpsc::UnboundedSender<ModuleMessage>) {
        log!("connected to the daemon");
        self.daemon = Some(sender);
        self.sent_settings = None;
        self.send_settings();
    }

    fn send_settings(&mut self) {
        if self.daemon.is_some() && self.sent_settings.as_ref() != Some(&self.settings) {
            self.sent_settings = Some(self.settings.clone());
            self.send_to_daemon(SpeechCommand::Settings(self.settings.clone()));
        }
    }

    async fn on_daemon_disconnected(&mut self) -> anyhow::Result<()> {
        log!("disconnected from the daemon");
        self.daemon = None;
        self.mode = OutputMode::Local;
        self.apply_mode().await
    }

    async fn on_daemon_message(&mut self, message: DaemonMessage) -> anyhow::Result<()> {
        match message {
            DaemonMessage::SetMode(mode) => {
                log!("output mode: {mode:?}");
                self.mode = mode;
                self.apply_mode().await
            }
            DaemonMessage::Event(event) => self.on_remote_event(event).await,
        }
    }

    /// Stops an utterance whose owner no longer matches the output mode. New utterances
    /// automatically go to the right place.
    async fn apply_mode(&mut self) -> anyhow::Result<()> {
        let mode = self.effective_mode();
        if mode != OutputMode::Mirror {
            self.drop_mirror().await?;
        }
        let remote = mode == OutputMode::Remote;
        match self.owner {
            Owner::Child { .. } if remote => self.send_to_child(b"STOP\n").await,
            Owner::Remote { id, .. } if !remote => {
                self.send_to_daemon(SpeechCommand::Stop { id });
                self.owner = Owner::Nobody;
                self.reply(b"703 STOP\n").await
            }
            _ => Ok(()),
        }
    }

    async fn on_remote_event(&mut self, event: SpeechEvent) -> anyhow::Result<()> {
        if let Owner::Child { mirror } = &mut self.owner {
            // The mirrored copy finished: release the real module's `END` if it is waiting,
            // otherwise let it through when it comes.
            if let (SpeechEvent::End { id } | SpeechEvent::Paused { id, .. }, Some(copy)) =
                (event, &*mirror)
                && id == copy.id
                && let Some(end) = mirror.take().and_then(|copy| copy.held_end)
            {
                self.owner = Owner::Nobody;
                self.reply(&end).await?;
            }
            return Ok(());
        }
        let Owner::Remote { id, marks } = &self.owner else {
            return Ok(());
        };
        let current = *id;
        match event {
            SpeechEvent::Mark { id, index } if id == current => {
                if let Some(name) = marks.get(index as usize) {
                    let message = format!("700-{name}\n700 INDEX MARK\n");
                    self.reply(message.as_bytes()).await?;
                }
                Ok(())
            }
            SpeechEvent::End { id } if id == current => {
                self.owner = Owner::Nobody;
                self.reply(b"702 END\n").await
            }
            SpeechEvent::Paused { id, .. } if id == current => {
                self.owner = Owner::Nobody;
                self.reply(b"704 PAUSE\n").await
            }
            _ => Ok(()),
        }
    }
}

fn is_terminal_event(message: &[u8]) -> bool {
    message.starts_with(b"702") || message.starts_with(b"703") || message.starts_with(b"704")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Harness {
        proxy: Proxy<Vec<u8>, Vec<u8>>,
        daemon: Option<mpsc::UnboundedReceiver<ModuleMessage>>,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                proxy: Proxy::new(Vec::new(), Vec::new()),
                daemon: None,
            }
        }

        async fn connect(&mut self, mode: OutputMode) {
            let (tx, rx) = mpsc::unbounded_channel();
            self.proxy.on_daemon_connected(tx);
            self.proxy
                .on_daemon_message(DaemonMessage::SetMode(mode))
                .await
                .unwrap();
            self.daemon = Some(rx);
        }

        /// Feeds speech-dispatcher's side, line by line.
        async fn upstream(&mut self, text: &str) {
            for line in text.split_inclusive('\n') {
                self.proxy
                    .on_upstream_line(line.as_bytes().to_vec())
                    .await
                    .unwrap();
            }
        }

        /// Feeds one message from the real module.
        async fn child(&mut self, message: &str) {
            self.proxy
                .on_child_message(message.as_bytes().to_vec())
                .await
                .unwrap();
        }

        async fn event(&mut self, event: SpeechEvent) {
            self.proxy
                .on_daemon_message(DaemonMessage::Event(event))
                .await
                .unwrap();
        }

        /// What was written to speech-dispatcher since the last call.
        fn sent_upstream(&mut self) -> String {
            String::from_utf8(std::mem::take(&mut self.proxy.stdout)).unwrap()
        }

        /// What was written to the real module since the last call.
        fn sent_child(&mut self) -> String {
            String::from_utf8(std::mem::take(&mut self.proxy.child)).unwrap()
        }

        /// Speech commands sent to the daemon since the last call, settings excluded.
        fn sent_daemon(&mut self) -> Vec<SpeechCommand> {
            let mut commands = Vec::new();
            if let Some(rx) = &mut self.daemon {
                while let Ok(message) = rx.try_recv() {
                    match message {
                        ModuleMessage::Speech(SpeechCommand::Settings(_)) => {}
                        ModuleMessage::Speech(command) => commands.push(command),
                        ModuleMessage::Hello { .. } => {}
                    }
                }
            }
            commands
        }

        /// Sends a text message and returns what went upstream after the `202`.
        async fn speak(&mut self, ssml: &str) -> String {
            self.upstream("SPEAK\n").await;
            assert_eq!(self.sent_upstream(), "202 OK RECEIVING MESSAGE\n");
            self.upstream(&format!("{ssml}\n.\n")).await;
            self.sent_upstream()
        }

        /// Lets the real module accept the speech command it was sent.
        async fn child_accepts(&mut self) {
            self.child("202 OK RECEIVING MESSAGE\n").await;
            self.child("200 OK SPEAKING\n").await;
            self.child("701 BEGIN\n").await;
            assert_eq!(self.sent_upstream(), "200 OK SPEAKING\n701 BEGIN\n");
        }
    }

    const HELLO: &str = "<speak><mark name=\"0:5\"/>Hello <mark name=\"6:11\"/>world</speak>";

    #[tokio::test]
    async fn local_speech_goes_through_the_real_module() {
        let mut h = Harness::new();
        assert_eq!(h.speak(HELLO).await, "");
        assert_eq!(h.sent_child(), format!("SPEAK\n{HELLO}\n.\n"));
        h.child_accepts().await;
        h.child("700-0:5\n700 INDEX MARK\n").await;
        h.child("702 END\n").await;
        assert_eq!(h.sent_upstream(), "700-0:5\n700 INDEX MARK\n702 END\n");
    }

    #[tokio::test]
    async fn configuration_is_relayed_and_recorded() {
        let mut h = Harness::new();
        h.upstream("SET\n").await;
        assert_eq!(h.sent_child(), "SET\n");
        h.child("203 OK RECEIVING SETTINGS\n").await;
        assert_eq!(h.sent_upstream(), "203 OK RECEIVING SETTINGS\n");
        h.upstream("rate=20\nlanguage=sk\n.\n").await;
        assert_eq!(h.sent_child(), "rate=20\nlanguage=sk\n.\n");
        h.child("203 OK SETTINGS RECEIVED\n").await;
        assert_eq!(h.sent_upstream(), "203 OK SETTINGS RECEIVED\n");
        assert_eq!(h.proxy.settings.rate, 20);
        assert_eq!(h.proxy.settings.language.as_deref(), Some("sk"));

        h.upstream("LIST VOICES\n").await;
        assert_eq!(h.sent_child(), "LIST VOICES\n");
        h.child("200-a\ten\tnone\n200 OK VOICE LIST SENT\n").await;
        assert_eq!(
            h.sent_upstream(),
            "200-a\ten\tnone\n200 OK VOICE LIST SENT\n"
        );
    }

    #[tokio::test]
    async fn remote_speech_is_driven_by_the_controller() {
        let mut h = Harness::new();
        h.connect(OutputMode::Remote).await;
        assert_eq!(h.speak(HELLO).await, "200 OK SPEAKING\n701 BEGIN\n");
        assert_eq!(h.sent_child(), "");

        let utterance = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.clone(),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(utterance.text, "Hello world");
        assert_eq!(
            utterance.marks,
            vec![
                Mark {
                    offset: 0,
                    index: 0
                },
                Mark {
                    offset: 6,
                    index: 1
                }
            ]
        );

        let id = utterance.id;
        h.event(SpeechEvent::Mark { id, index: 1 }).await;
        h.event(SpeechEvent::End { id: id + 1 }).await; // stale, ignored
        h.event(SpeechEvent::End { id }).await;
        assert_eq!(h.sent_upstream(), "700-6:11\n700 INDEX MARK\n702 END\n");
    }

    #[tokio::test]
    async fn remote_stop_is_reported_immediately() {
        let mut h = Harness::new();
        h.connect(OutputMode::Remote).await;
        h.speak(HELLO).await;
        let id = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.id,
            other => panic!("unexpected {other:?}"),
        };
        h.upstream("STOP\n").await;
        assert_eq!(h.sent_upstream(), "703 STOP\n");
        assert_eq!(h.sent_daemon(), vec![SpeechCommand::Stop { id }]);
        // The controller's late end must not produce a second terminal event.
        h.event(SpeechEvent::End { id }).await;
        assert_eq!(h.sent_upstream(), "");
    }

    #[tokio::test]
    async fn losing_the_daemon_ends_remote_speech_and_falls_back_to_local() {
        let mut h = Harness::new();
        h.connect(OutputMode::Remote).await;
        h.speak(HELLO).await;
        h.proxy.on_daemon_disconnected().await.unwrap();
        assert_eq!(h.sent_upstream(), "703 STOP\n");

        h.speak(HELLO).await;
        assert_eq!(h.sent_child(), format!("SPEAK\n{HELLO}\n.\n"));
    }

    #[tokio::test]
    async fn switching_to_remote_stops_local_speech() {
        let mut h = Harness::new();
        h.speak(HELLO).await;
        h.sent_child();
        h.child_accepts().await;
        h.connect(OutputMode::Remote).await;
        assert_eq!(h.sent_child(), "STOP\n");
        h.child("703 STOP\n").await;
        assert_eq!(h.sent_upstream(), "703 STOP\n");
    }

    #[tokio::test]
    async fn mirror_waits_for_both_to_finish() {
        let mut h = Harness::new();
        h.connect(OutputMode::Mirror).await;
        h.speak(HELLO).await;
        assert_eq!(h.sent_child(), format!("SPEAK\n{HELLO}\n.\n"));
        let id = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.id,
            other => panic!("unexpected {other:?}"),
        };
        h.child_accepts().await;

        // Controller marks are ignored; the real module drives events.
        h.event(SpeechEvent::Mark { id, index: 0 }).await;
        h.child("702 END\n").await;
        assert_eq!(h.sent_upstream(), "");
        h.event(SpeechEvent::End { id }).await;
        assert_eq!(h.sent_upstream(), "702 END\n");
    }

    #[tokio::test]
    async fn mirror_controller_finishing_first_lets_end_through() {
        let mut h = Harness::new();
        h.connect(OutputMode::Mirror).await;
        h.speak(HELLO).await;
        let id = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.id,
            other => panic!("unexpected {other:?}"),
        };
        h.child_accepts().await;
        h.event(SpeechEvent::End { id }).await;
        h.child("702 END\n").await;
        assert_eq!(h.sent_upstream(), "702 END\n");
    }

    #[tokio::test]
    async fn mirror_stop_while_end_is_held() {
        let mut h = Harness::new();
        h.connect(OutputMode::Mirror).await;
        h.speak(HELLO).await;
        h.sent_child();
        let id = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.id,
            other => panic!("unexpected {other:?}"),
        };
        h.child_accepts().await;
        h.child("702 END\n").await;

        h.upstream("STOP\n").await;
        assert_eq!(h.sent_upstream(), "702 END\n");
        assert_eq!(h.sent_daemon(), vec![SpeechCommand::Stop { id }]);
        assert_eq!(h.sent_child(), "");
    }

    #[tokio::test]
    async fn mirror_stop_while_speaking_locally() {
        let mut h = Harness::new();
        h.connect(OutputMode::Mirror).await;
        h.speak(HELLO).await;
        h.sent_child();
        let id = match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => utterance.id,
            other => panic!("unexpected {other:?}"),
        };
        h.child_accepts().await;

        h.upstream("STOP\n").await;
        assert_eq!(h.sent_child(), "STOP\n");
        assert_eq!(h.sent_daemon(), vec![SpeechCommand::Stop { id }]);
        h.child("703 STOP\n").await;
        assert_eq!(h.sent_upstream(), "703 STOP\n");
    }

    #[tokio::test]
    async fn dot_escaped_lines_are_unescaped_for_the_controller() {
        let mut h = Harness::new();
        h.connect(OutputMode::Remote).await;
        h.speak("<speak>a\n..b\n.</speak>").await;
        match h.sent_daemon().as_slice() {
            [SpeechCommand::Speak(utterance)] => assert_eq!(utterance.text, "a\n.b\n"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
