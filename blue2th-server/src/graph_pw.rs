// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`PipeWireGraph`]: the [`Graph`] that drives PipeWire natively, from a
//! `pw_main_loop` running on a thread of its own (#79).
//!
//! RED phase: every item below is a typed stub, returning a wrong-but-typed
//! value so the tests at the bottom of this file compile and fail.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use crate::audio::{AudioError, CombineBranch};
use crate::graph::{Graph, LoadedBranch};

/// How long the handle waits for the loop thread to answer one command.
pub(crate) const GRAPH_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the loop thread sends the answer to one command.
pub(crate) type Reply<T> = mpsc::Sender<Result<T, AudioError>>;

/// One request from the handle to the loop thread, carrying its reply channel.
pub(crate) enum Command {
    Sinks {
        reply: Reply<Vec<String>>,
    },
    Branches {
        sink_name: String,
        reply: Reply<Vec<LoadedBranch>>,
    },
    CreateCombinedSink {
        sink_name: String,
        reply: Reply<()>,
    },
    LoadBranch {
        sink_name: String,
        real_sink: String,
        latency_ms: u32,
        reply: Reply<()>,
    },
    UnloadBranch {
        id: u32,
        reply: Reply<()>,
    },
    Teardown {
        sink_name: String,
        reply: Reply<()>,
    },
    SetDefaultSink {
        sink: String,
        reply: Reply<()>,
    },
    SinkVolume {
        sink: String,
        reply: mpsc::Sender<Option<f32>>,
    },
    SetSinkVolume {
        sink: String,
        level: f32,
        reply: Reply<()>,
    },
}

/// The handle's end of the channel into the loop thread.
pub(crate) trait LoopSender: Send {
    /// Hand `command` to the loop thread; give it back when the thread is gone.
    fn send(&self, command: Command) -> Result<(), Command>;
}

/// Starts a loop thread and returns the sender into it.
pub(crate) type SpawnLoop = Box<dyn FnMut() -> Box<dyn LoopSender> + Send>;

/// The handle the router owns: a sender into the loop thread, and the means to
/// start a new thread when the previous one has died.
pub struct PipeWireGraph {
    spawn_loop: SpawnLoop,
    sender: Option<Box<dyn LoopSender>>,
}

impl PipeWireGraph {
    /// A graph over the PipeWire daemon of the current session.
    pub fn spawn() -> Self {
        Self::with_loop(Box::new(|| Box::new(DeadLoop) as Box<dyn LoopSender>))
    }

    /// A graph whose loop threads are started by `spawn_loop`.
    pub(crate) fn with_loop(spawn_loop: SpawnLoop) -> Self {
        Self {
            spawn_loop,
            sender: None,
        }
    }
}

/// RED stub: a loop that is never there.
struct DeadLoop;

impl LoopSender for DeadLoop {
    fn send(&self, command: Command) -> Result<(), Command> {
        Err(command)
    }
}

impl Graph for PipeWireGraph {
    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        Ok(Vec::new())
    }

    fn branches(&mut self, _sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        Ok(Vec::new())
    }

    fn create_combined_sink(&mut self, _sink_name: &str) -> Result<(), AudioError> {
        Ok(())
    }

    fn load_branch(
        &mut self,
        _sink_name: &str,
        _real_sink: &str,
        _latency_ms: u32,
    ) -> Result<(), AudioError> {
        Ok(())
    }

    fn unload_branch(&mut self, _id: u32) -> Result<(), AudioError> {
        Ok(())
    }

    fn teardown(&mut self, _sink_name: &str) -> Result<(), AudioError> {
        Ok(())
    }

    fn set_default_sink(&mut self, _sink: &str) -> Result<(), AudioError> {
        Ok(())
    }

    fn sink_volume(&mut self, _sink: &str) -> Option<f32> {
        Some(0.0)
    }

    fn set_sink_volume(&mut self, _sink: &str, _level: f32) -> Result<(), AudioError> {
        Ok(())
    }
}

/// One node of the registry mirror: the properties of its global.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NodeEntry {
    pub(crate) props: BTreeMap<String, String>,
}

/// One link of the registry mirror: the node it leaves and the node it enters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkEntry {
    pub(crate) output_node: u32,
    pub(crate) input_node: u32,
}

/// One device of the registry mirror: the properties of its global.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeviceEntry {
    pub(crate) props: BTreeMap<String, String>,
}

/// The registry as the listener callbacks last reported it, keyed by global id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Mirror {
    pub(crate) nodes: BTreeMap<u32, NodeEntry>,
    pub(crate) links: BTreeMap<u32, LinkEntry>,
    pub(crate) devices: BTreeMap<u32, DeviceEntry>,
}

impl Mirror {
    /// Whether the mirror knows no global at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.links.is_empty() && self.devices.is_empty()
    }
}

/// Where a sink's volume lives: the device global and the `Route` device index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteTarget {
    pub(crate) device_id: u32,
    pub(crate) route_device: i32,
}

/// The `libpipewire-module-loopback` argument string for one branch.
pub(crate) fn loopback_module_args(
    _sink_name: &str,
    _real_sink: &str,
    _latency_ms: u32,
    _id: u32,
) -> Result<String, AudioError> {
    Ok(String::new())
}

/// The properties the combined sink's `adapter` node is created with.
pub(crate) fn combined_sink_props(_sink_name: &str) -> Vec<(String, String)> {
    Vec::new()
}

/// The node names of the mirror's `Audio/Sink` nodes.
pub(crate) fn sink_names(_mirror: &Mirror) -> Vec<String> {
    Vec::new()
}

/// Whether the branch whose playback node is `out_node` feeds `real_sink`.
pub(crate) fn branch_liveness(_mirror: &Mirror, _out_node: &str, _real_sink: &str) -> bool {
    false
}

/// The globals a teardown of `sink_name` destroys whoever owns them.
pub(crate) fn foreign_combined_globals(_mirror: &Mirror, _sink_name: &str) -> Vec<u32> {
    Vec::new()
}

/// The value written to `default.configured.audio.sink` to make `sink` default.
pub(crate) fn default_sink_metadata_value(_sink: &str) -> String {
    String::new()
}

/// The device and `Route` index carrying `sink`'s volume.
pub(crate) fn route_target(_mirror: &Mirror, _sink: &str) -> Option<RouteTarget> {
    None
}

/// The volume fraction a device `Route`'s `channelVolumes` stands for.
pub(crate) fn volume_fraction_from_route(_channel_volumes: &[f32]) -> Option<f32> {
    None
}

/// The `channelVolumes` a `Route` is written with to set the volume to `level`.
pub(crate) fn route_channel_volumes(_level: f32, _channels: usize) -> Vec<f32> {
    Vec::new()
}

/// Opens a connection to the daemon from the loop thread.
pub(crate) trait Connector {
    /// What the loop thread holds while connected.
    type Connection;
    /// One loopback module loaded into the server process.
    type Module;
    /// The proxy owning the combined null sink.
    type NullSink;
    /// Connect to the daemon; an `Err` is "no daemon".
    fn connect(&mut self) -> Result<Self::Connection, AudioError>;
}

/// The loop thread's state: the connection, the mirror, and what the graph
/// itself created.
pub(crate) struct LoopState<C: Connector> {
    connector: C,
    connection: Option<C::Connection>,
    mirror: Mirror,
    modules: BTreeMap<u32, (String, CombineBranch, C::Module)>,
    null_sinks: BTreeMap<String, C::NullSink>,
    next_module_id: u32,
}

impl<C: Connector> LoopState<C> {
    /// A loop that has not connected yet.
    pub(crate) fn new(connector: C) -> Self {
        Self {
            connector,
            connection: None,
            mirror: Mirror::default(),
            modules: BTreeMap::new(),
            null_sinks: BTreeMap::new(),
            next_module_id: 0,
        }
    }

    /// The live connection, connecting first when there is none.
    pub(crate) fn connection(&mut self) -> Result<&mut C::Connection, AudioError> {
        Err(AudioError::PipeWire("RED stub".to_string()))
    }

    /// Whether a connection is currently held.
    pub(crate) fn is_connected(&self) -> bool {
        false
    }

    /// The registry mirror.
    pub(crate) fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// The registry mirror, for the listener callbacks.
    pub(crate) fn mirror_mut(&mut self) -> &mut Mirror {
        &mut self.mirror
    }

    /// Keep a loaded module and return the graph's own id for it.
    pub(crate) fn add_module(
        &mut self,
        _sink_name: &str,
        _branch: CombineBranch,
        _module: C::Module,
    ) -> u32 {
        0
    }

    /// The modules the graph loaded for `sink_name`, with their ids.
    pub(crate) fn modules_for(&self, _sink_name: &str) -> Vec<(u32, CombineBranch)> {
        Vec::new()
    }

    /// Forget the module `id` and hand it back for destruction.
    pub(crate) fn take_module(&mut self, _id: u32) -> Option<C::Module> {
        None
    }

    /// Keep the proxy owning the combined sink `sink_name`.
    pub(crate) fn set_null_sink(&mut self, _sink_name: &str, _proxy: C::NullSink) {}

    /// Whether the graph itself owns the combined sink `sink_name`.
    pub(crate) fn owns_null_sink(&self, _sink_name: &str) -> bool {
        false
    }

    /// The core `error`/disconnect callback: the connection is gone.
    pub(crate) fn on_disconnect(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const COMBINED: &str = "blue2th_combined";
    const SPEAKER: &str = "bluez_output.80_99_E7_63_50_29.1";

    // ─── SPA-JSON reading, for the module argument tests ─────────────────────

    /// A value of the SPA-JSON dialect the module arguments are written in.
    #[derive(Debug, Clone, PartialEq)]
    enum Spa {
        Word(String),
        Object(BTreeMap<String, Spa>),
    }

    /// Split SPA-JSON into braces and words. `=`, `:`, `,` and whitespace all
    /// separate; a quoted word loses its quotes.
    fn spa_tokens(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for c in text.chars() {
            if quoted {
                if c == '"' {
                    quoted = false;
                    tokens.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                '{' | '}' => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                    tokens.push(c.to_string());
                },
                c if c.is_whitespace() || c == '=' || c == ':' || c == ',' => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                },
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    /// Read the key/value pairs of one object, stopping at its closing brace.
    fn spa_object(tokens: &mut std::vec::IntoIter<String>) -> BTreeMap<String, Spa> {
        let mut object = BTreeMap::new();
        while let Some(key) = tokens.next() {
            if key == "}" {
                break;
            }
            match tokens.next() {
                Some(v) if v == "{" => {
                    object.insert(key, Spa::Object(spa_object(tokens)));
                },
                Some(v) => {
                    object.insert(key, Spa::Word(v));
                },
                None => break,
            }
        }
        object
    }

    /// Parse module arguments: one object, its outer braces optional.
    fn parse_args(text: &str) -> BTreeMap<String, Spa> {
        let mut tokens = spa_tokens(text);
        if tokens.first().map(String::as_str) == Some("{") {
            tokens.remove(0);
        }
        spa_object(&mut tokens.into_iter())
    }

    fn word(object: &BTreeMap<String, Spa>, key: &str) -> Option<String> {
        match object.get(key) {
            Some(Spa::Word(w)) => Some(w.clone()),
            _ => None,
        }
    }

    fn section(args: &BTreeMap<String, Spa>, name: &str) -> BTreeMap<String, Spa> {
        match args.get(name) {
            Some(Spa::Object(o)) => o.clone(),
            _ => BTreeMap::new(),
        }
    }

    /// A property one stream carries: from its own props, else from the module
    /// level, which the loopback module copies into both streams.
    fn stream_prop(args: &BTreeMap<String, Spa>, stream: &str, key: &str) -> Option<String> {
        word(&section(args, stream), key).or_else(|| word(args, key))
    }

    // ─── Mirror fixtures ─────────────────────────────────────────────────────

    fn node(props: &[(&str, &str)]) -> NodeEntry {
        NodeEntry {
            props: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn mirror_of(nodes: &[(u32, NodeEntry)], links: &[(u32, u32, u32)]) -> Mirror {
        Mirror {
            nodes: nodes.iter().cloned().collect(),
            links: links
                .iter()
                .map(|&(id, output_node, input_node)| {
                    (
                        id,
                        LinkEntry {
                            output_node,
                            input_node,
                        },
                    )
                })
                .collect(),
            devices: BTreeMap::new(),
        }
    }

    fn sorted(mut ids: Vec<u32>) -> Vec<u32> {
        ids.sort_unstable();
        ids
    }

    // ─── loopback_module_args ────────────────────────────────────────────────

    // Criterion: `target.delay.sec` is `latency_ms / 1000` with three decimals,
    // as `pipewire-pulse` derived it from `latency_msec`.
    #[test]
    fn test_loopback_module_args_carries_the_delay_in_seconds() {
        for (latency_ms, expected) in [(50, "0.050"), (170, "0.170"), (1250, "1.250"), (0, "0.000")]
        {
            let args = parse_args(&loopback_module_args(COMBINED, SPEAKER, latency_ms, 3).unwrap());
            assert_eq!(
                word(&args, "target.delay.sec").as_deref(),
                Some(expected),
                "latency {latency_ms} ms"
            );
        }
    }

    // Criterion: both `target.object`s and both `node.dont-reconnect = true` are
    // present, so neither end is ever moved onto another node.
    #[test]
    fn test_loopback_module_args_pins_both_ends_with_dont_reconnect() {
        let args = parse_args(&loopback_module_args(COMBINED, SPEAKER, 50, 3).unwrap());

        assert_eq!(
            word(&section(&args, "capture.props"), "target.object").as_deref(),
            Some(COMBINED)
        );
        assert_eq!(
            word(&section(&args, "playback.props"), "target.object").as_deref(),
            Some(SPEAKER)
        );
        for stream in ["capture.props", "playback.props"] {
            assert_eq!(
                word(&section(&args, stream), "node.dont-reconnect").as_deref(),
                Some("true"),
                "{stream} must not reconnect"
            );
        }
    }

    // Criterion: `stream.capture.sink = true` on the capture side only — that is
    // what makes the capture stream read the combined sink's monitor.
    #[test]
    fn test_loopback_module_args_captures_the_sink_on_the_capture_side_only() {
        let args = parse_args(&loopback_module_args(COMBINED, SPEAKER, 50, 3).unwrap());

        assert_eq!(
            word(&section(&args, "capture.props"), "stream.capture.sink").as_deref(),
            Some("true")
        );
        assert_eq!(
            word(&section(&args, "playback.props"), "stream.capture.sink"),
            None,
            "the playback stream writes into a sink, it captures nothing"
        );
        assert_eq!(word(&args, "stream.capture.sink"), None);
    }

    // Criterion: both streams are named `blue2th_loop.<n>.in` / `.out` and grouped
    // by `node.group = blue2th_loop.<n>`: the `.out` name is what liveness reads.
    #[test]
    fn test_loopback_module_args_names_and_groups_both_streams_by_id() {
        let args = parse_args(&loopback_module_args(COMBINED, SPEAKER, 50, 7).unwrap());

        assert_eq!(
            word(&section(&args, "capture.props"), "node.name").as_deref(),
            Some("blue2th_loop.7.in")
        );
        assert_eq!(
            word(&section(&args, "playback.props"), "node.name").as_deref(),
            Some("blue2th_loop.7.out")
        );
        for stream in ["capture.props", "playback.props"] {
            assert_eq!(
                stream_prop(&args, stream, "node.group").as_deref(),
                Some("blue2th_loop.7"),
                "{stream} belongs to the branch's group"
            );
        }
    }

    // Criterion (non-nominal): an empty end is refused before anything reaches
    // the loop — an empty `target.object` lets the session manager pick any node.
    #[test]
    fn test_loopback_module_args_refuses_an_empty_end() {
        assert!(matches!(
            loopback_module_args("", SPEAKER, 50, 1),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            loopback_module_args(COMBINED, "", 50, 1),
            Err(AudioError::PipeWire(_))
        ));
        assert!(loopback_module_args(COMBINED, SPEAKER, 50, 1).is_ok());
    }

    // ─── combined_sink_props ─────────────────────────────────────────────────

    // Criterion: the combined sink is an `adapter` over `support.null-audio-sink`,
    // `media.class = Audio/Sink`, `audio.position = FL,FR`, named `sink_name`.
    #[test]
    fn test_combined_sink_props_describe_a_stereo_null_audio_sink() {
        let props: BTreeMap<String, String> = combined_sink_props(COMBINED).into_iter().collect();

        assert_eq!(
            props.get("factory.name").map(String::as_str),
            Some("support.null-audio-sink")
        );
        assert_eq!(props.get("node.name").map(String::as_str), Some(COMBINED));
        assert_eq!(
            props.get("media.class").map(String::as_str),
            Some("Audio/Sink")
        );
        assert_eq!(
            props.get("audio.position").map(String::as_str),
            Some("FL,FR")
        );
    }

    // ─── sink_names ──────────────────────────────────────────────────────────

    // Criterion: `sinks()` answers the node names with `media.class ==
    // "Audio/Sink"`, and nothing else — no stream, no source.
    #[test]
    fn test_sink_names_lists_audio_sinks_only() {
        let mirror = mirror_of(
            &[
                (
                    39,
                    node(&[
                        ("node.name", "alsa_output.pci-0000_00_1f.3.analog-stereo"),
                        ("media.class", "Audio/Sink"),
                    ]),
                ),
                (
                    40,
                    node(&[
                        ("node.name", "alsa_input.pci-0000_00_1f.3.analog-stereo"),
                        ("media.class", "Audio/Source"),
                    ]),
                ),
                (
                    57,
                    node(&[("node.name", SPEAKER), ("media.class", "Audio/Sink")]),
                ),
                (
                    61,
                    node(&[("node.name", COMBINED), ("media.class", "Audio/Sink")]),
                ),
                (
                    70,
                    node(&[
                        ("node.name", "blue2th_loop.1.out"),
                        ("media.class", "Stream/Output/Audio"),
                    ]),
                ),
                (
                    71,
                    node(&[
                        ("node.name", "v4l2_input.cam"),
                        ("media.class", "Video/Source"),
                    ]),
                ),
            ],
            &[],
        );

        assert_eq!(
            sink_names(&mirror),
            vec![
                "alsa_output.pci-0000_00_1f.3.analog-stereo".to_string(),
                SPEAKER.to_string(),
                COMBINED.to_string(),
            ]
        );
    }

    // Criterion (the empty value is a wildcard): a sink node naming nothing
    // contributes no name, so no empty name ever reaches the planning layer.
    #[test]
    fn test_sink_names_skips_a_sink_naming_no_node() {
        let mirror = mirror_of(
            &[
                (
                    39,
                    node(&[("node.name", ""), ("media.class", "Audio/Sink")]),
                ),
                (40, node(&[("media.class", "Audio/Sink")])),
                (
                    57,
                    node(&[("node.name", SPEAKER), ("media.class", "Audio/Sink")]),
                ),
            ],
            &[],
        );

        assert_eq!(sink_names(&mirror), vec![SPEAKER.to_string()]);
    }

    // ─── branch_liveness ─────────────────────────────────────────────────────

    /// The branch's `.out` node, the resolved sink, a second sink, and whatever
    /// links the test asks for.
    fn liveness_mirror(links: &[(u32, u32, u32)]) -> Mirror {
        mirror_of(
            &[
                (
                    57,
                    node(&[("node.name", SPEAKER), ("media.class", "Audio/Sink")]),
                ),
                (
                    58,
                    node(&[
                        ("node.name", "bluez_output.11_22_33_44_55_66.1"),
                        ("media.class", "Audio/Sink"),
                    ]),
                ),
                (
                    90,
                    node(&[
                        ("node.name", "blue2th_loop.3.out"),
                        ("media.class", "Stream/Output/Audio"),
                    ]),
                ),
            ],
            links,
        )
    }

    // Criterion: `Some(true)` when the branch's `.out` node has ≥ 1 link into the
    // resolved sink, `Some(false)` when unlinked or linked elsewhere — the #75
    // phantom, whose stream sat on no sink at all.
    #[test]
    fn test_branch_liveness_is_live_only_with_a_link_into_the_resolved_sink() {
        let linked = liveness_mirror(&[(200, 90, 57), (201, 90, 57)]);
        assert!(
            branch_liveness(&linked, "blue2th_loop.3.out", SPEAKER),
            "two links (FL, FR) into the resolved sink: live"
        );

        let one_link = liveness_mirror(&[(200, 90, 57)]);
        assert!(branch_liveness(&one_link, "blue2th_loop.3.out", SPEAKER));

        let unlinked = liveness_mirror(&[]);
        assert!(
            !branch_liveness(&unlinked, "blue2th_loop.3.out", SPEAKER),
            "no link at all: dead"
        );

        let elsewhere = liveness_mirror(&[(200, 90, 58)]);
        assert!(
            !branch_liveness(&elsewhere, "blue2th_loop.3.out", SPEAKER),
            "linked into another sink: dead for this one"
        );
    }

    // Criterion: a branch whose `.out` node is missing from the mirror is dead.
    #[test]
    fn test_branch_liveness_of_a_missing_out_node_is_dead() {
        let mirror = liveness_mirror(&[(200, 90, 57)]);

        assert!(!branch_liveness(&mirror, "blue2th_loop.4.out", SPEAKER));
    }

    // Criterion: liveness needs a named sink — an empty sink name names no node,
    // so it cannot vouch for a branch.
    #[test]
    fn test_branch_liveness_with_an_empty_name_is_dead() {
        let mirror = liveness_mirror(&[(200, 90, 57)]);

        assert!(branch_liveness(&mirror, "blue2th_loop.3.out", SPEAKER));
        assert!(!branch_liveness(&mirror, "blue2th_loop.3.out", ""));
        assert!(!branch_liveness(&mirror, "", SPEAKER));
    }

    // ─── foreign_combined_globals ────────────────────────────────────────────

    /// A graph left by a `pactl`-era server (#78): its null sink, its loopback
    /// pair, plus near misses on every axis — longer namesakes, another combined
    /// sink's pair, a loopback feeding *into* ours, and the ALSA devices.
    fn foreign_mirror() -> Mirror {
        mirror_of(
            &[
                (
                    39,
                    node(&[
                        ("node.name", "alsa_output.pci-0000_00_1f.3.analog-stereo"),
                        ("media.class", "Audio/Sink"),
                        ("device.api", "alsa"),
                        ("node.description", "blue2th_combined"),
                    ]),
                ),
                (
                    40,
                    node(&[
                        ("node.name", "alsa_input.pci-0000_00_1f.3.analog-stereo"),
                        ("media.class", "Audio/Source"),
                        ("device.api", "alsa"),
                    ]),
                ),
                (
                    61,
                    node(&[("node.name", COMBINED), ("media.class", "Audio/Sink")]),
                ),
                (
                    62,
                    node(&[
                        ("node.name", "blue2th_combined_old"),
                        ("media.class", "Audio/Sink"),
                    ]),
                ),
                (
                    63,
                    node(&[
                        ("node.name", "x.blue2th_combined"),
                        ("media.class", "Audio/Sink"),
                    ]),
                ),
                // Our loopback pair.
                (
                    70,
                    node(&[
                        ("node.name", "input.loopback-6815-13"),
                        ("media.class", "Stream/Input/Audio"),
                        ("target.object", COMBINED),
                        ("stream.capture.sink", "true"),
                        ("node.link-group", "loopback-6815-13"),
                    ]),
                ),
                (
                    71,
                    node(&[
                        ("node.name", "output.loopback-6815-13"),
                        ("media.class", "Stream/Output/Audio"),
                        ("target.object", SPEAKER),
                        ("node.link-group", "loopback-6815-13"),
                    ]),
                ),
                // Another combined sink's pair.
                (
                    72,
                    node(&[
                        ("node.name", "input.loopback-6815-14"),
                        ("media.class", "Stream/Input/Audio"),
                        ("target.object", "other_combined"),
                        ("stream.capture.sink", "true"),
                        ("node.link-group", "loopback-6815-14"),
                    ]),
                ),
                (
                    73,
                    node(&[
                        ("node.name", "output.loopback-6815-14"),
                        ("media.class", "Stream/Output/Audio"),
                        ("target.object", "bluez_output.AA_BB_CC_DD_EE_FF.1"),
                        ("node.link-group", "loopback-6815-14"),
                    ]),
                ),
                // A pair capturing a longer namesake.
                (
                    74,
                    node(&[
                        ("node.name", "input.loopback-6815-15"),
                        ("media.class", "Stream/Input/Audio"),
                        ("target.object", "blue2th_combined_old"),
                        ("stream.capture.sink", "true"),
                        ("node.link-group", "loopback-6815-15"),
                    ]),
                ),
                (
                    75,
                    node(&[
                        ("node.name", "output.loopback-6815-15"),
                        ("media.class", "Stream/Output/Audio"),
                        ("target.object", "bluez_output.99_88_77_66_55_44.1"),
                        ("node.link-group", "loopback-6815-15"),
                    ]),
                ),
                // A loopback feeding *into* the combined sink from the ALSA input:
                // its capture stream does not target ours, so it is not a branch.
                (
                    76,
                    node(&[
                        ("node.name", "input.loopback-6815-16"),
                        ("media.class", "Stream/Input/Audio"),
                        ("target.object", "alsa_input.pci-0000_00_1f.3.analog-stereo"),
                        ("node.link-group", "loopback-6815-16"),
                    ]),
                ),
                (
                    77,
                    node(&[
                        ("node.name", "output.loopback-6815-16"),
                        ("media.class", "Stream/Output/Audio"),
                        ("target.object", COMBINED),
                        ("node.link-group", "loopback-6815-16"),
                    ]),
                ),
            ],
            &[],
        )
    }

    // Criterion: the combined sink is matched on `node.name` by **exact** name —
    // a name merely containing `sink_name` is never selected.
    #[test]
    fn test_foreign_combined_globals_matches_the_sink_by_exact_name_only() {
        let selected = foreign_combined_globals(&foreign_mirror(), COMBINED);

        assert!(
            selected.contains(&61),
            "the combined sink itself, got {selected:?}"
        );
        assert!(!selected.contains(&62), "a longer namesake is not ours");
        assert!(!selected.contains(&63), "a name ending in ours is not ours");
        assert!(
            !selected.contains(&74) && !selected.contains(&75),
            "a pair capturing a longer namesake is not ours"
        );
    }

    // Criterion: a loopback pair whose capture stream targets the sink is taken
    // whole — both members of its `node.link-group` — and no other pair is.
    #[test]
    fn test_foreign_combined_globals_takes_both_members_of_a_loopback_pair() {
        assert_eq!(
            sorted(foreign_combined_globals(&foreign_mirror(), COMBINED)),
            vec![61, 70, 71],
            "the sink and its one pair, nothing else"
        );
    }

    // Criterion: an ALSA node is never selected — destroying one switches its
    // card's profile to `off` (the 2026-09-19 session) — nor is a loopback
    // feeding into the combined sink from an ALSA source.
    #[test]
    fn test_foreign_combined_globals_never_names_an_alsa_node() {
        let selected = foreign_combined_globals(&foreign_mirror(), COMBINED);

        for alsa in [39, 40, 76, 77] {
            assert!(
                !selected.contains(&alsa),
                "global {alsa} is not ours, got {selected:?}"
            );
        }
        assert!(!selected.is_empty(), "the combined sink itself is selected");
    }

    // Criterion (the empty value is a wildcard): an empty sink name selects
    // nothing, even against nodes whose name or target is empty.
    #[test]
    fn test_foreign_combined_globals_of_an_empty_name_selects_nothing() {
        let mut mirror = foreign_mirror();
        mirror.nodes.insert(
            80,
            node(&[("node.name", ""), ("media.class", "Audio/Sink")]),
        );
        mirror.nodes.insert(
            81,
            node(&[
                ("node.name", "input.loopback-6815-17"),
                ("target.object", ""),
                ("node.link-group", ""),
            ]),
        );

        assert!(foreign_combined_globals(&mirror, "").is_empty());
        assert_eq!(
            sorted(foreign_combined_globals(&mirror, COMBINED)),
            vec![61, 70, 71],
            "and the nameless nodes do not join a real teardown either"
        );
    }

    // ─── default_sink_metadata_value ─────────────────────────────────────────

    // Criterion: `set_default_sink` writes `{"name": "<sink>"}` on
    // `default.configured.audio.sink`, the JSON `pactl set-default-sink` wrote.
    #[test]
    fn test_default_sink_metadata_value_is_the_json_pactl_writes() {
        let value: Option<serde_json::Value> =
            serde_json::from_str(&default_sink_metadata_value(COMBINED)).ok();

        assert_eq!(value, Some(serde_json::json!({ "name": COMBINED })));
    }

    // Criterion: the value is JSON, not a template — a name carrying a quote
    // stays one string.
    #[test]
    fn test_default_sink_metadata_value_escapes_the_name() {
        let value: Option<serde_json::Value> =
            serde_json::from_str(&default_sink_metadata_value("odd\"sink")).ok();

        assert_eq!(value, Some(serde_json::json!({ "name": "odd\"sink" })));
    }

    // ─── Volume on the device Route ──────────────────────────────────────────

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    // Criterion: the volume is the cube root of the first `channelVolumes`
    // entry (`0.006749 → 0.189`, `1.0 → 1.0`, `0 → 0`).
    #[test]
    fn test_volume_fraction_from_route_is_the_cube_root_of_the_first_channel() {
        let read = volume_fraction_from_route(&[0.006749, 0.5]);
        assert!(
            read.is_some_and(|v| close(v, 0.189, 1e-3)),
            "0.006749 reads 0.189, got {read:?}"
        );
        assert_eq!(volume_fraction_from_route(&[1.0, 1.0]), Some(1.0));
        assert_eq!(volume_fraction_from_route(&[0.0]), Some(0.0));
    }

    // Criterion: a Route with no channel volume cannot be read — `None`, never
    // a silent zero.
    #[test]
    fn test_volume_fraction_from_route_without_channels_is_none() {
        assert_eq!(volume_fraction_from_route(&[]), None);
    }

    // Criterion: `set_sink_volume` writes `level³` on every channel
    // (`0.19 → 0.006859`).
    #[test]
    fn test_route_channel_volumes_cubes_the_level_for_every_channel() {
        let written = route_channel_volumes(0.19, 2);
        assert_eq!(written.len(), 2, "one entry per channel, got {written:?}");
        assert!(
            written.iter().all(|&v| close(v, 0.006859, 1e-6)),
            "0.19 writes 0.006859, got {written:?}"
        );
        assert_eq!(route_channel_volumes(1.0, 2), vec![1.0, 1.0]);
        assert_eq!(route_channel_volumes(0.0, 1), vec![0.0]);
    }

    // Criterion: the two mappings are inverse — what is written reads back as
    // the level the operator set.
    #[test]
    fn test_route_channel_volumes_round_trips_through_the_route_reading() {
        for level in [0.0_f32, 0.19, 0.4, 0.75, 1.0] {
            let read = volume_fraction_from_route(&route_channel_volumes(level, 2));
            assert!(
                read.is_some_and(|v| close(v, level, 1e-4)),
                "{level} reads back as {read:?}"
            );
        }
    }

    // Criterion: `sink_volume` resolves the sink node's `device.id` and its
    // `card.profile.device`; a sink without a `device.id` (the null sink itself)
    // has no Route, and an empty name resolves nothing.
    #[test]
    fn test_route_target_resolves_the_device_and_route_of_a_speaker_sink() {
        let mirror = mirror_of(
            &[
                (
                    57,
                    node(&[
                        ("node.name", SPEAKER),
                        ("media.class", "Audio/Sink"),
                        ("device.id", "77"),
                        ("card.profile.device", "1"),
                    ]),
                ),
                (
                    61,
                    node(&[("node.name", COMBINED), ("media.class", "Audio/Sink")]),
                ),
            ],
            &[],
        );

        assert_eq!(
            route_target(&mirror, SPEAKER),
            Some(RouteTarget {
                device_id: 77,
                route_device: 1
            })
        );
        assert_eq!(route_target(&mirror, COMBINED), None);
        assert_eq!(
            route_target(&mirror, "bluez_output.AA_BB_CC_DD_EE_FF.1"),
            None
        );
        assert_eq!(route_target(&mirror, ""), None);
    }

    // ─── The handle: command layer over a fake loop thread ───────────────────

    impl LoopSender for mpsc::Sender<Command> {
        fn send(&self, command: Command) -> Result<(), Command> {
            mpsc::Sender::send(self, command).map_err(|e| e.0)
        }
    }

    /// A loop thread that answers every command as a healthy graph holding
    /// `sinks` would, recording the name of each command it received.
    fn answering_loop(
        sinks: Vec<String>,
        received: Arc<Mutex<Vec<&'static str>>>,
    ) -> Box<dyn LoopSender> {
        let (tx, rx) = mpsc::channel::<Command>();
        std::thread::spawn(move || {
            for command in rx {
                let mut log = received.lock().unwrap();
                match command {
                    Command::Sinks { reply } => {
                        log.push("sinks");
                        let _ = reply.send(Ok(sinks.clone()));
                    },
                    Command::Branches { reply, .. } => {
                        log.push("branches");
                        let _ = reply.send(Ok(Vec::new()));
                    },
                    Command::CreateCombinedSink { reply, .. } => {
                        log.push("create_combined_sink");
                        let _ = reply.send(Ok(()));
                    },
                    Command::LoadBranch { reply, .. } => {
                        log.push("load_branch");
                        let _ = reply.send(Ok(()));
                    },
                    Command::UnloadBranch { reply, .. } => {
                        log.push("unload_branch");
                        let _ = reply.send(Ok(()));
                    },
                    Command::Teardown { reply, .. } => {
                        log.push("teardown");
                        let _ = reply.send(Ok(()));
                    },
                    Command::SetDefaultSink { reply, .. } => {
                        log.push("set_default_sink");
                        let _ = reply.send(Ok(()));
                    },
                    Command::SinkVolume { reply, .. } => {
                        log.push("sink_volume");
                        let _ = reply.send(Some(0.5));
                    },
                    Command::SetSinkVolume { reply, .. } => {
                        log.push("set_sink_volume");
                        let _ = reply.send(Ok(()));
                    },
                }
            }
        });
        Box::new(tx)
    }

    // Criterion: every method is one command answered through the reply
    // channel — the handle hands back what the loop thread answered.
    #[test]
    fn test_a_command_is_answered_by_the_loop_thread() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let mut graph = PipeWireGraph::with_loop(Box::new(move || {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert_eq!(graph.sinks().ok(), Some(vec![SPEAKER.to_string()]));
        assert_eq!(graph.sink_volume(SPEAKER), Some(0.5));
        assert_eq!(*received.lock().unwrap(), vec!["sinks", "sink_volume"]);
    }

    // Criterion: a loop thread that does not answer within 2 s is an
    // `AudioError::PipeWire` naming the timeout — the handle never waits longer.
    #[test]
    fn test_a_command_without_an_answer_errs_after_the_timeout() {
        // The receivers are kept alive and never read: the thread is "stuck".
        let parked: Arc<Mutex<Vec<mpsc::Receiver<Command>>>> = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&parked);
        let mut graph = PipeWireGraph::with_loop(Box::new(move || {
            let (tx, rx) = mpsc::channel::<Command>();
            keep.lock().unwrap().push(rx);
            Box::new(tx) as Box<dyn LoopSender>
        }));

        let started = Instant::now();
        let answer = graph.sinks();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= GRAPH_REPLY_TIMEOUT,
            "the handle waited the whole timeout, waited {elapsed:?}"
        );
        assert!(
            elapsed < GRAPH_REPLY_TIMEOUT + Duration::from_secs(2),
            "and not much more, waited {elapsed:?}"
        );
        let message = match answer {
            Err(AudioError::PipeWire(message)) => message,
            other => format!("not a PipeWire error: {other:?}"),
        };
        assert!(
            message.contains("did not answer within 2 s"),
            "the error names the timeout, got {message:?}"
        );
    }

    // Criterion: the timeout is two seconds.
    #[test]
    fn test_graph_reply_timeout_is_two_seconds() {
        assert_eq!(GRAPH_REPLY_TIMEOUT, Duration::from_secs(2));
    }

    // Criterion: a loop thread that took the command and died without answering
    // is an error at once — a dropped reply is not a slow one.
    #[test]
    fn test_a_dropped_reply_errs_without_waiting_for_the_timeout() {
        let mut graph = PipeWireGraph::with_loop(Box::new(|| {
            let (tx, rx) = mpsc::channel::<Command>();
            std::thread::spawn(move || {
                // Take one command and drop it, reply sender included.
                let _ = rx.recv();
            });
            Box::new(tx) as Box<dyn LoopSender>
        }));

        let started = Instant::now();
        let answer = graph.sinks();

        assert!(
            matches!(answer, Err(AudioError::PipeWire(_))),
            "got {answer:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dropped reply is known at once, waited {:?}",
            started.elapsed()
        );
    }

    // Criterion: a thread that has died (its receiver dropped) is respawned by
    // the next call, and that call is answered by the new thread.
    #[test]
    fn test_a_dead_loop_thread_is_respawned_by_the_next_command() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&spawned);
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let mut graph = PipeWireGraph::with_loop(Box::new(move || {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                // The first thread is already dead: its receiver is gone.
                let (tx, rx) = mpsc::channel::<Command>();
                drop(rx);
                Box::new(tx) as Box<dyn LoopSender>
            } else {
                answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
            }
        }));

        // The call that finds the thread dead may err ("cannot tell").
        let _first = graph.sinks();
        let second = graph.sinks();

        assert_eq!(second.ok(), Some(vec![SPEAKER.to_string()]));
        assert!(
            spawned.load(Ordering::SeqCst) >= 2,
            "a second thread was started"
        );
    }

    // Criterion (non-nominal): an empty sink name, target or prefix is refused
    // before anything reaches the loop.
    #[test]
    fn test_an_empty_name_is_refused_before_reaching_the_loop() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let mut graph = PipeWireGraph::with_loop(Box::new(move || {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert!(matches!(graph.teardown(""), Err(AudioError::PipeWire(_))));
        assert!(matches!(
            graph.create_combined_sink(""),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            graph.load_branch("", SPEAKER, 50),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            graph.load_branch(COMBINED, "", 50),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(graph.branches(""), Err(AudioError::PipeWire(_))));
        assert!(matches!(
            graph.set_default_sink(""),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            graph.set_sink_volume("", 0.5),
            Err(AudioError::PipeWire(_))
        ));
        assert_eq!(graph.sink_volume(""), None);

        assert!(
            received.lock().unwrap().is_empty(),
            "no command reached the loop, got {:?}",
            received.lock().unwrap()
        );
    }

    // Criterion: `PipeWireGraph::spawn()` never fails and never blocks — it does
    // not connect until the first command, so constructing it in a test touches
    // no daemon.
    #[test]
    fn test_spawn_returns_at_once_without_a_command() {
        let started = Instant::now();
        let graph = PipeWireGraph::spawn();
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(graph);
    }

    // ─── The loop side: connection lifecycle over a fake connector ───────────

    /// A connector that counts its attempts and fails the first `failures`.
    struct FakeConnector {
        attempts: Arc<AtomicUsize>,
        failures: usize,
    }

    impl Connector for FakeConnector {
        type Connection = usize;
        type Module = &'static str;
        type NullSink = &'static str;

        fn connect(&mut self) -> Result<usize, AudioError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.failures {
                Err(AudioError::PipeWire("no daemon".to_string()))
            } else {
                Ok(attempt)
            }
        }
    }

    fn fake_state(failures: usize) -> (LoopState<FakeConnector>, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let state = LoopState::new(FakeConnector {
            attempts: Arc::clone(&attempts),
            failures,
        });
        (state, attempts)
    }

    fn branch(sink: &str, latency_ms: u32) -> CombineBranch {
        CombineBranch {
            sink: sink.to_string(),
            latency_ms,
        }
    }

    // Criterion: the loop does not connect until the first command, then keeps
    // the one connection it opened.
    #[test]
    fn test_loop_state_does_not_connect_before_the_first_command() {
        let (mut state, attempts) = fake_state(0);
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        assert!(!state.is_connected());

        assert!(state.connection().is_ok());
        assert!(state.connection().is_ok());

        assert!(state.is_connected());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "connected once, reused after"
        );
    }

    // Criterion (non-nominal): with no daemon, the command errs with
    // `AudioError::PipeWire`, and the next command retries the connection.
    #[test]
    fn test_loop_state_without_a_daemon_errs_and_retries_on_the_next_command() {
        let (mut state, attempts) = fake_state(1);

        assert!(matches!(state.connection(), Err(AudioError::PipeWire(_))));
        assert!(!state.is_connected());

        assert!(state.connection().is_ok(), "the daemon is back");
        assert!(state.is_connected());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    // Criterion: the module ids are the graph's own counter, per combined sink,
    // and `take_module` hands one back exactly once.
    #[test]
    fn test_loop_state_keeps_the_modules_it_loaded_under_its_own_ids() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());

        let first = state.add_module(COMBINED, branch(SPEAKER, 50), "m1");
        let second = state.add_module(
            COMBINED,
            branch("bluez_output.11_22_33_44_55_66.1", 300),
            "m2",
        );
        let other = state.add_module("other_combined", branch(SPEAKER, 70), "m3");

        assert_ne!(first, second);
        assert_ne!(second, other);
        assert_eq!(
            state.modules_for(COMBINED),
            vec![
                (first, branch(SPEAKER, 50)),
                (second, branch("bluez_output.11_22_33_44_55_66.1", 300)),
            ]
        );

        assert_eq!(state.take_module(first), Some("m1"));
        assert_eq!(
            state.take_module(first),
            None,
            "a module is handed back once"
        );
        assert_eq!(
            state.modules_for(COMBINED),
            vec![(second, branch("bluez_output.11_22_33_44_55_66.1", 300))]
        );
    }

    // Criterion: a lost connection resets the loop thread's state — mirror,
    // modules, proxies — and the next command reconnects to a clean slate.
    #[test]
    fn test_loop_state_lost_connection_resets_the_state_and_reconnects() {
        let (mut state, attempts) = fake_state(0);
        assert!(state.connection().is_ok());
        state.mirror_mut().nodes.insert(
            61,
            node(&[("node.name", COMBINED), ("media.class", "Audio/Sink")]),
        );
        let id = state.add_module(COMBINED, branch(SPEAKER, 50), "m1");
        state.set_null_sink(COMBINED, "proxy");
        assert!(state.owns_null_sink(COMBINED));
        assert_eq!(state.modules_for(COMBINED).len(), 1);

        state.on_disconnect();

        assert!(!state.is_connected());
        assert!(
            state.mirror().is_empty(),
            "the mirror described a dead daemon"
        );
        assert!(state.modules_for(COMBINED).is_empty());
        assert_eq!(state.take_module(id), None);
        assert!(
            !state.owns_null_sink(COMBINED),
            "the null sink died with the daemon"
        );

        assert!(state.connection().is_ok());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "the next command reconnected"
        );
        let fresh = state.add_module(COMBINED, branch(SPEAKER, 50), "m2");
        assert_ne!(
            fresh, id,
            "an id handed out before the loss is never reused"
        );
    }
}
