//! Captures what applications play, through PipeWire (see `docs/ARCHITECTURE.md`, §5.6).
//!
//! A passive capture stream is created without being connected anywhere. The daemon then
//! links the output ports of every application playback stream to it, next to the
//! stream's own link to its sink. Nothing is rerouted, and the links belong to this
//! connection: they go away with it, even if the daemon crashes.
//!
//! The fallback, [`CaptureKind::Monitor`], captures the default sink's monitor instead.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{SyncSender, TrySendError};

use anyhow::Context;
use pipewire as pw;
use pw::link::Link;
use pw::node::{Node, NodeChangeMask, NodeListener};
use pw::properties::properties;
use pw::registry::GlobalObject;
use pw::spa;
use pw::spa::utils::dict::DictRef;
use pw::types::ObjectType;

use super::RATE;

/// Name of the capture node, also used to find it in the registry. The process id keeps
/// two daemons (a test one next to the real one) from taking each other's node.
fn node_name() -> String {
    format!("rconnect-audio-{}", std::process::id())
}

/// Where the audio comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    /// Tap each application's stream.
    Streams,
    /// Capture the default sink's monitor. It contains local speech too.
    Monitor,
}

/// A running capture; dropping it stops the capture and removes its links.
pub struct Capture {
    stop: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing. Interleaved stereo samples at [`RATE`] go to `pcm`, in chunks
    /// of whatever size PipeWire delivers; chunks are dropped when `pcm` is full.
    pub fn start(kind: CaptureKind, pcm: SyncSender<Vec<f32>>) -> anyhow::Result<Capture> {
        let (stop, stop_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("audio capture".into())
            .spawn(move || {
                let ready = ready_tx.clone();
                if let Err(e) = run(kind, pcm, stop_rx, move || {
                    let _ = ready.send(Ok(()));
                }) {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        ready_rx.recv().context("the capture thread stopped")??;
        Ok(Capture {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    kind: CaptureKind,
    pcm: SyncSender<Vec<f32>>,
    stop: pw::channel::Receiver<()>,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("cannot start PipeWire")?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("cannot connect to PipeWire")?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Accessibility",
        *pw::keys::NODE_NAME => node_name(),
        *pw::keys::NODE_DESCRIPTION => "RConnect audio for the phone",
        // Never keeps a sink running: it only listens while applications play.
        "node.passive" => "true",
        // 10 ms. The quantum is shared by the whole graph, so this is not lower.
        "node.latency" => "480/48000",
    };
    let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
    match kind {
        CaptureKind::Streams => props.insert("node.autoconnect", "false"),
        CaptureKind::Monitor => {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
            flags |= pw::stream::StreamFlags::AUTOCONNECT;
        }
    }
    let stream = pw::stream::StreamBox::new(&core, &node_name(), props)?;
    let _listener = stream
        .add_local_listener_with_user_data(pcm)
        .process(|stream, pcm| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            let samples: Vec<f32> = bytes[..size.min(bytes.len())]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect();
            if let Err(TrySendError::Full(_)) = pcm.try_send(samples) {
                tracing::debug!("audio encoder is behind; dropping captured audio");
            }
        })
        .register()?;

    let format = format_pod(&[
        spa::sys::SPA_AUDIO_CHANNEL_FL,
        spa::sys::SPA_AUDIO_CHANNEL_FR,
    ])?;
    let mut params = [spa::pod::Pod::from_bytes(&format).context("invalid audio format")?];
    stream.connect(spa::utils::Direction::Input, None, flags, &mut params)?;

    let _tap = match kind {
        CaptureKind::Streams => Some(Tap::start(&core)?),
        CaptureKind::Monitor => None,
    };
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    ready();
    mainloop.run();
    Ok(())
}

/// F32LE at 48 kHz with the given channel positions (`SPA_AUDIO_CHANNEL_*`).
pub(super) fn format_pod(channels: &[u32]) -> anyhow::Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(channels.len() as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[..channels.len()].copy_from_slice(channels);
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|e| anyhow::anyhow!("cannot describe the audio format: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// Links application streams to the capture node, following the registry.
struct Tap {
    _state: Rc<RefCell<TapState>>,
    _listener: pw::registry::Listener,
}

struct TapState {
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
    /// The capture node, once it shows up in the registry.
    own_node: Option<u32>,
    /// Playback streams, capture streams and sinks, bound to read their full properties:
    /// whether a playback stream is tapped depends on the others in its link group.
    nodes: HashMap<u32, BoundNode>,
    ports: HashMap<u32, PortInfo>,
    /// Our links, by (output port, input port).
    links: HashMap<(u32, u32), Link>,
}

struct BoundNode {
    _proxy: Node,
    _listener: NodeListener,
    class: NodeClass,
    /// Known once the node's properties arrive.
    facts: Option<NodeFacts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeClass {
    /// `Stream/Output/Audio`: may be tapped.
    Playback,
    /// `Stream/Input/Audio`: the capture side of a loopback, among others.
    Capture,
    /// `Audio/Sink`.
    Sink,
}

impl NodeClass {
    fn of(media_class: &str) -> Option<NodeClass> {
        match media_class {
            "Stream/Output/Audio" => Some(NodeClass::Playback),
            "Stream/Input/Audio" => Some(NodeClass::Capture),
            "Audio/Sink" => Some(NodeClass::Sink),
            _ => None,
        }
    }
}

/// What a node's full properties say.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeFacts {
    /// For playback streams: an application's own audio, apart from link groups.
    own_audio: bool,
    /// `node.link-group`, shared by the two sides of a loopback or filter.
    link_group: Option<String>,
    /// For capture streams: records what a sink plays.
    captures_playback: bool,
}

impl NodeFacts {
    fn of(props: &DictRef) -> NodeFacts {
        NodeFacts {
            own_audio: !is_speech_dispatcher(props)
                && !is_rconnect(props)
                && props.get("stream.monitor") != Some("true"),
            link_group: props.get("node.link-group").map(str::to_owned),
            captures_playback: props.get(*pw::keys::STREAM_CAPTURE_SINK) == Some("true")
                || props
                    .get("target.object")
                    .is_some_and(|target| target.ends_with(".monitor")),
        }
    }
}

struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
}

impl Tap {
    fn start(core: &pw::core::CoreRc) -> anyhow::Result<Tap> {
        let registry = core.get_registry_rc()?;
        let state = Rc::new(RefCell::new(TapState {
            core: core.clone(),
            registry: registry.clone(),
            own_node: None,
            nodes: HashMap::new(),
            ports: HashMap::new(),
            links: HashMap::new(),
        }));
        let listener = registry
            .add_listener_local()
            .global({
                let state = Rc::downgrade(&state);
                move |global| TapState::on_global(&state, global)
            })
            .global_remove({
                let state = Rc::downgrade(&state);
                move |id| {
                    if let Some(state) = state.upgrade() {
                        state.borrow_mut().on_global_remove(id);
                    }
                }
            })
            .register();
        Ok(Tap {
            _state: state,
            _listener: listener,
        })
    }
}

impl TapState {
    fn on_global(state: &Weak<RefCell<TapState>>, global: &GlobalObject<&DictRef>) {
        let Some(this) = state.upgrade() else {
            return;
        };
        let Some(props) = global.props else {
            return;
        };
        match global.type_ {
            ObjectType::Node => {
                let name = props.get(*pw::keys::NODE_NAME).unwrap_or_default();
                if name == node_name() {
                    this.borrow_mut().own_node = Some(global.id);
                    this.borrow_mut().reconcile();
                    return;
                }
                let Some(class) = props.get(*pw::keys::MEDIA_CLASS).and_then(NodeClass::of) else {
                    return;
                };
                if class == NodeClass::Playback && is_speech_dispatcher(props) {
                    return;
                }
                // Link groups show only in the node's full properties.
                let proxy: Node = match this.borrow().registry.bind(global) {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        tracing::debug!("cannot inspect stream {}: {e}", global.id);
                        return;
                    }
                };
                let id = global.id;
                let listener = proxy
                    .add_listener_local()
                    .info({
                        let state = state.clone();
                        move |info| {
                            // Other changes, such as the node's state, come without props.
                            if !info.change_mask().contains(NodeChangeMask::PROPS) {
                                return;
                            }
                            let Some(state) = state.upgrade() else {
                                return;
                            };
                            let Some(props) = info.props() else {
                                return;
                            };
                            let facts = NodeFacts::of(props);
                            let mut state = state.borrow_mut();
                            if let Some(node) = state.nodes.get_mut(&id)
                                && node.facts.as_ref() != Some(&facts)
                            {
                                tracing::debug!("node {id}: {:?} {facts:?}", node.class);
                                node.facts = Some(facts);
                                state.reconcile();
                            }
                        }
                    })
                    .register();
                this.borrow_mut().nodes.insert(
                    id,
                    BoundNode {
                        _proxy: proxy,
                        _listener: listener,
                        class,
                        facts: None,
                    },
                );
            }
            ObjectType::Port => {
                let Some(node) = props.get("node.id").and_then(|id| id.parse().ok()) else {
                    return;
                };
                if props.get("port.monitor") == Some("true") {
                    return;
                }
                let port = PortInfo {
                    node,
                    output: props.get("port.direction") == Some("out"),
                    channel: props.get("audio.channel").unwrap_or("MONO").to_owned(),
                };
                let mut this = this.borrow_mut();
                this.ports.insert(global.id, port);
                this.reconcile();
            }
            _ => {}
        }
    }

    fn on_global_remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        if self.own_node == Some(id) {
            self.own_node = None;
        }
        if self.ports.remove(&id).is_some() {
            // The server removed the links with the port; drop our proxies too.
            self.links
                .retain(|&(output, input), _| output != id && input != id);
        }
        self.reconcile();
    }

    /// Whether a node is a playback stream carrying new audio. Loopbacks and filters that
    /// pass on playback, whose link group has a sink or captures a sink's monitor, repeat
    /// audio the applications behind them already produced, so they are not tapped. A
    /// loopback from a microphone is new audio, and is.
    fn is_tapped(&self, id: u32) -> bool {
        let Some(node) = self.nodes.get(&id) else {
            return false;
        };
        let Some(facts) = &node.facts else {
            return false;
        };
        if node.class != NodeClass::Playback || !facts.own_audio {
            return false;
        }
        let Some(group) = &facts.link_group else {
            return true;
        };
        !self.nodes.values().any(|other| {
            other.facts.as_ref().is_some_and(|other_facts| {
                other_facts.link_group.as_ref() == Some(group)
                    && (other.class == NodeClass::Sink
                        || (other.class == NodeClass::Capture && other_facts.captures_playback))
            })
        })
    }

    /// Creates the links that should exist and drops those that should not.
    fn reconcile(&mut self) {
        let Some(own_node) = self.own_node else {
            return;
        };
        let inputs: Vec<(u32, &str)> = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == own_node && !port.output)
            .map(|(&id, port)| (id, port.channel.as_str()))
            .collect();
        let mut wanted = HashSet::new();
        for (&output, port) in &self.ports {
            if !port.output || !self.is_tapped(port.node) {
                continue;
            }
            for &(input, channel) in &inputs {
                if feeds(&port.channel, channel) {
                    wanted.insert((output, input));
                }
            }
        }

        self.links.retain(|key, _| wanted.contains(key));
        for (output, input) in wanted {
            if self.links.contains_key(&(output, input)) {
                continue;
            }
            let output_node = self.ports[&output].node;
            let props = properties! {
                "link.output.node" => output_node.to_string(),
                "link.output.port" => output.to_string(),
                "link.input.node" => own_node.to_string(),
                "link.input.port" => input.to_string(),
                // Never keeps the graph running on its own.
                "link.passive" => "true",
            };
            match self.core.create_object::<Link>("link-factory", &props) {
                Ok(link) => {
                    self.links.insert((output, input), link);
                }
                Err(e) => tracing::warn!("cannot tap stream {output_node}: {e}"),
            }
        }
    }
}

/// speech-dispatcher's streams are named after the output module, such as
/// `speech-dispatcher-espeak-ng`, and carry no application name.
fn is_speech_dispatcher(props: &DictRef) -> bool {
    props
        .get(*pw::keys::NODE_NAME)
        .is_some_and(|name| name.starts_with("speech-dispatcher"))
        || props
            .get(*pw::keys::APP_NAME)
            .is_some_and(|name| name.starts_with("speech-dispatcher"))
}

/// RConnect's own processes, such as the dev controller playing the stream on this very
/// computer, which would otherwise hear itself.
fn is_rconnect(props: &DictRef) -> bool {
    [
        *pw::keys::NODE_NAME,
        *pw::keys::APP_NAME,
        *pw::keys::APP_PROCESS_BINARY,
    ]
    .iter()
    .filter_map(|key| props.get(key))
    .any(|value| value.contains("rconnect"))
}

/// Whether an output channel is mixed into one of the capture's two input channels.
fn feeds(output: &str, input: &str) -> bool {
    if output.starts_with("LFE") {
        return false;
    }
    match input {
        "FL" => !output.ends_with('R'),
        "FR" => !output.ends_with('L'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_fold_to_stereo() {
        assert!(feeds("FL", "FL") && !feeds("FL", "FR"));
        assert!(feeds("FR", "FR") && !feeds("FR", "FL"));
        assert!(feeds("MONO", "FL") && feeds("MONO", "FR"));
        assert!(feeds("FC", "FL") && feeds("FC", "FR"));
        assert!(feeds("RL", "FL") && feeds("SR", "FR"));
        assert!(!feeds("LFE", "FL") && !feeds("LFE", "FR"));
    }
}
