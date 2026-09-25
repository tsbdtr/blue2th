// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`PipeWireGraph`]: the [`Graph`] that drives PipeWire natively, from a
//! `pw_main_loop` running on a thread of its own (#79).
//!
//! The PipeWire objects are `Rc`-based and never leave that thread. The handle
//! the router owns only holds a [`pipewire::channel`] sender into it: every
//! [`Graph`] method is one [`Command`], answered through a reply channel the
//! handle waits on for at most [`GRAPH_REPLY_TIMEOUT`].
//!
//! The decisions are pure functions over a [`Mirror`] of the registry, so the
//! tests pin them without a daemon; the loop side only applies them.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libspa::param::ParamType;
use libspa::pod::deserialize::PodDeserializer;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::device::{Device, DeviceListener};
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::metadata::Metadata;
use pw::node::{Node, NodeListener};
use pw::properties::PropertiesBox;
use pw::registry::{GlobalObject, RegistryRc};
use pw::types::ObjectType;

use crate::audio::{AudioError, CombineBranch};
use crate::graph::{Graph, LoadedBranch};

/// How long the handle waits for the loop thread to answer one command.
pub(crate) const GRAPH_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the loop thread gives the round trips of one command, all of them
/// together. It is shorter than [`GRAPH_REPLY_TIMEOUT`] whatever the number of
/// round trips, so a command answers with the daemon's error rather than with
/// the handle's timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1600);

/// The factory the combined sink's node is created from.
const NULL_SINK_FACTORY: &str = "support.null-audio-sink";

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
    SetBranchDelay {
        id: u32,
        delay_ms: u32,
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
    /// A graph over the PipeWire daemon of the current session. Starts nothing:
    /// the loop thread is spawned, and connects, on the first command.
    pub fn spawn() -> Self {
        Self::with_loop(Box::new(spawn_loop_thread))
    }

    /// A graph with no loop thread at all: every command errs at once, as when
    /// the thread cannot be started. It never reaches a daemon, which is what
    /// the store-free routers the integration tests build need: a graph over
    /// the session's daemon would tear down the operator's live combined sink.
    pub fn detached() -> Self {
        Self::with_loop(Box::new(|| Box::new(NoLoop)))
    }

    /// A graph whose loop threads are started by `spawn_loop`.
    pub(crate) fn with_loop(spawn_loop: SpawnLoop) -> Self {
        Self {
            spawn_loop,
            sender: None,
        }
    }

    /// Hand `command` to the loop thread, starting one when there is none and
    /// replacing one that has died.
    fn send(&mut self, command: Command) -> Result<(), AudioError> {
        let spawn_loop = &mut self.spawn_loop;
        let sender = self.sender.get_or_insert_with(|| spawn_loop());
        let Err(command) = sender.send(command) else {
            return Ok(());
        };
        // The thread is gone: a new one answers this very command.
        let fresh = spawn_loop();
        let sent = fresh.send(command);
        self.sender = Some(fresh);
        sent.map_err(|_| AudioError::PipeWire("the PipeWire graph thread is not running".into()))
    }

    /// Send the command `make` builds around a fresh reply channel, and wait for
    /// the answer.
    fn ask<R>(&mut self, make: impl FnOnce(mpsc::Sender<R>) -> Command) -> Result<R, AudioError> {
        let (reply, answer) = mpsc::channel();
        self.send(make(reply))?;
        answer
            .recv_timeout(GRAPH_REPLY_TIMEOUT)
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => AudioError::PipeWire(format!(
                    "PipeWire graph thread did not answer within {} s",
                    GRAPH_REPLY_TIMEOUT.as_secs()
                )),
                mpsc::RecvTimeoutError::Disconnected => AudioError::PipeWire(
                    "PipeWire graph thread dropped the command without answering".into(),
                ),
            })
    }
}

/// Refuse an empty name before it reaches the loop: an empty name is a wildcard
/// to every match below it, never "no node".
fn named(what: &str, name: &str) -> Result<(), AudioError> {
    if name.is_empty() {
        return Err(AudioError::PipeWire(format!("empty {what} name refused")));
    }
    Ok(())
}

impl Graph for PipeWireGraph {
    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        self.ask(|reply| Command::Sinks { reply })?
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::Branches {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::CreateCombinedSink {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        named("target sink", real_sink)?;
        self.ask(|reply| Command::LoadBranch {
            sink_name: sink_name.to_string(),
            real_sink: real_sink.to_string(),
            latency_ms,
            reply,
        })?
    }

    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        self.ask(|reply| Command::UnloadBranch { id, reply })?
    }

    fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
        // Red-phase stub: answers without reaching the loop.
        let _ = (id, delay_ms);
        Ok(())
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::Teardown {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn set_default_sink(&mut self, sink: &str) -> Result<(), AudioError> {
        named("sink", sink)?;
        self.ask(|reply| Command::SetDefaultSink {
            sink: sink.to_string(),
            reply,
        })?
    }

    fn sink_volume(&mut self, sink: &str) -> Option<f32> {
        if sink.is_empty() {
            return None;
        }
        self.ask(|reply| Command::SinkVolume {
            sink: sink.to_string(),
            reply,
        })
        .ok()
        .flatten()
    }

    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
        named("sink", sink)?;
        self.ask(|reply| Command::SetSinkVolume {
            sink: sink.to_string(),
            level,
            reply,
        })?
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

/// One port of the registry mirror: the properties of its global, among them
/// `node.id`, `port.direction` and `audio.channel`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PortEntry {
    pub(crate) props: BTreeMap<String, String>,
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
    pub(crate) ports: BTreeMap<u32, PortEntry>,
    pub(crate) devices: BTreeMap<u32, DeviceEntry>,
}

impl Mirror {
    /// Whether the mirror knows no global at all.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.links.is_empty() && self.devices.is_empty()
    }

    /// The ids of the nodes whose `node.name` is exactly `name`; none for an
    /// empty name.
    fn node_ids_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = u32> + 'a {
        self.nodes
            .iter()
            .filter(move |(_, node)| !name.is_empty() && node.prop("node.name") == Some(name))
            .map(|(id, _)| *id)
    }
}

impl NodeEntry {
    fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }
}

/// Where a sink's volume lives: the device global and the `Route` device index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteTarget {
    pub(crate) device_id: u32,
    pub(crate) route_device: i32,
}

/// The name of branch `id`'s capture (`in`) or playback (`out`) stream node.
fn branch_node_name(id: u32, end: &str) -> String {
    format!("blue2th_loop.{id}.{end}")
}

/// `value` as a quoted SPA-JSON string.
fn spa_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The `libpipewire-module-loopback` argument string for one branch.
pub(crate) fn loopback_module_args(
    sink_name: &str,
    real_sink: &str,
    latency_ms: u32,
    id: u32,
) -> Result<String, AudioError> {
    named("sink", sink_name)?;
    named("target sink", real_sink)?;
    let group = format!("blue2th_loop.{id}");
    let delay = format!("{}.{:03}", latency_ms / 1000, latency_ms % 1000);
    Ok(format!(
        "{{ node.group = {group} target.delay.sec = {delay} \
         capture.props = {{ node.name = {capture} target.object = {sink} \
         stream.capture.sink = true node.dont-reconnect = true }} \
         playback.props = {{ node.name = {playback} target.object = {real} \
         node.dont-reconnect = true }} }}",
        group = spa_string(&group),
        capture = spa_string(&branch_node_name(id, "in")),
        playback = spa_string(&branch_node_name(id, "out")),
        sink = spa_string(sink_name),
        real = spa_string(real_sink),
    ))
}

/// The largest delay a branch's `delay` node can be tuned to, in seconds: its
/// `max-delay`, fixed when the branch is loaded.
pub(crate) const MAX_DELAY_SECONDS: f32 = 1.0;

/// The `libpipewire-module-filter-chain` argument string for one delay branch
/// into `real_sink`, delayed by `delay_ms`.
pub(crate) fn delay_chain_module_args(
    real_sink: &str,
    delay_ms: u32,
    id: u32,
) -> Result<String, AudioError> {
    // Red-phase stub.
    let _ = (real_sink, delay_ms, id);
    Ok(String::new())
}

/// The `Props` param that sets a branch's `delay` node to `seconds`.
pub(crate) fn delay_props_pod(seconds: f32) -> Result<Vec<u8>, AudioError> {
    // Red-phase stub.
    let _ = seconds;
    Ok(Vec::new())
}

/// The `(output port, input port)` pairs linking `out_node`'s outputs to
/// `in_node`'s inputs, channel to channel.
pub(crate) fn channel_port_pairs(
    mirror: &Mirror,
    out_node: &str,
    in_node: &str,
) -> Vec<(u32, u32)> {
    // Red-phase stub.
    let _ = (mirror, out_node, in_node);
    Vec::new()
}

/// The properties the combined sink's `adapter` node is created with.
pub(crate) fn combined_sink_props(sink_name: &str) -> Vec<(String, String)> {
    [
        ("factory.name", NULL_SINK_FACTORY),
        ("node.name", sink_name),
        ("node.description", sink_name),
        ("media.class", "Audio/Sink"),
        ("audio.position", "FL,FR"),
        ("monitor.channel-volumes", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The node names of the mirror's `Audio/Sink` nodes.
pub(crate) fn sink_names(mirror: &Mirror) -> Vec<String> {
    mirror
        .nodes
        .values()
        .filter(|node| node.prop("media.class") == Some("Audio/Sink"))
        .filter_map(|node| node.prop("node.name"))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether branch `id` is fed by `sink_name` and feeds `real_sink`.
pub(crate) fn branch_liveness(mirror: &Mirror, id: u32, sink_name: &str, real_sink: &str) -> bool {
    // Red-phase stub: the #79 check, speaker side only.
    let _ = sink_name;
    let out_node = branch_node_name(id, "out");
    let outs: BTreeSet<u32> = mirror.node_ids_named(&out_node).collect();
    let sinks: BTreeSet<u32> = mirror.node_ids_named(real_sink).collect();
    mirror
        .links
        .values()
        .any(|link| outs.contains(&link.output_node) && sinks.contains(&link.input_node))
}

/// How long a branch may wait for the ports its monitor links need before it
/// counts as dead. A combined sink created a moment ago announces its monitor
/// ports only once the session manager has configured it, which takes seconds
/// on a server that has just started.
pub(crate) const PENDING_LINKS_GRACE: Duration = Duration::from_secs(10);

/// What `load_branch` answers once branch `id`'s module is loaded and kept:
/// `Ok`, whatever the sync and the wiring after it did. Reported as failed,
/// such a branch was never armed for its confirming reload (#75) while it
/// stayed listed, so it was never loaded again either — a silent start on it
/// had no remedy. A follow-up that failed leaves the branch waiting for its
/// ports or dead, which the router already handles.
pub(crate) fn kept_branch_load(
    id: u32,
    follow_up: Result<(), AudioError>,
) -> Result<(), AudioError> {
    let _ = id;
    follow_up
}

/// What branch `id` reports as its liveness. `pending_for` is how long its
/// monitor links have been waiting for their ports, `None` once they are
/// made: a branch still waiting is "cannot tell", not dead, until the grace
/// runs out or its playback side is gone.
pub(crate) fn branch_live(
    mirror: &Mirror,
    id: u32,
    sink_name: &str,
    real_sink: &str,
    pending_for: Option<Duration>,
) -> Option<bool> {
    let _ = pending_for;
    Some(branch_liveness(mirror, id, sink_name, real_sink))
}

/// Whether branch `id`'s capture side can be linked from `sink_name` now:
/// both channel pairs are in the mirror.
pub(crate) fn ready_to_wire(mirror: &Mirror, sink_name: &str, id: u32) -> bool {
    let _ = (mirror, sink_name, id);
    false
}

/// The globals a teardown of `sink_name` destroys whoever owns them.
pub(crate) fn foreign_combined_globals(mirror: &Mirror, sink_name: &str) -> Vec<u32> {
    if sink_name.is_empty() {
        return Vec::new();
    }
    // A hardware node is never ours: destroying one switches its card off.
    let candidates = || {
        mirror
            .nodes
            .iter()
            .filter(|(_, node)| node.prop("device.api").is_none())
    };
    let mut selected: BTreeSet<u32> = candidates()
        .filter(|(_, node)| node.prop("node.name") == Some(sink_name))
        .map(|(id, _)| *id)
        .collect();
    // The capture streams reading the sink, and the groups pairing each one with
    // its playback stream.
    let mut groups = BTreeSet::new();
    for (id, node) in candidates() {
        let captures = node.prop("media.class") == Some("Stream/Input/Audio")
            || node.prop("stream.capture.sink") == Some("true");
        if captures && node.prop("target.object") == Some(sink_name) {
            selected.insert(*id);
            if let Some(group) = node.prop("node.link-group").filter(|g| !g.is_empty()) {
                groups.insert(group);
            }
        }
    }
    selected.extend(
        candidates()
            .filter(|(_, node)| {
                node.prop("node.link-group")
                    .is_some_and(|group| groups.contains(group))
            })
            .map(|(id, _)| *id),
    );
    selected.into_iter().collect()
}

/// The value written to `default.configured.audio.sink` to make `sink` default.
pub(crate) fn default_sink_metadata_value(sink: &str) -> String {
    serde_json::json!({ "name": sink }).to_string()
}

/// The device and `Route` index carrying `sink`'s volume.
pub(crate) fn route_target(mirror: &Mirror, sink: &str) -> Option<RouteTarget> {
    mirror.node_ids_named(sink).find_map(|id| {
        let node = mirror.nodes.get(&id)?;
        Some(RouteTarget {
            device_id: node.prop("device.id")?.parse().ok()?,
            route_device: node.prop("card.profile.device")?.parse().ok()?,
        })
    })
}

/// The volume fraction a device `Route`'s `channelVolumes` stands for.
pub(crate) fn volume_fraction_from_route(channel_volumes: &[f32]) -> Option<f32> {
    channel_volumes.first().map(|volume| volume.cbrt())
}

/// The `channelVolumes` a `Route` is written with to set the volume to `level`.
pub(crate) fn route_channel_volumes(level: f32, channels: usize) -> Vec<f32> {
    vec![level.powi(3); channels]
}

/// Opens a connection to the daemon from the loop thread.
pub(crate) trait Connector {
    /// The client-side context every connection is opened from.
    type Context;
    /// What the loop thread holds while connected.
    type Connection;
    /// One loopback module loaded into the server process.
    type Module;
    /// The proxy owning the combined null sink.
    type NullSink;
    /// Create the context. It needs no daemon.
    fn context(&mut self) -> Result<Self::Context, AudioError>;
    /// Connect to the daemon from `context`; an `Err` is "no daemon".
    fn connect(&mut self, context: &Self::Context) -> Result<Self::Connection, AudioError>;
}

/// The loop thread's state: the connection, the mirror, and what the graph
/// itself created.
pub(crate) struct LoopState<C: Connector> {
    connector: C,
    // Declared before `connection`, so dropped before it: a proxy outliving the
    // core that owns it would be freed twice.
    null_sinks: BTreeMap<String, C::NullSink>,
    modules: BTreeMap<u32, (String, CombineBranch, C::Module)>,
    connection: Option<C::Connection>,
    // Declared after `connection`, so dropped after it: a connection is opened
    // from this context and must not outlive it.
    context: Option<C::Context>,
    mirror: Mirror,
    next_module_id: u32,
    /// When the command being handled runs out of time for its round trips.
    deadline: Instant,
    /// The branches whose monitor links wait for their ports, and since when.
    pending_links: BTreeMap<u32, Instant>,
}

impl<C: Connector> LoopState<C> {
    /// A loop that has not connected yet.
    pub(crate) fn new(connector: C) -> Self {
        Self {
            connector,
            null_sinks: BTreeMap::new(),
            modules: BTreeMap::new(),
            connection: None,
            context: None,
            mirror: Mirror::default(),
            next_module_id: 0,
            deadline: Instant::now(),
            pending_links: BTreeMap::new(),
        }
    }

    /// The live connection, connecting first when there is none.
    pub(crate) fn connection(&mut self) -> Result<&mut C::Connection, AudioError> {
        let connection = match self.connection.take() {
            Some(connection) => connection,
            None => {
                // One context for the thread's lifetime: destroying one joins
                // its `module-rt` thread, which can block on RTKit for 25 s.
                let context = match self.context.take() {
                    Some(context) => context,
                    None => self.connector.context()?,
                };
                self.connector.connect(self.context.insert(context))?
            },
        };
        Ok(self.connection.insert(connection))
    }

    /// Whether a connection is currently held.
    #[cfg(test)]
    pub(crate) fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// The registry mirror.
    pub(crate) fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// The registry mirror, for the listener callbacks.
    pub(crate) fn mirror_mut(&mut self) -> &mut Mirror {
        &mut self.mirror
    }

    /// The id [`Self::add_module`] hands the next module.
    fn next_module_id(&self) -> u32 {
        self.next_module_id
    }

    /// Keep a loaded module and return the graph's own id for it.
    pub(crate) fn add_module(
        &mut self,
        sink_name: &str,
        branch: CombineBranch,
        module: C::Module,
    ) -> u32 {
        let id = self.next_module_id;
        self.next_module_id = self.next_module_id.wrapping_add(1);
        self.modules
            .insert(id, (sink_name.to_string(), branch, module));
        id
    }

    /// The modules the graph loaded for `sink_name`, with their ids.
    pub(crate) fn modules_for(&self, sink_name: &str) -> Vec<(u32, CombineBranch)> {
        self.modules
            .iter()
            .filter(|(_, (sink, _, _))| sink == sink_name)
            // Cloned: the answer leaves the loop thread, the module stays.
            .map(|(id, (_, branch, _))| (*id, branch.clone()))
            .collect()
    }

    /// Record that module `id` now runs at `delay_ms`, so the branches report
    /// the delay last applied. An id the graph does not hold is an `Err`.
    pub(crate) fn record_module_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
        // Red-phase stub.
        let _ = (id, delay_ms);
        Ok(())
    }

    /// Record that branch `id`'s monitor links wait for their ports since `since`.
    pub(crate) fn mark_links_pending(&mut self, id: u32, since: Instant) {
        let _ = (id, since);
    }

    /// Record that branch `id`'s monitor links are made.
    pub(crate) fn mark_links_made(&mut self, id: u32) {
        let _ = id;
    }

    /// How long branch `id`'s monitor links have waited at `now`; `None` when
    /// they are not waiting.
    pub(crate) fn links_pending_for(&self, id: u32, now: Instant) -> Option<Duration> {
        let _ = (id, now);
        None
    }

    /// The branches whose monitor links wait, with the combined sink each one
    /// is fed from.
    pub(crate) fn pending_link_branches(&self) -> Vec<(u32, String)> {
        Vec::new()
    }

    /// Forget the module `id` and hand it back for destruction.
    pub(crate) fn take_module(&mut self, id: u32) -> Option<C::Module> {
        self.modules.remove(&id).map(|(_, _, module)| module)
    }

    /// Keep the proxy owning the combined sink `sink_name`.
    pub(crate) fn set_null_sink(&mut self, sink_name: &str, proxy: C::NullSink) {
        self.null_sinks.insert(sink_name.to_string(), proxy);
    }

    /// Forget the proxy owning the combined sink `sink_name` and hand it back.
    fn take_null_sink(&mut self, sink_name: &str) -> Option<C::NullSink> {
        self.null_sinks.remove(sink_name)
    }

    /// Whether the graph itself owns the combined sink `sink_name`.
    pub(crate) fn owns_null_sink(&self, sink_name: &str) -> bool {
        self.null_sinks.contains_key(sink_name)
    }

    /// Whether `name` is a null sink this graph does not own — a leftover a
    /// build must replace, never reuse: the router reuses a listed combined
    /// sink, and a leftover's own loopbacks would keep feeding the speakers
    /// next to the ones this graph loads (#78).
    ///
    /// `factory.name` is not in the summary the registry announces with a
    /// node, only in the node's own info: this relies on `refresh` binding
    /// every node before the mirror is read.
    fn is_foreign_null_sink(&self, name: &str) -> bool {
        !self.owns_null_sink(name)
            && self
                .mirror()
                .node_ids_named(name)
                .filter_map(|id| self.mirror().nodes.get(&id))
                .any(|node| node.prop("factory.name") == Some(NULL_SINK_FACTORY))
    }

    /// The mirror's sinks, less the null sinks this graph does not own.
    fn listed_sinks(&self) -> Vec<String> {
        sink_names(&self.mirror)
            .into_iter()
            .filter(|name| !self.is_foreign_null_sink(name))
            .collect()
    }

    /// The core `error`/disconnect callback: the connection is gone.
    pub(crate) fn on_disconnect(&mut self) {
        // Proxies first, while their core still exists. The modules are only
        // forgotten: a loopback unloads itself on its core's error. The
        // context stays, so the next command reconnects from it.
        self.null_sinks.clear();
        self.modules.clear();
        self.connection = None;
        self.mirror = Mirror::default();
    }
}

// ─── The production loop thread ─────────────────────────────────────────────

/// A loop that is not there: every command comes back, so the handle knows.
struct NoLoop;

impl LoopSender for NoLoop {
    fn send(&self, command: Command) -> Result<(), Command> {
        Err(command)
    }
}

/// The handle's end into a real loop thread.
struct PwLoopSender {
    sender: pw::channel::Sender<Command>,
    thread: JoinHandle<()>,
}

impl LoopSender for PwLoopSender {
    fn send(&self, command: Command) -> Result<(), Command> {
        // The channel's queue outlives the thread, so a send to a dead thread
        // would succeed and wait out the timeout: ask the thread instead.
        if self.thread.is_finished() {
            return Err(command);
        }
        self.sender.send(command)
    }
}

/// Start a loop thread; [`NoLoop`] when the thread cannot even be started.
fn spawn_loop_thread() -> Box<dyn LoopSender> {
    let (sender, receiver) = pw::channel::channel::<Command>();
    match std::thread::Builder::new()
        .name("pipewire-graph".into())
        .spawn(move || run_loop_thread(receiver))
    {
        Ok(thread) => Box::new(PwLoopSender { sender, thread }),
        Err(e) => {
            tracing::error!("cannot start the PipeWire graph thread: {e}");
            Box::new(NoLoop)
        },
    }
}

/// The loop thread: receive commands, answer each against the daemon, and
/// drop the connection's state when the daemon goes away.
fn run_loop_thread(receiver: pw::channel::Receiver<Command>) {
    pw::init();
    let mainloop = match MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(e) => {
            tracing::error!("cannot create the PipeWire main loop: {e}");
            return;
        },
    };
    // Commands are queued by the channel callback and handled outside of it, so
    // a command can iterate the loop while it waits for the daemon.
    let inbox: Rc<RefCell<VecDeque<Command>>> = Rc::default();
    let _attached = receiver.attach(mainloop.loop_(), {
        let inbox = Rc::clone(&inbox);
        move |command| inbox.borrow_mut().push_back(command)
    });
    let mut state = LoopState::new(PwConnector {
        mainloop: mainloop.clone(),
    });
    loop {
        mainloop.loop_().iterate(Timeout::Infinite);
        state.forget_a_lost_connection();
        loop {
            let next = inbox.borrow_mut().pop_front();
            let Some(command) = next else {
                break;
            };
            handle(&mut state, command);
            state.forget_a_lost_connection();
        }
    }
}

fn pw_error(what: &'static str) -> impl Fn(pw::Error) -> AudioError {
    move |e| AudioError::PipeWire(format!("{what}: {e}"))
}

/// Opens [`PwConnection`]s on the loop thread's main loop.
struct PwConnector {
    mainloop: MainLoopRc,
}

impl Connector for PwConnector {
    type Context = ContextRc;
    type Connection = PwConnection;
    type Module = InProcessModule;
    type NullSink = Node;

    fn context(&mut self) -> Result<ContextRc, AudioError> {
        ContextRc::new(&self.mainloop, None).map_err(pw_error("cannot create a PipeWire context"))
    }

    fn connect(&mut self, context: &ContextRc) -> Result<PwConnection, AudioError> {
        PwConnection::open(&self.mainloop, context)
    }
}

/// A loopback loaded into this process. It holds nothing: the module owns
/// itself and goes away with its streams — see [`PwConnection::unload`].
struct InProcessModule;

/// What the registry and core callbacks report, shared with the loop side.
#[derive(Default)]
struct Shared {
    mirror: Mirror,
    globals: BTreeMap<u32, GlobalObject<PropertiesBox>>,
    done: Option<i32>,
    lost: bool,
}

/// A connection to the daemon and the registry mirror it keeps.
struct PwConnection {
    // Field order is drop order: every proxy and listener before the core that
    // owns it, the core before the context.
    bound_nodes: BTreeMap<u32, (Node, NodeListener)>,
    _registry_listener: pw::registry::Listener,
    _core_listener: pw::core::Listener,
    registry: RegistryRc,
    core: CoreRc,
    context: ContextRc,
    mainloop: MainLoopRc,
    shared: Rc<RefCell<Shared>>,
}

/// One entry of a device's `Route` param.
struct Route {
    index: i32,
    device: i32,
    channel_volumes: Vec<f32>,
}

impl PwConnection {
    fn open(mainloop: &MainLoopRc, context: &ContextRc) -> Result<Self, AudioError> {
        let core = context
            .connect_rc(None)
            .map_err(pw_error("cannot connect to PipeWire"))?;
        let registry = core
            .get_registry_rc()
            .map_err(pw_error("cannot read the PipeWire registry"))?;
        let shared = Rc::new(RefCell::new(Shared::default()));
        let core_listener = core
            .add_listener_local()
            .done({
                let shared = Rc::clone(&shared);
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE {
                        shared.borrow_mut().done = Some(seq.seq());
                    }
                }
            })
            .error({
                let shared = Rc::clone(&shared);
                move |id, _seq, res, message| {
                    if id == pw::core::PW_ID_CORE {
                        tracing::warn!("PipeWire connection lost ({res}): {message}");
                        shared.borrow_mut().lost = true;
                    }
                }
            })
            .register();
        let registry_listener = registry
            .add_listener_local()
            .global({
                let shared = Rc::clone(&shared);
                move |global| shared.borrow_mut().add_global(global)
            })
            .global_remove({
                let shared = Rc::clone(&shared);
                move |id| shared.borrow_mut().remove_global(id)
            })
            .register();
        Ok(Self {
            bound_nodes: BTreeMap::new(),
            _registry_listener: registry_listener,
            _core_listener: core_listener,
            registry,
            core,
            // Cloned: the loop state owns the context; the connection holds a
            // second reference for the module FFI calls.
            context: context.clone(),
            mainloop: mainloop.clone(),
            shared,
        })
    }

    fn is_lost(&self) -> bool {
        self.shared.borrow().lost
    }

    /// One `core.sync` round trip: every event the daemon emitted before it has
    /// been delivered when this returns `Ok`. It errs once `deadline` — the
    /// command's, not its own — has passed.
    fn roundtrip(&self, deadline: Instant) -> Result<(), AudioError> {
        let pending = self
            .core
            .sync(0)
            .map_err(pw_error("cannot sync with PipeWire"))?
            .seq();
        loop {
            {
                let shared = self.shared.borrow();
                if shared.lost {
                    return Err(AudioError::PipeWire("PipeWire connection lost".into()));
                }
                if shared.done.is_some_and(|done| done >= pending) {
                    return Ok(());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(AudioError::PipeWire(
                    "PipeWire did not answer a sync round trip".into(),
                ));
            }
            self.mainloop.loop_().iterate(Timeout::Finite(left));
        }
    }

    /// Bring the mirror up to date: a round trip for the globals, then one more
    /// for the full properties of every node bound on the way. The registry
    /// only announces a node's summary; the stream targets and link groups the
    /// pure functions read are in the node's own info.
    fn refresh(&mut self, deadline: Instant) -> Result<Mirror, AudioError> {
        self.roundtrip(deadline)?;
        let unbound: Vec<u32> = {
            let shared = self.shared.borrow();
            self.bound_nodes
                .retain(|id, _| shared.mirror.nodes.contains_key(id));
            shared
                .mirror
                .nodes
                .keys()
                .filter(|id| !self.bound_nodes.contains_key(id))
                .copied()
                .collect()
        };
        for id in &unbound {
            let bound = {
                let shared = self.shared.borrow();
                match shared.globals.get(id) {
                    Some(global) => self.registry.bind::<Node, _>(global),
                    None => continue,
                }
            };
            let Ok(node) = bound else {
                continue;
            };
            let listener = node
                .add_listener_local()
                .info({
                    let shared = Rc::clone(&self.shared);
                    let id = *id;
                    move |info| {
                        let Some(props) = info.props() else {
                            return;
                        };
                        let mut shared = shared.borrow_mut();
                        if let Some(entry) = shared.mirror.nodes.get_mut(&id) {
                            for (key, value) in props.iter() {
                                entry.props.insert(key.to_string(), value.to_string());
                            }
                        }
                    }
                })
                .register();
            self.bound_nodes.insert(*id, (node, listener));
        }
        if !unbound.is_empty() {
            self.roundtrip(deadline)?;
        }
        // Cloned: the loop side reads a snapshot while the callbacks keep
        // writing the live one.
        Ok(self.shared.borrow().mirror.clone())
    }

    /// Create the combined sink's `adapter` node, owned by this connection.
    fn create_null_sink(&self, sink_name: &str) -> Result<Node, AudioError> {
        let mut props = PropertiesBox::new();
        for (key, value) in combined_sink_props(sink_name) {
            props.insert(key, value);
        }
        self.core
            .create_object::<Node>("adapter", &props)
            .map_err(pw_error("cannot create the combined sink"))
    }

    /// Load one `libpipewire-module-loopback` into this process.
    fn load_loopback(&self, args: &str) -> Result<InProcessModule, AudioError> {
        let name = CString::new("libpipewire-module-loopback")
            .map_err(|e| AudioError::PipeWire(e.to_string()))?;
        let args = CString::new(args).map_err(|e| AudioError::PipeWire(e.to_string()))?;
        // SAFETY: `self.context` is a live `pw_context` for the whole call, on
        // the thread that created it; both strings are NUL-terminated and
        // outlive the call, which copies them; a null `properties` is allowed.
        // The module returned is owned by the context, which frees it when it
        // is destroyed — this code never dereferences nor frees it.
        let module = unsafe {
            pw::sys::pw_context_load_module(
                self.context.as_raw_ptr(),
                name.as_ptr(),
                args.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if module.is_null() {
            return Err(AudioError::PipeWire(format!(
                "cannot load the loopback module: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(InProcessModule)
    }

    /// Unload branch `id` by destroying its two stream nodes: the loopback
    /// module destroys itself once its streams are gone.
    ///
    /// Not `pw_impl_module_destroy`: a loopback whose target vanished destroys
    /// itself too, with no notice to this code, so its handle can dangle at any
    /// time. Its nodes are looked up in the daemon's registry instead, where a
    /// module that is gone has none left to destroy.
    fn unload(&self, mirror: &Mirror, id: u32) {
        let ins = branch_node_name(id, "in");
        let outs = branch_node_name(id, "out");
        let nodes: Vec<u32> = mirror
            .node_ids_named(&ins)
            .chain(mirror.node_ids_named(&outs))
            .collect();
        tracing::debug!("unloading loopback branch {id}: destroying nodes {nodes:?}");
        for node in nodes {
            self.destroy_global(node);
        }
    }

    fn destroy_global(&self, id: u32) {
        let _ = self.registry.destroy_global(id);
    }

    /// Point the `default` metadata's configured sink at `sink`.
    fn set_default_sink(&self, sink: &str, deadline: Instant) -> Result<(), AudioError> {
        let metadata = {
            let shared = self.shared.borrow();
            let global = shared
                .globals
                .values()
                .find(|global| {
                    global.type_ == ObjectType::Metadata
                        && global
                            .props
                            .as_ref()
                            .and_then(|props| props.get("metadata.name"))
                            == Some("default")
                })
                .ok_or_else(|| AudioError::PipeWire("no default metadata object".into()))?;
            self.registry
                .bind::<Metadata, _>(global)
                .map_err(pw_error("cannot bind the default metadata"))?
        };
        metadata.set_property(
            0,
            "default.configured.audio.sink",
            Some("Spa:String:JSON"),
            Some(&default_sink_metadata_value(sink)),
        );
        self.roundtrip(deadline)
    }

    /// Bind the device `device_id` and read its `Route` param.
    fn routes(
        &self,
        device_id: u32,
        deadline: Instant,
    ) -> Result<(Device, Vec<Route>), AudioError> {
        let device = {
            let shared = self.shared.borrow();
            let global = shared
                .globals
                .get(&device_id)
                .filter(|global| global.type_ == ObjectType::Device)
                .ok_or_else(|| AudioError::PipeWire(format!("no device {device_id}")))?;
            self.registry
                .bind::<Device, _>(global)
                .map_err(pw_error("cannot bind the speaker's device"))?
        };
        let routes: Rc<RefCell<Vec<Route>>> = Rc::default();
        let listener: DeviceListener = device
            .add_listener_local()
            .param({
                let routes = Rc::clone(&routes);
                move |_seq, _id, _index, _next, param| {
                    if let Some(route) = param.and_then(parse_route) {
                        routes.borrow_mut().push(route);
                    }
                }
            })
            .register();
        device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);
        let synced = self.roundtrip(deadline);
        drop(listener);
        synced?;
        let routes = routes.take();
        Ok((device, routes))
    }

    /// The route of `target`, and the device carrying it.
    fn route(&self, target: RouteTarget, deadline: Instant) -> Result<(Device, Route), AudioError> {
        let (device, routes) = self.routes(target.device_id, deadline)?;
        let route = routes
            .into_iter()
            .find(|route| route.device == target.route_device)
            .ok_or_else(|| AudioError::PipeWire("the speaker's device has no route".into()))?;
        Ok((device, route))
    }
}

impl Shared {
    fn add_global(&mut self, global: &GlobalObject<&libspa::utils::dict::DictRef>) {
        let props: BTreeMap<String, String> = global
            .props
            .map(|props| {
                props
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        match global.type_ {
            ObjectType::Node => {
                self.mirror.nodes.insert(global.id, NodeEntry { props });
            },
            ObjectType::Device => {
                self.mirror.devices.insert(global.id, DeviceEntry { props });
            },
            ObjectType::Link => {
                let end = |key: &str| props.get(key).and_then(|v| v.parse::<u32>().ok());
                if let (Some(output_node), Some(input_node)) =
                    (end("link.output.node"), end("link.input.node"))
                {
                    self.mirror.links.insert(
                        global.id,
                        LinkEntry {
                            output_node,
                            input_node,
                        },
                    );
                }
                return;
            },
            ObjectType::Metadata => {},
            _ => return,
        }
        self.globals.insert(global.id, global.to_owned());
    }

    fn remove_global(&mut self, id: u32) {
        self.mirror.nodes.remove(&id);
        self.mirror.links.remove(&id);
        self.mirror.devices.remove(&id);
        self.globals.remove(&id);
    }
}

/// Read one `Route` param: its index, its device, and its channel volumes.
fn parse_route(pod: &Pod) -> Option<Route> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(object) = value else {
        return None;
    };
    let (mut index, mut device, mut channel_volumes) = (None, None, Vec::new());
    for property in object.properties {
        match (property.key, property.value) {
            (key, Value::Int(v)) if key == libspa::sys::SPA_PARAM_ROUTE_index => index = Some(v),
            (key, Value::Int(v)) if key == libspa::sys::SPA_PARAM_ROUTE_device => device = Some(v),
            (key, Value::Object(props)) if key == libspa::sys::SPA_PARAM_ROUTE_props => {
                for prop in props.properties {
                    if let (key, Value::ValueArray(ValueArray::Float(volumes))) =
                        (prop.key, prop.value)
                    {
                        if key == libspa::sys::SPA_PROP_channelVolumes {
                            channel_volumes = volumes;
                        }
                    }
                }
            },
            _ => {},
        }
    }
    Some(Route {
        index: index?,
        device: device?,
        channel_volumes,
    })
}

/// The `Route` param that sets `route`'s channels to `volumes` and keeps it.
fn route_pod(route: &Route, volumes: Vec<f32>) -> Result<Vec<u8>, AudioError> {
    let property = |key, value| Property {
        key,
        flags: PropertyFlags::empty(),
        value,
    };
    let value = Value::Object(Object {
        type_: libspa::sys::SPA_TYPE_OBJECT_ParamRoute,
        id: libspa::sys::SPA_PARAM_Route,
        properties: vec![
            property(libspa::sys::SPA_PARAM_ROUTE_index, Value::Int(route.index)),
            property(
                libspa::sys::SPA_PARAM_ROUTE_device,
                Value::Int(route.device),
            ),
            property(
                libspa::sys::SPA_PARAM_ROUTE_props,
                Value::Object(Object {
                    type_: libspa::sys::SPA_TYPE_OBJECT_Props,
                    id: libspa::sys::SPA_PARAM_Route,
                    properties: vec![property(
                        libspa::sys::SPA_PROP_channelVolumes,
                        Value::ValueArray(ValueArray::Float(volumes)),
                    )],
                }),
            ),
            property(libspa::sys::SPA_PARAM_ROUTE_save, Value::Bool(true)),
        ],
    });
    PodSerializer::serialize(Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| AudioError::PipeWire(format!("cannot build the Route param: {e:?}")))
}

impl LoopState<PwConnector> {
    /// Drop everything the connection held once the daemon has gone away.
    fn forget_a_lost_connection(&mut self) {
        if self.connection.as_ref().is_some_and(PwConnection::is_lost) {
            self.on_disconnect();
        }
    }

    /// Refresh the mirror from the daemon.
    fn sync_mirror(&mut self) -> Result<(), AudioError> {
        let deadline = self.deadline;
        let mirror = self.connection()?.refresh(deadline)?;
        *self.mirror_mut() = mirror;
        Ok(())
    }

    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        self.sync_mirror()?;
        Ok(self.listed_sinks())
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        self.sync_mirror()?;
        Ok(self
            .modules_for(sink_name)
            .into_iter()
            .map(|(id, branch)| LoadedBranch {
                id,
                live: Some(branch_liveness(&self.mirror, id, sink_name, &branch.sink)),
                branch,
            })
            .collect())
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        let node = self.connection()?.create_null_sink(sink_name)?;
        self.set_null_sink(sink_name, node);
        self.sync_mirror()?;
        if !sink_names(&self.mirror)
            .iter()
            .any(|name| name == sink_name)
        {
            return Err(AudioError::PipeWire(format!(
                "the combined sink {sink_name} did not appear"
            )));
        }
        Ok(())
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        let args = loopback_module_args(sink_name, real_sink, latency_ms, self.next_module_id())?;
        let module = self.connection()?.load_loopback(&args)?;
        let id = self.add_module(
            sink_name,
            CombineBranch {
                sink: real_sink.to_string(),
                latency_ms,
            },
            module,
        );
        tracing::debug!("loaded loopback branch {id}: {args}");
        self.sync_mirror()
    }

    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        self.sync_mirror()?;
        if self.take_module(id).is_none() {
            return Err(AudioError::PipeWire(format!("no loopback branch {id}")));
        }
        let mirror = std::mem::take(&mut self.mirror);
        self.connection()?.unload(&mirror, id);
        self.mirror = mirror;
        self.sync_mirror()
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        // Dropping the proxy destroys the node this connection created.
        drop(self.take_null_sink(sink_name));
        for (id, _) in self.modules_for(sink_name) {
            self.take_module(id);
        }
        // A partial view destroys nothing: the sync must succeed first.
        self.sync_mirror()?;
        let doomed = foreign_combined_globals(&self.mirror, sink_name);
        tracing::debug!("teardown of {sink_name}: destroying globals {doomed:?}");
        let connection = self.connection()?;
        for id in doomed {
            connection.destroy_global(id);
        }
        self.sync_mirror()
    }

    fn set_default_sink(&mut self, sink: &str) -> Result<(), AudioError> {
        self.sync_mirror()?;
        let deadline = self.deadline;
        self.connection()?.set_default_sink(sink, deadline)
    }

    fn sink_volume(&mut self, sink: &str) -> Option<f32> {
        self.sync_mirror().ok()?;
        let target = route_target(&self.mirror, sink)?;
        let deadline = self.deadline;
        let (_, route) = self.connection().ok()?.route(target, deadline).ok()?;
        volume_fraction_from_route(&route.channel_volumes)
    }

    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
        self.sync_mirror()?;
        let target = route_target(&self.mirror, sink)
            .ok_or_else(|| AudioError::PipeWire(format!("no volume route for {sink}")))?;
        let deadline = self.deadline;
        let connection = self.connection()?;
        let (device, route) = connection.route(target, deadline)?;
        if route.channel_volumes.is_empty() {
            return Err(AudioError::PipeWire(format!(
                "the route of {sink} carries no channel volume"
            )));
        }
        let bytes = route_pod(
            &route,
            route_channel_volumes(level, route.channel_volumes.len()),
        )?;
        let pod = Pod::from_bytes(&bytes)
            .ok_or_else(|| AudioError::PipeWire("malformed Route param".into()))?;
        device.set_param(ParamType::Route, 0, pod);
        connection.roundtrip(deadline)
    }
}

/// Answer one command against the daemon.
fn handle(state: &mut LoopState<PwConnector>, command: Command) {
    state.deadline = Instant::now() + COMMAND_TIMEOUT;
    // A reply nobody waits for any more (the handle timed out) is dropped.
    match command {
        Command::Sinks { reply } => {
            let _ = reply.send(state.sinks());
        },
        Command::Branches { sink_name, reply } => {
            let _ = reply.send(state.branches(&sink_name));
        },
        Command::CreateCombinedSink { sink_name, reply } => {
            let _ = reply.send(state.create_combined_sink(&sink_name));
        },
        Command::LoadBranch {
            sink_name,
            real_sink,
            latency_ms,
            reply,
        } => {
            let _ = reply.send(state.load_branch(&sink_name, &real_sink, latency_ms));
        },
        Command::UnloadBranch { id, reply } => {
            let _ = reply.send(state.unload_branch(id));
        },
        Command::SetBranchDelay {
            id,
            delay_ms,
            reply,
        } => {
            // Red-phase stub.
            let _ = reply.send(Err(AudioError::PipeWire(format!(
                "set_branch_delay({id}, {delay_ms}) is not wired yet"
            ))));
        },
        Command::Teardown { sink_name, reply } => {
            let _ = reply.send(state.teardown(&sink_name));
        },
        Command::SetDefaultSink { sink, reply } => {
            let _ = reply.send(state.set_default_sink(&sink));
        },
        Command::SinkVolume { sink, reply } => {
            let _ = reply.send(state.sink_volume(&sink));
        },
        Command::SetSinkVolume { sink, level, reply } => {
            let _ = reply.send(state.set_sink_volume(&sink, level));
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::MAX_OFFSET_MS;
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
        Array(Vec<Spa>),
    }

    /// Split SPA-JSON into brackets and words. `=`, `:`, `,` and whitespace all
    /// separate; a quoted word loses its quotes, and a backslash inside one
    /// takes the next character literally. `None` for a quote left open.
    fn spa_tokens(text: &str) -> Option<Vec<String>> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut escaped = false;
        for c in text.chars() {
            if quoted {
                if escaped {
                    current.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quoted = false;
                    tokens.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                '{' | '}' | '[' | ']' => {
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
        if quoted {
            return None;
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        Some(tokens)
    }

    /// One value starting at `first`: a word, an object or an array.
    fn spa_value(first: String, tokens: &mut std::vec::IntoIter<String>) -> Option<Spa> {
        match first.as_str() {
            "{" => spa_object(tokens, true).map(Spa::Object),
            "[" => spa_array(tokens).map(Spa::Array),
            "}" | "]" => None,
            _ => Some(Spa::Word(first)),
        }
    }

    /// The key/value pairs of one object, up to its closing brace when
    /// `closed`, else up to the end. `None` for anything malformed: a missing
    /// or stray bracket, a key without a value, or a key given twice.
    fn spa_object(
        tokens: &mut std::vec::IntoIter<String>,
        closed: bool,
    ) -> Option<BTreeMap<String, Spa>> {
        let mut object = BTreeMap::new();
        loop {
            let Some(key) = tokens.next() else {
                return (!closed).then_some(object);
            };
            if key == "}" {
                return closed.then_some(object);
            }
            if matches!(key.as_str(), "{" | "[" | "]") {
                return None;
            }
            let value = spa_value(tokens.next()?, tokens)?;
            if object.insert(key, value).is_some() {
                return None;
            }
        }
    }

    /// The items of one array, up to its closing bracket.
    fn spa_array(tokens: &mut std::vec::IntoIter<String>) -> Option<Vec<Spa>> {
        let mut items = Vec::new();
        loop {
            let token = tokens.next()?;
            if token == "]" {
                return Some(items);
            }
            items.push(spa_value(token, tokens)?);
        }
    }

    /// Parse module arguments: exactly one object, its outer braces optional.
    /// `None` unless the whole text is that one well-formed object.
    fn parse_args(text: &str) -> Option<BTreeMap<String, Spa>> {
        let mut tokens = spa_tokens(text)?.into_iter();
        let braced = tokens.as_slice().first().map(String::as_str) == Some("{");
        if braced {
            tokens.next();
        }
        let object = spa_object(&mut tokens, braced)?;
        tokens.next().is_none().then_some(object)
    }

    fn word<'a>(object: &'a BTreeMap<String, Spa>, key: &str) -> Option<&'a str> {
        match object.get(key) {
            Some(Spa::Word(w)) => Some(w.as_str()),
            _ => None,
        }
    }

    fn section(args: &BTreeMap<String, Spa>, name: &str) -> BTreeMap<String, Spa> {
        match args.get(name) {
            Some(Spa::Object(o)) => o.clone(),
            _ => BTreeMap::new(),
        }
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
            ports: BTreeMap::new(),
            devices: BTreeMap::new(),
        }
    }

    fn sorted(mut ids: Vec<u32>) -> Vec<u32> {
        ids.sort_unstable();
        ids
    }

    // ─── delay_chain_module_args ─────────────────────────────────────────────

    /// The parsed arguments of branch `id` into `real_sink` at `delay_ms`,
    /// asserting on the way that they are accepted and are one well-formed
    /// SPA-JSON object.
    fn chain_args(real_sink: &str, delay_ms: u32, id: u32) -> BTreeMap<String, Spa> {
        let text = delay_chain_module_args(real_sink, delay_ms, id);
        assert!(text.is_ok(), "refused: {text:?}");
        let text = text.unwrap();
        let parsed = parse_args(&text);
        assert!(
            parsed.is_some(),
            "not one well-formed SPA-JSON object: {text:?}"
        );
        parsed.unwrap()
    }

    /// The node objects of the arguments' `filter.graph`.
    fn graph_nodes(args: &BTreeMap<String, Spa>) -> Vec<BTreeMap<String, Spa>> {
        match section(args, "filter.graph").get("nodes") {
            Some(Spa::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Spa::Object(object) => Some(object.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The one node of the filter graph, asserting that it is the only one.
    fn delay_node(args: &BTreeMap<String, Spa>) -> BTreeMap<String, Spa> {
        let nodes = graph_nodes(args);
        assert_eq!(nodes.len(), 1, "one node in the filter graph: {args:?}");
        nodes.into_iter().next().unwrap_or_default()
    }

    // Criterion: the filter graph is one builtin `delay` whose `"Delay (s)"`
    // control is `delay_ms / 1000` written with three decimals — 0 included,
    // which is a delay of zero and not "no branch", up to `MAX_OFFSET_MS`.
    #[test]
    fn test_delay_chain_module_args_carries_the_offset_in_seconds() {
        for (delay_ms, expected) in [(0, "0.000"), (120, "0.120"), (750, "0.750")] {
            let delay = delay_node(&chain_args(SPEAKER, delay_ms, 3));

            assert_eq!(word(&delay, "type"), Some("builtin"), "{delay_ms} ms");
            assert_eq!(word(&delay, "label"), Some("delay"), "{delay_ms} ms");
            assert_eq!(
                word(&section(&delay, "control"), "Delay (s)"),
                Some(expected),
                "{delay_ms} ms"
            );
        }
    }

    // Criterion: the capture side is never linked by the session manager —
    // `node.autoconnect = false` in `capture.props` — so only the server's two
    // monitor links feed it. On the capture side only: at module level it
    // would reach the playback side too, which then never reaches its speaker.
    #[test]
    fn test_delay_chain_module_args_leaves_the_capture_side_unconnected() {
        let args = chain_args(SPEAKER, 120, 3);

        assert_eq!(
            word(&section(&args, "capture.props"), "node.autoconnect"),
            Some("false")
        );
        assert_eq!(
            word(&section(&args, "playback.props"), "node.autoconnect"),
            None,
            "the playback side is linked to its target by the session manager"
        );
        assert_eq!(
            word(&args, "node.autoconnect"),
            None,
            "a module-level value would reach both sides"
        );
    }

    // Criterion: the playback side targets the resolved speaker node and never
    // reconnects — so a speaker that goes away never moves its branch onto
    // the PC's own speakers (#67). Both keys on the playback side only: the
    // capture side targets nothing, the server links it.
    #[test]
    fn test_delay_chain_module_args_pins_the_playback_side_to_the_speaker_without_reconnect() {
        let args = chain_args(SPEAKER, 120, 3);
        let playback = section(&args, "playback.props");
        let capture = section(&args, "capture.props");

        assert_eq!(word(&playback, "target.object"), Some(SPEAKER));
        assert_eq!(word(&playback, "node.dont-reconnect"), Some("true"));
        for key in ["target.object", "node.dont-reconnect"] {
            assert_eq!(word(&capture, key), None, "{key} on the capture side");
            assert_eq!(word(&args, key), None, "{key} at module level");
        }
    }

    // Criterion: both sides are named `blue2th_delay.<id>.in` / `.out` and each
    // carries `node.group = blue2th_delay.<id>` in its own props. The names are
    // what liveness, the port links and `set_param` look the branch up by.
    #[test]
    fn test_delay_chain_module_args_names_and_groups_both_sides_by_id() {
        for id in [7, 12] {
            let args = chain_args(SPEAKER, 120, id);
            let capture = section(&args, "capture.props");
            let playback = section(&args, "playback.props");
            let in_name = format!("blue2th_delay.{id}.in");
            let out_name = format!("blue2th_delay.{id}.out");
            let group = format!("blue2th_delay.{id}");

            assert_eq!(word(&capture, "node.name"), Some(in_name.as_str()));
            assert_eq!(word(&playback, "node.name"), Some(out_name.as_str()));
            assert_eq!(word(&capture, "node.group"), Some(group.as_str()));
            assert_eq!(word(&playback, "node.group"), Some(group.as_str()));
        }
    }

    // Criterion: the graph runs on `audio.channels = 2` over
    // `audio.position = [ FL FR ]`, the channels the monitor links are paired
    // by.
    #[test]
    fn test_delay_chain_module_args_runs_on_two_channels_fl_fr() {
        let args = chain_args(SPEAKER, 120, 3);

        assert_eq!(word(&args, "audio.channels"), Some("2"));
        assert_eq!(
            args.get("audio.position"),
            Some(&Spa::Array(vec![
                Spa::Word("FL".to_string()),
                Spa::Word("FR".to_string()),
            ]))
        );
    }

    // Criterion: the `delay` node is loaded with `"max-delay" = 1.0`
    // (`MAX_DELAY_SECONDS`) whatever the delay, and every accepted delay lies
    // within it — a `max-delay` derived from the delay itself could never be
    // retuned upwards.
    #[test]
    fn test_delay_chain_module_args_bounds_the_delay_at_max_delay() {
        for delay_ms in [0, 120, MAX_OFFSET_MS] {
            let delay = delay_node(&chain_args(SPEAKER, delay_ms, 3));
            let max =
                word(&section(&delay, "config"), "max-delay").and_then(|w| w.parse::<f32>().ok());
            let seconds =
                word(&section(&delay, "control"), "Delay (s)").and_then(|w| w.parse::<f32>().ok());

            assert_eq!(max, Some(MAX_DELAY_SECONDS), "{delay_ms} ms");
            assert!(
                seconds.is_some_and(|s| s <= MAX_DELAY_SECONDS),
                "{delay_ms} ms reads as {seconds:?} s"
            );
        }
    }

    // Criterion: the node and control the arguments declare are the ones the
    // `Props` pod addresses: the pod's `"delay:Delay (s)"` is
    // `<node name>:<control>`, so a node named anything else is never retuned.
    #[test]
    fn test_delay_chain_module_args_names_the_control_the_props_pod_sets() {
        let delay = delay_node(&chain_args(SPEAKER, 120, 3));
        let pod = delay_props_pod(0.25).unwrap_or_default();
        let param = match PodDeserializer::deserialize_any_from(&pod) {
            Ok((_, Value::Object(object))) => {
                object.properties.into_iter().find_map(|p| match p.value {
                    Value::Struct(fields) => fields.into_iter().find_map(|f| match f {
                        Value::String(name) => Some(name),
                        _ => None,
                    }),
                    _ => None,
                })
            },
            _ => None,
        };
        assert!(param.is_some(), "the pod names no param");
        let param = param.unwrap_or_default();
        let (node_name, control) = param.split_once(':').unwrap_or_default();

        assert_eq!(word(&delay, "name"), Some(node_name));
        assert!(
            section(&delay, "control").contains_key(control),
            "the node declares no control {control:?}: {delay:?}"
        );
    }

    // Criterion (non-nominal): an empty speaker is refused before anything
    // reaches the loop — an empty `target.object` lets the session manager
    // pick any node.
    #[test]
    fn test_delay_chain_module_args_refuses_an_empty_speaker() {
        assert!(matches!(
            delay_chain_module_args("", 120, 1),
            Err(AudioError::PipeWire(_))
        ));
        assert!(delay_chain_module_args(SPEAKER, 120, 1).is_ok());
    }

    // Criterion: a node name is written as one quoted SPA-JSON string whatever
    // it carries — a quote in it cannot close the string early and leave the
    // rest of the name to be read as another key.
    #[test]
    fn test_delay_chain_module_args_escapes_a_quote_in_a_node_name() {
        let args = chain_args("odd\"sink", 120, 1);

        assert_eq!(
            word(&section(&args, "playback.props"), "target.object"),
            Some("odd\"sink")
        );
    }

    // Criterion: `MAX_DELAY_SECONDS` covers the largest offset the selection
    // accepts, so no accepted offset is ever beyond what a branch can delay.
    #[test]
    fn test_max_delay_covers_the_largest_accepted_offset() {
        assert!(MAX_DELAY_SECONDS >= MAX_OFFSET_MS as f32 / 1000.0);
    }

    // ─── delay_props_pod ─────────────────────────────────────────────────────

    // Criterion: the pod `set_branch_delay` writes decodes back to a `Props`
    // object carrying `params = [ "delay:Delay (s)", <seconds> ]` as a struct
    // of a String and a Float — the shape `pw-cli set-param … Props` wrote in
    // spike #110 (2026-09-10), which read back in `pw-dump` as
    // `['delay:Delay (s)', 0.25]`.
    #[test]
    fn test_delay_props_pod_decodes_back_to_the_delay_param() {
        for seconds in [0.0_f32, 0.12, 0.25, 0.75] {
            let bytes = delay_props_pod(seconds);
            assert!(bytes.is_ok(), "{seconds} s: {bytes:?}");
            let bytes = bytes.unwrap_or_default();
            assert!(Pod::from_bytes(&bytes).is_some(), "{seconds} s: no pod");
            let object = match PodDeserializer::deserialize_any_from(&bytes) {
                Ok((_, Value::Object(object))) => Some(object),
                _ => None,
            };
            assert!(object.is_some(), "{seconds} s: not an object");
            let object = object.unwrap();

            assert_eq!(
                (object.type_, object.id),
                (
                    libspa::sys::SPA_TYPE_OBJECT_Props,
                    libspa::sys::SPA_PARAM_Props
                ),
                "{seconds} s"
            );
            let properties: Vec<(u32, Value)> = object
                .properties
                .into_iter()
                .map(|p| (p.key, p.value))
                .collect();
            assert_eq!(
                properties,
                vec![(
                    libspa::sys::SPA_PROP_params,
                    Value::Struct(vec![
                        Value::String("delay:Delay (s)".to_string()),
                        Value::Float(seconds),
                    ])
                )],
                "{seconds} s"
            );
        }
    }

    // ─── combined_sink_props ─────────────────────────────────────────────────

    // Criterion: the combined sink is an `adapter` over `support.null-audio-sink`,
    // `media.class = Audio/Sink`, `audio.position = FL,FR`, named `sink_name`,
    // with `monitor.channel-volumes = true`: the monitor the branches capture
    // applies the sink's volume, so a volume set on the combined sink reaches
    // every speaker.
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
        assert_eq!(
            props.get("monitor.channel-volumes").map(String::as_str),
            Some("true")
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
                        ("node.name", "blue2th_delay.1.out"),
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

    const OTHER_SPEAKER: &str = "bluez_output.11_22_33_44_55_66.1";

    /// The monitor links feeding branch 1 (FL, FR), and the links carrying its
    /// output into the speaker (FL, FR), as node-level `(id, from, to)`.
    const MONITOR_INTO_1: [(u32, u32, u32); 2] = [(200, 61, 90), (201, 61, 90)];
    const BRANCH_1_INTO_SPEAKER: [(u32, u32, u32); 2] = [(202, 91, 57), (203, 91, 57)];

    /// The combined sink and a namesake opening with its name, two speakers,
    /// and both sides of branches 1 and 10 — whose names and group open with
    /// branch 1's group, `blue2th_delay.1`. Links as the test asks.
    fn liveness_mirror(links: &[(u32, u32, u32)]) -> Mirror {
        mirror_of(
            &[
                (
                    57,
                    node(&[("node.name", SPEAKER), ("media.class", "Audio/Sink")]),
                ),
                (
                    58,
                    node(&[("node.name", OTHER_SPEAKER), ("media.class", "Audio/Sink")]),
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
                    90,
                    node(&[
                        ("node.name", "blue2th_delay.1.in"),
                        ("media.class", "Stream/Input/Audio"),
                        ("node.group", "blue2th_delay.1"),
                    ]),
                ),
                (
                    91,
                    node(&[
                        ("node.name", "blue2th_delay.1.out"),
                        ("media.class", "Stream/Output/Audio"),
                        ("node.group", "blue2th_delay.1"),
                    ]),
                ),
                (
                    92,
                    node(&[
                        ("node.name", "blue2th_delay.10.in"),
                        ("media.class", "Stream/Input/Audio"),
                        ("node.group", "blue2th_delay.10"),
                    ]),
                ),
                (
                    93,
                    node(&[
                        ("node.name", "blue2th_delay.10.out"),
                        ("media.class", "Stream/Output/Audio"),
                        ("node.group", "blue2th_delay.10"),
                    ]),
                ),
            ],
            links,
        )
    }

    // Criterion: a branch is live only when **both** hold — the combined sink
    // feeds its `.in` node and its `.out` node feeds the speaker. One link on
    // each side is enough; either side alone is dead.
    #[test]
    fn test_branch_liveness_is_live_only_with_both_links() {
        let both = [MONITOR_INTO_1, BRANCH_1_INTO_SPEAKER].concat();
        assert!(
            branch_liveness(&liveness_mirror(&both), 1, COMBINED, SPEAKER),
            "fed by the monitor and feeding the speaker: live"
        );

        let one_each = [(200, 61, 90), (202, 91, 57)];
        assert!(branch_liveness(
            &liveness_mirror(&one_each),
            1,
            COMBINED,
            SPEAKER
        ));

        assert!(
            !branch_liveness(
                &liveness_mirror(&BRANCH_1_INTO_SPEAKER),
                1,
                COMBINED,
                SPEAKER
            ),
            "feeding the speaker but fed by nothing: dead"
        );
        assert!(
            !branch_liveness(&liveness_mirror(&MONITOR_INTO_1), 1, COMBINED, SPEAKER),
            "fed but feeding no speaker: dead"
        );
        assert!(!branch_liveness(
            &liveness_mirror(&[]),
            1,
            COMBINED,
            SPEAKER
        ));
    }

    // Criterion (guard, both sides): a branch linked to its speaker but not fed
    // by the node named exactly `sink_name` is dead. The near misses: its `.in`
    // is fed by `blue2th_combined_old`, which opens with the combined sink's
    // name, and the combined sink feeds `blue2th_delay.10.in`, which opens with
    // `blue2th_delay.1.in`'s stem.
    #[test]
    fn test_branch_liveness_without_the_monitor_link_is_dead() {
        let links = [
            BRANCH_1_INTO_SPEAKER.to_vec(),
            vec![(204, 62, 90), (205, 61, 92)],
        ]
        .concat();

        assert!(!branch_liveness(
            &liveness_mirror(&links),
            1,
            COMBINED,
            SPEAKER
        ));
    }

    // Criterion (guard, both sides): a branch fed by the monitor but not linked
    // into the node named exactly `real_sink` is dead. The near misses: its
    // `.out` feeds the other speaker, and `blue2th_delay.10.out` — whose name
    // and group open with `blue2th_delay.1` — feeds this one.
    #[test]
    fn test_branch_liveness_without_the_speaker_link_is_dead() {
        let links = [MONITOR_INTO_1.to_vec(), vec![(206, 91, 58), (207, 93, 57)]].concat();

        assert!(!branch_liveness(
            &liveness_mirror(&links),
            1,
            COMBINED,
            SPEAKER
        ));
    }

    // Criterion: a branch whose nodes are missing from the mirror is dead —
    // an id with no node at all, and a branch whose `.in` node is gone while a
    // stale link still names its id.
    #[test]
    fn test_branch_liveness_of_a_missing_node_is_dead() {
        let both = [MONITOR_INTO_1, BRANCH_1_INTO_SPEAKER].concat();
        let mirror = liveness_mirror(&both);
        assert!(
            !branch_liveness(&mirror, 4, COMBINED, SPEAKER),
            "no branch 4"
        );

        let mut without_in = mirror;
        without_in.nodes.remove(&90);
        assert!(
            !branch_liveness(&without_in, 1, COMBINED, SPEAKER),
            "branch 1 has lost its capture side"
        );
    }

    // Criterion (guard, exact names): branch 1's liveness is not satisfied by
    // links on `blue2th_delay.10.in` / `.10.out`, which a `starts_with` match
    // would take — while those very links do make branch 10 live.
    #[test]
    fn test_branch_liveness_does_not_take_branch_10_for_branch_1() {
        let links = [(210, 61, 92), (211, 93, 57)];
        let mirror = liveness_mirror(&links);

        assert!(
            branch_liveness(&mirror, 10, COMBINED, SPEAKER),
            "branch 10 is live"
        );
        assert!(
            !branch_liveness(&mirror, 1, COMBINED, SPEAKER),
            "branch 1 has no link of its own"
        );
    }

    // Criterion (guard, the empty value is a wildcard): an empty name matches
    // no node, not even one whose `node.name` is itself empty. A nameless node
    // feeding the `.in` side does not make `branch_liveness(…, "", …)` live,
    // and a link into a nameless sink feeds no named speaker.
    #[test]
    fn test_branch_liveness_of_an_empty_name_ignores_a_nameless_node() {
        let links = [
            MONITOR_INTO_1.to_vec(),
            BRANCH_1_INTO_SPEAKER.to_vec(),
            vec![(220, 95, 90), (221, 91, 96)],
        ]
        .concat();
        let mut mirror = liveness_mirror(&links);
        mirror.nodes.insert(
            95,
            node(&[("node.name", ""), ("media.class", "Audio/Sink")]),
        );
        mirror.nodes.insert(
            96,
            node(&[("node.name", ""), ("media.class", "Audio/Sink")]),
        );
        assert!(
            branch_liveness(&mirror, 1, COMBINED, SPEAKER),
            "the same branch is live under its real names"
        );

        assert!(
            !branch_liveness(&mirror, 1, "", SPEAKER),
            "a nameless node feeding the branch is no combined sink"
        );
        assert!(
            !branch_liveness(&mirror, 1, COMBINED, ""),
            "a link into a nameless sink feeds no named speaker"
        );
    }

    // ─── channel_port_pairs ──────────────────────────────────────────────────

    /// A port as the registry announces it; the keys are the ones the #110
    /// spike's `pw-probe list` reads (`node.id`, `port.direction`,
    /// `port.name`, `audio.channel`). `None` leaves the channel out.
    fn port(node: u32, direction: &str, name: &str, channel: Option<&str>) -> PortEntry {
        let node = node.to_string();
        let mut props = vec![
            ("node.id", node.as_str()),
            ("port.direction", direction),
            ("port.name", name),
        ];
        if let Some(channel) = channel {
            props.push(("audio.channel", channel));
        }
        PortEntry {
            props: props
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// The combined sink with its playback inputs and its monitor outputs —
    /// listed FR before FL — and the capture sides of branch 30 and branch 3,
    /// branch 30's inputs listed first. Synthetic, built on the port keys of
    /// the spike's registry reader.
    fn ports_mirror() -> Mirror {
        let mut mirror = mirror_of(
            &[
                (
                    61,
                    node(&[("node.name", COMBINED), ("media.class", "Audio/Sink")]),
                ),
                (
                    90,
                    node(&[
                        ("node.name", "blue2th_delay.3.in"),
                        ("media.class", "Stream/Input/Audio"),
                    ]),
                ),
                (
                    92,
                    node(&[
                        ("node.name", "blue2th_delay.30.in"),
                        ("media.class", "Stream/Input/Audio"),
                    ]),
                ),
            ],
            &[],
        );
        mirror.ports = [
            (110, port(61, "in", "playback_FL", Some("FL"))),
            (111, port(61, "in", "playback_FR", Some("FR"))),
            (112, port(61, "out", "monitor_FR", Some("FR"))),
            (113, port(61, "out", "monitor_FL", Some("FL"))),
            (120, port(92, "in", "input_FL", Some("FL"))),
            (121, port(92, "in", "input_FR", Some("FR"))),
            (130, port(90, "in", "input_FL", Some("FL"))),
            (131, port(90, "in", "input_FR", Some("FR"))),
        ]
        .into_iter()
        .collect();
        mirror
    }

    fn pair_set(pairs: Vec<(u32, u32)>) -> BTreeSet<(u32, u32)> {
        let count = pairs.len();
        let set: BTreeSet<(u32, u32)> = pairs.into_iter().collect();
        assert_eq!(set.len(), count, "a pair listed twice");
        set
    }

    /// Branch 3's monitor links: `monitor_FL → input_FL`, `monitor_FR → input_FR`.
    fn branch_3_pairs() -> BTreeSet<(u32, u32)> {
        [(113, 130), (112, 131)].into_iter().collect()
    }

    // Criterion (guard, port pairing by channel): FL pairs with FL and FR with
    // FR, taking the out node's outputs and the in node's inputs. The near
    // misses: the monitor lists FR before FL while the input lists FL first, so
    // a pairing by list index crosses the channels; and the combined sink's
    // own inputs carry the same channels, so a pairing ignoring the direction
    // links them too.
    #[test]
    fn test_channel_port_pairs_pairs_fl_with_fl_and_fr_with_fr() {
        let pairs = channel_port_pairs(&ports_mirror(), COMBINED, "blue2th_delay.3.in");

        assert_eq!(pair_set(pairs), branch_3_pairs());
    }

    // Criterion: only the ports of the two named nodes are paired. The near
    // miss: `blue2th_delay.30.in` opens with `blue2th_delay.3`, and its inputs
    // are listed before branch 3's with the same direction and channels.
    #[test]
    fn test_channel_port_pairs_ignores_a_port_of_another_node() {
        let pairs = channel_port_pairs(&ports_mirror(), COMBINED, "blue2th_delay.3.in");
        let pairs = pair_set(pairs);

        assert!(
            !pairs.iter().any(|(_, input)| [120, 121].contains(input)),
            "linked into branch 30: {pairs:?}"
        );
        assert_eq!(pairs, branch_3_pairs());
    }

    // Criterion: a node missing from the mirror pairs nothing, on either end,
    // and an empty name is a missing node — even beside a nameless node whose
    // ports would otherwise pair.
    #[test]
    fn test_channel_port_pairs_of_a_missing_node_is_empty() {
        let mut mirror = ports_mirror();
        mirror.nodes.insert(
            99,
            node(&[("node.name", ""), ("media.class", "Stream/Input/Audio")]),
        );
        mirror
            .ports
            .insert(140, port(99, "in", "input_FL", Some("FL")));
        mirror
            .ports
            .insert(141, port(99, "in", "input_FR", Some("FR")));
        assert_eq!(
            pair_set(channel_port_pairs(&mirror, COMBINED, "blue2th_delay.3.in")),
            branch_3_pairs(),
            "the fixture pairs under the real names"
        );

        assert!(channel_port_pairs(&mirror, COMBINED, "blue2th_delay.4.in").is_empty());
        assert!(channel_port_pairs(&mirror, "blue2th_absent", "blue2th_delay.3.in").is_empty());
        assert!(
            channel_port_pairs(&mirror, COMBINED, "").is_empty(),
            "an empty name pairs nothing, not the nameless node"
        );
    }

    // Criterion (the empty value is a wildcard): two ports without a channel
    // are not the same channel — neither two absent `audio.channel`s nor two
    // empty ones pair.
    #[test]
    fn test_channel_port_pairs_never_pairs_two_ports_without_a_channel() {
        let mut mirror = ports_mirror();
        mirror.ports.insert(114, port(61, "out", "control", None));
        mirror
            .ports
            .insert(115, port(61, "out", "monitor_AUX", Some("")));
        mirror.ports.insert(132, port(90, "in", "control", None));
        mirror
            .ports
            .insert(133, port(90, "in", "input_AUX", Some("")));

        let pairs = channel_port_pairs(&mirror, COMBINED, "blue2th_delay.3.in");

        assert_eq!(pair_set(pairs), branch_3_pairs());
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

    // Criterion: a hardware node is spared even when it would otherwise match —
    // named exactly like the sink, or sharing a pair's link group — since
    // destroying it switches its card's profile to `off` (the 2026-09-19
    // session).
    #[test]
    fn test_foreign_combined_globals_spares_a_hardware_node_that_would_match() {
        let mut mirror = foreign_mirror();
        mirror.nodes.insert(
            90,
            node(&[
                ("node.name", COMBINED),
                ("media.class", "Audio/Sink"),
                ("device.api", "alsa"),
            ]),
        );
        mirror.nodes.insert(
            91,
            node(&[
                ("node.name", "alsa_output.usb-dac.analog-stereo"),
                ("media.class", "Audio/Sink"),
                ("device.api", "alsa"),
                ("node.link-group", "loopback-6815-13"),
            ]),
        );

        assert_eq!(
            sorted(foreign_combined_globals(&mirror, COMBINED)),
            vec![61, 70, 71],
            "no hardware node joins the teardown"
        );
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

    // Criterion (the empty value is a wildcard): a capture stream of the sink
    // carrying an empty `node.link-group` is taken alone — its empty group
    // pairs it with no other node, not with every node whose group is empty.
    #[test]
    fn test_foreign_combined_globals_of_an_empty_link_group_pairs_nothing() {
        let mut mirror = foreign_mirror();
        mirror.nodes.insert(
            82,
            node(&[
                ("node.name", "input.loopback-6815-18"),
                ("media.class", "Stream/Input/Audio"),
                ("target.object", COMBINED),
                ("stream.capture.sink", "true"),
                ("node.link-group", ""),
            ]),
        );
        mirror.nodes.insert(
            83,
            node(&[
                ("node.name", "firefox"),
                ("media.class", "Stream/Output/Audio"),
                ("target.object", SPEAKER),
                ("node.link-group", ""),
            ]),
        );

        assert_eq!(
            sorted(foreign_combined_globals(&mirror, COMBINED)),
            vec![61, 70, 71, 82],
            "the capture stream is ours, the other groupless stream is not"
        );
    }

    // Criterion: a capture stream of the sink is recognised by either marker —
    // `media.class = Stream/Input/Audio` or `stream.capture.sink = true` — and
    // takes its pair along in both cases.
    #[test]
    fn test_foreign_combined_globals_recognises_a_capture_by_either_marker() {
        let mut mirror = foreign_mirror();
        for (capture, playback, group, marker) in [
            (
                84,
                85,
                "loopback-6815-19",
                ("media.class", "Stream/Input/Audio"),
            ),
            (86, 87, "loopback-6815-20", ("stream.capture.sink", "true")),
        ] {
            mirror.nodes.insert(
                capture,
                node(&[
                    ("node.name", "input.loopback"),
                    marker,
                    ("target.object", COMBINED),
                    ("node.link-group", group),
                ]),
            );
            mirror.nodes.insert(
                playback,
                node(&[
                    ("node.name", "output.loopback"),
                    ("media.class", "Stream/Output/Audio"),
                    ("target.object", SPEAKER),
                    ("node.link-group", group),
                ]),
            );
        }

        assert_eq!(
            sorted(foreign_combined_globals(&mirror, COMBINED)),
            vec![61, 70, 71, 84, 85, 86, 87]
        );
    }

    // ─── default_sink_metadata_value ─────────────────────────────────────────

    // Criterion: `set_default_sink` writes `{"name": "<sink>"}` on
    // `default.configured.audio.sink`, the JSON the session manager reads the
    // configured default sink from.
    #[test]
    fn test_default_sink_metadata_value_is_a_json_object_naming_the_sink() {
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

    // Criterion: an over-amplified route reads above 1.0, unclamped — it is
    // `reported_volume` that refuses a level the DTO cannot carry, and a clamp
    // here would present 100% for a speaker that is not at 100%.
    #[test]
    fn test_volume_fraction_from_route_reports_an_over_amplified_route_above_one() {
        let read = volume_fraction_from_route(&[1.53_f32.powi(3)]);

        assert!(
            read.is_some_and(|v| close(v, 1.53, 1e-4)),
            "153% reads 1.53, got {read:?}"
        );
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
    // has no Route, and an empty name resolves nothing — not even a nameless
    // node that carries a Route.
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
                (
                    62,
                    node(&[
                        ("node.name", ""),
                        ("media.class", "Audio/Sink"),
                        ("device.id", "78"),
                        ("card.profile.device", "2"),
                    ]),
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

    // ─── The Route param: real pods from a live daemon ───────────────────────

    /// `Route` pods captured from real devices; the file header says how.
    const ROUTE_FIXTURE: &str = include_str!("../tests/fixtures/pipewire_route_params.txt");

    /// The bytes of the fixture pod labelled `label`.
    fn fixture_pod(label: &str) -> Vec<u8> {
        let hex = ROUTE_FIXTURE
            .lines()
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| line.strip_prefix(label)?.strip_prefix(' '));
        assert!(hex.is_some(), "no fixture pod labelled {label}");
        let hex = hex.unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    fn parse(bytes: &[u8]) -> Option<Route> {
        parse_route(Pod::from_bytes(bytes).unwrap())
    }

    fn fixture_route(label: &str) -> Route {
        let route = parse(&fixture_pod(label));
        assert!(route.is_some(), "{label} parses to no route");
        route.unwrap()
    }

    /// A `Route` object carrying only `properties`, as serialized bytes.
    fn route_bytes(properties: Vec<Property>) -> Vec<u8> {
        let value = Value::Object(Object {
            type_: libspa::sys::SPA_TYPE_OBJECT_ParamRoute,
            id: libspa::sys::SPA_PARAM_Route,
            properties,
        });
        PodSerializer::serialize(Cursor::new(Vec::new()), &value)
            .unwrap()
            .0
            .into_inner()
    }

    fn int_property(key: u32, value: i32) -> Property {
        Property {
            key,
            flags: PropertyFlags::empty(),
            value: Value::Int(value),
        }
    }

    // Criterion: a real Bluetooth speaker's route reads its index, its device
    // and its channel volumes, which stand for the level `wpctl` showed (0.13).
    #[test]
    fn test_parse_route_reads_a_real_bluetooth_speaker_route() {
        let route = fixture_route("jbl_xtreme_3.speaker-output");

        assert_eq!((route.index, route.device), (1, 1));
        assert_eq!(route.channel_volumes, vec![0.002197, 0.002197]);
        let level = volume_fraction_from_route(&route.channel_volumes);
        assert!(level.is_some_and(|v| close(v, 0.13, 1e-3)), "got {level:?}");
    }

    // Criterion: a headset exposes an input and an output route; each reads
    // under its own device, which is what `route` matches the sink's
    // `card.profile.device` against.
    #[test]
    fn test_parse_route_reads_each_route_of_a_headset_under_its_own_device() {
        let input = fixture_route("sony_wh_1000xm5.headset-input");
        let output = fixture_route("sony_wh_1000xm5.headset-output");

        assert_eq!((input.index, input.device), (0, 0));
        assert_eq!(input.channel_volumes, vec![1.0]);
        assert_eq!((output.index, output.device), (1, 1));
        let level = volume_fraction_from_route(&output.channel_volumes);
        assert!(level.is_some_and(|v| close(v, 0.37, 1e-3)), "got {level:?}");
    }

    // Criterion: an ALSA route carries `softVolumes` next to `channelVolumes`;
    // the volume is read from `channelVolumes` (0.34 in `wpctl`), never from
    // the soft ones (0.959). The card's input route reads under its own index
    // and device.
    #[test]
    fn test_parse_route_reads_the_channel_volumes_of_an_alsa_route_not_the_soft_ones() {
        let speaker = fixture_route("ryzen_hd_audio.out-speaker");
        let mic = fixture_route("ryzen_hd_audio.in-mic1");

        assert_eq!((speaker.index, speaker.device), (0, 0));
        assert_eq!(speaker.channel_volumes, vec![0.03930273, 0.03930273]);
        let level = volume_fraction_from_route(&speaker.channel_volumes);
        assert!(level.is_some_and(|v| close(v, 0.34, 1e-3)), "got {level:?}");
        assert_eq!((mic.index, mic.device), (2, 2));
    }

    // Criterion: the pod `set_sink_volume` writes reads back as the route it
    // was built from, with the new volumes on every channel.
    #[test]
    fn test_route_pod_round_trips_through_parse_route() {
        let real = fixture_route("jbl_xtreme_3.speaker-output");
        let volumes = route_channel_volumes(0.5, real.channel_volumes.len());

        let written = parse(&route_pod(&real, volumes.clone()).unwrap()).unwrap();

        assert_eq!((written.index, written.device), (real.index, real.device));
        assert_eq!(written.channel_volumes, volumes);
    }

    // Criterion: the pod `set_sink_volume` writes has the shape of the one the
    // daemon sends — the same object type and id outside, the same props
    // object inside — and asks the daemon to keep the volume (`save = true`).
    #[test]
    fn test_route_pod_has_the_shape_of_a_real_route_and_is_saved() {
        let decode = |bytes: &[u8]| match PodDeserializer::deserialize_any_from(bytes) {
            Ok((_, Value::Object(object))) => Some(object),
            _ => None,
        };
        let props_of = |object: &Object| {
            object.properties.iter().find_map(|p| match &p.value {
                Value::Object(props) if p.key == libspa::sys::SPA_PARAM_ROUTE_props => {
                    Some((props.type_, props.id))
                },
                _ => None,
            })
        };
        let real = decode(&fixture_pod("jbl_xtreme_3.speaker-output")).unwrap();
        let route = fixture_route("jbl_xtreme_3.speaker-output");
        let written = decode(&route_pod(&route, vec![0.1, 0.1]).unwrap()).unwrap();

        assert_eq!((written.type_, written.id), (real.type_, real.id));
        assert!(props_of(&real).is_some(), "the real route carries props");
        assert_eq!(props_of(&written), props_of(&real));
        let save = written
            .properties
            .iter()
            .find(|p| p.key == libspa::sys::SPA_PARAM_ROUTE_save)
            .map(|p| &p.value);
        assert_eq!(save, Some(&Value::Bool(true)));
    }

    // Criterion: the index and the device are two fields, read and written each
    // under its own key. Every captured route has them equal, so only a route
    // where they differ can tell them apart.
    #[test]
    fn test_route_index_and_device_are_not_confused() {
        let parsed = parse(&route_bytes(vec![
            int_property(libspa::sys::SPA_PARAM_ROUTE_index, 3),
            int_property(libspa::sys::SPA_PARAM_ROUTE_device, 7),
        ]))
        .unwrap();
        assert_eq!((parsed.index, parsed.device), (3, 7), "read");

        let written =
            PodDeserializer::deserialize_any_from(&route_pod(&parsed, vec![0.5]).unwrap())
                .ok()
                .and_then(|(_, value)| match value {
                    Value::Object(object) => Some(object),
                    _ => None,
                })
                .unwrap();
        let int_at = |key| {
            written.properties.iter().find_map(|p| match p.value {
                Value::Int(v) if p.key == key => Some(v),
                _ => None,
            })
        };
        assert_eq!(
            (
                int_at(libspa::sys::SPA_PARAM_ROUTE_index),
                int_at(libspa::sys::SPA_PARAM_ROUTE_device)
            ),
            (Some(3), Some(7)),
            "written"
        );
    }

    // Criterion: a route missing its index or its device is no route — `route`
    // could not address it, nor `route_pod` write it back.
    #[test]
    fn test_parse_route_without_index_or_device_is_none() {
        let index = int_property(libspa::sys::SPA_PARAM_ROUTE_index, 1);
        let device = int_property(libspa::sys::SPA_PARAM_ROUTE_device, 1);

        assert!(parse(&route_bytes(vec![index.clone(), device.clone()])).is_some());
        assert!(parse(&route_bytes(vec![device])).is_none(), "no index");
        assert!(parse(&route_bytes(vec![index])).is_none(), "no device");
    }

    // Criterion: a route without a props object reads with no channel volume
    // — which `set_sink_volume` refuses and `sink_volume` reads as unknown —
    // rather than failing to parse.
    #[test]
    fn test_parse_route_without_props_has_no_channel_volume() {
        let route = parse(&route_bytes(vec![
            int_property(libspa::sys::SPA_PARAM_ROUTE_index, 1),
            int_property(libspa::sys::SPA_PARAM_ROUTE_device, 1),
        ]))
        .unwrap();

        assert!(route.channel_volumes.is_empty());
        assert_eq!(volume_fraction_from_route(&route.channel_volumes), None);
    }

    // ─── The handle: command layer over a fake loop thread ───────────────────

    impl LoopSender for mpsc::Sender<Command> {
        fn send(&self, command: Command) -> Result<(), Command> {
            mpsc::Sender::send(self, command).map_err(|e| e.0)
        }
    }

    /// A loop thread that answers every command as a healthy graph holding
    /// `sinks` would, recording each command it received: its name, then its
    /// arguments in declaration order.
    fn answering_loop(
        sinks: Vec<String>,
        received: Arc<Mutex<Vec<String>>>,
    ) -> Box<dyn LoopSender> {
        let (tx, rx) = mpsc::channel::<Command>();
        std::thread::spawn(move || {
            for command in rx {
                let mut log = received.lock().unwrap();
                match command {
                    Command::Sinks { reply } => {
                        log.push("sinks".to_string());
                        // Cloned: every `sinks` command is answered with the list.
                        let _ = reply.send(Ok(sinks.clone()));
                    },
                    Command::Branches { sink_name, reply } => {
                        log.push(format!("branches {sink_name}"));
                        let _ = reply.send(Ok(Vec::new()));
                    },
                    Command::CreateCombinedSink { sink_name, reply } => {
                        log.push(format!("create_combined_sink {sink_name}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::LoadBranch {
                        sink_name,
                        real_sink,
                        latency_ms,
                        reply,
                    } => {
                        log.push(format!("load_branch {sink_name} {real_sink} {latency_ms}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::UnloadBranch { id, reply } => {
                        log.push(format!("unload_branch {id}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::SetBranchDelay {
                        id,
                        delay_ms,
                        reply,
                    } => {
                        log.push(format!("set_branch_delay {id} {delay_ms}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::Teardown { sink_name, reply } => {
                        log.push(format!("teardown {sink_name}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::SetDefaultSink { sink, reply } => {
                        log.push(format!("set_default_sink {sink}"));
                        let _ = reply.send(Ok(()));
                    },
                    Command::SinkVolume { sink, reply } => {
                        log.push(format!("sink_volume {sink}"));
                        let _ = reply.send(Some(0.5));
                    },
                    Command::SetSinkVolume { sink, level, reply } => {
                        log.push(format!("set_sink_volume {sink} {level}"));
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
        assert_eq!(
            *received.lock().unwrap(),
            vec!["sinks".to_string(), format!("sink_volume {SPEAKER}")]
        );
    }

    // Criterion: each method sends its own command, carrying its arguments in
    // their own fields — `load_branch` takes two sink names side by side, and a
    // swap would capture the speaker and play into the combined sink;
    // `set_branch_delay` takes two numbers side by side, given distinct values
    // here so a swap shows.
    #[test]
    fn test_every_method_sends_its_own_command_with_its_arguments() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let mut graph = PipeWireGraph::with_loop(Box::new(move || {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert!(graph.sinks().is_ok());
        assert!(graph.branches(COMBINED).is_ok());
        assert!(graph.create_combined_sink(COMBINED).is_ok());
        assert!(graph.load_branch(COMBINED, SPEAKER, 170).is_ok());
        assert!(graph.unload_branch(7).is_ok());
        assert!(graph.set_branch_delay(9, 250).is_ok());
        assert!(graph.teardown(COMBINED).is_ok());
        assert!(graph.set_default_sink(SPEAKER).is_ok());
        assert_eq!(graph.sink_volume(SPEAKER), Some(0.5));
        assert!(graph.set_sink_volume(SPEAKER, 0.25).is_ok());

        assert_eq!(
            *received.lock().unwrap(),
            vec![
                "sinks".to_string(),
                format!("branches {COMBINED}"),
                format!("create_combined_sink {COMBINED}"),
                format!("load_branch {COMBINED} {SPEAKER} 170"),
                "unload_branch 7".to_string(),
                "set_branch_delay 9 250".to_string(),
                format!("teardown {COMBINED}"),
                format!("set_default_sink {SPEAKER}"),
                format!("sink_volume {SPEAKER}"),
                format!("set_sink_volume {SPEAKER} 0.25"),
            ]
        );
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

    // Criterion: the loop thread's own waits fit inside the handle's, so a slow
    // daemon is reported with its own error rather than the handle's timeout.
    // The budget covers every round trip of a command together, so this holds
    // however many round trips a command makes.
    #[test]
    fn test_a_command_s_round_trips_fit_in_the_reply_timeout() {
        assert!(COMMAND_TIMEOUT < GRAPH_REPLY_TIMEOUT);
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

    // Criterion: a thread that has died (its receiver dropped) is replaced by the
    // call that finds it dead, and that very call is answered by the new thread;
    // the next call reuses the new thread.
    #[test]
    fn test_a_dead_loop_thread_is_replaced_by_one_answering_the_same_command() {
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

        let first = graph.sinks();
        assert_eq!(first.ok(), Some(vec![SPEAKER.to_string()]));
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            2,
            "the dead thread and its replacement"
        );

        let second = graph.sinks();
        assert_eq!(second.ok(), Some(vec![SPEAKER.to_string()]));
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            2,
            "a live thread is not replaced"
        );
        assert_eq!(*received.lock().unwrap(), vec!["sinks", "sinks"]);
    }

    // Criterion (non-nominal): an empty sink name or target sink is refused
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

    // Criterion: `PipeWireGraph::spawn()` starts no loop thread until the first
    // command, so constructing it in a test touches no daemon.
    #[test]
    fn test_spawn_starts_no_loop_thread_before_the_first_command() {
        let graph = PipeWireGraph::spawn();

        assert!(graph.sender.is_none(), "no loop thread was started");
    }

    // Criterion: a detached graph reaches no loop and no daemon — every command
    // errs at once, and a volume reads as unknown.
    #[test]
    fn test_detached_graph_errs_at_once_without_a_loop() {
        let mut graph = PipeWireGraph::detached();
        let started = Instant::now();

        let sinks = graph.sinks();
        let teardown = graph.teardown(COMBINED);
        let retune = graph.set_branch_delay(1, 120);

        assert!(
            matches!(&sinks, Err(AudioError::PipeWire(m)) if m.contains("not running")),
            "got {sinks:?}"
        );
        assert!(matches!(teardown, Err(AudioError::PipeWire(_))));
        assert!(
            matches!(retune, Err(AudioError::PipeWire(_))),
            "a retune goes through the loop like every command, got {retune:?}"
        );
        assert_eq!(graph.sink_volume(SPEAKER), None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no timeout is waited out, waited {:?}",
            started.elapsed()
        );
    }

    // ─── The loop side: connection lifecycle over a fake connector ───────────

    /// A connector that counts its attempts and fails the first `failures`,
    /// and counts the contexts it creates.
    struct FakeConnector {
        attempts: Arc<AtomicUsize>,
        contexts: Arc<AtomicUsize>,
        failures: usize,
    }

    impl Connector for FakeConnector {
        type Context = usize;
        type Connection = usize;
        type Module = &'static str;
        type NullSink = &'static str;

        fn context(&mut self) -> Result<usize, AudioError> {
            Ok(self.contexts.fetch_add(1, Ordering::SeqCst))
        }

        fn connect(&mut self, _context: &usize) -> Result<usize, AudioError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.failures {
                Err(AudioError::PipeWire("no daemon".to_string()))
            } else {
                Ok(attempt)
            }
        }
    }

    fn fake_state(failures: usize) -> (LoopState<FakeConnector>, Arc<AtomicUsize>) {
        let (state, attempts, _) = fake_state_counting_contexts(failures);
        (state, attempts)
    }

    fn fake_state_counting_contexts(
        failures: usize,
    ) -> (LoopState<FakeConnector>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let contexts = Arc::new(AtomicUsize::new(0));
        let state = LoopState::new(FakeConnector {
            attempts: Arc::clone(&attempts),
            contexts: Arc::clone(&contexts),
            failures,
        });
        (state, attempts, contexts)
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

    // Criterion: the module ids come from the graph's own counter, shared by
    // every combined sink; `modules_for` lists one sink's modules only, and
    // `take_module` hands one back exactly once.
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
        assert_ne!(first, other, "the counter is shared across sinks");
        assert_ne!(second, other, "the counter is shared across sinks");
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

    // Criterion: `next_module_id` announces the id `add_module` then hands out,
    // across sinks and across a lost connection. `load_branch` names the
    // loopback's nodes `blue2th_loop.<id>` before the module is added: a
    // mismatch would leave liveness and unload looking for another branch.
    #[test]
    fn test_loop_state_next_module_id_is_the_id_add_module_hands_out() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());

        let announced = state.next_module_id();
        assert_eq!(
            state.add_module(COMBINED, branch(SPEAKER, 50), "m1"),
            announced
        );
        let announced = state.next_module_id();
        assert_eq!(
            state.add_module("other_combined", branch(SPEAKER, 70), "m2"),
            announced
        );

        state.on_disconnect();
        let announced = state.next_module_id();
        assert_eq!(
            state.add_module(COMBINED, branch(SPEAKER, 50), "m3"),
            announced,
            "and after a lost connection"
        );
    }

    // Criterion: a branch reports the delay the graph last applied to it — at
    // load, then after each `set_branch_delay` — not a value re-read from the
    // node. Only that module changes, under the same id.
    #[test]
    fn test_loop_state_reports_the_delay_last_applied_to_a_module() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        let a = state.add_module(COMBINED, branch(SPEAKER, 0), "m1");
        let b = state.add_module(
            COMBINED,
            branch("bluez_output.11_22_33_44_55_66.1", 250),
            "m2",
        );

        assert!(state.record_module_delay(a, 120).is_ok());

        assert_eq!(
            state.modules_for(COMBINED),
            vec![
                (a, branch(SPEAKER, 120)),
                (b, branch("bluez_output.11_22_33_44_55_66.1", 250)),
            ]
        );
    }

    // Criterion (non-nominal): `set_branch_delay` on an id the graph does not
    // hold is an `Err`, and changes no module.
    #[test]
    fn test_loop_state_delay_of_an_unknown_module_errs() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        let a = state.add_module(COMBINED, branch(SPEAKER, 0), "m1");

        assert!(matches!(
            state.record_module_delay(a + 1, 120),
            Err(AudioError::PipeWire(_))
        ));
        assert_eq!(state.modules_for(COMBINED), vec![(a, branch(SPEAKER, 0))]);
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

    // Criterion (non-nominal): with no daemon, a failed attempt costs a socket
    // connect, never a context. Destroying a context joins its `module-rt`
    // thread, which can sit in a D-Bus call to RTKit for 25 s: one context per
    // attempt kept the loop thread blocked that long for every command.
    #[test]
    fn test_loop_state_creates_one_context_across_failed_connections() {
        let (mut state, attempts, contexts) = fake_state_counting_contexts(3);

        for _ in 0..3 {
            assert!(matches!(state.connection(), Err(AudioError::PipeWire(_))));
        }
        assert!(state.connection().is_ok(), "the daemon is back");

        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(
            contexts.load(Ordering::SeqCst),
            1,
            "every attempt reuses the one context"
        );
    }

    // Criterion (non-nominal): a lost connection is reopened from the same
    // context — the loss drops the connection, not the context.
    #[test]
    fn test_loop_state_keeps_its_context_across_a_lost_connection() {
        let (mut state, attempts, contexts) = fake_state_counting_contexts(0);
        assert!(state.connection().is_ok());

        state.on_disconnect();
        assert!(state.connection().is_ok());

        assert_eq!(attempts.load(Ordering::SeqCst), 2, "reconnected");
        assert_eq!(contexts.load(Ordering::SeqCst), 1);
    }

    // ─── The loop side: which sinks are listed ───────────────────────────────

    /// A combined sink as its node reads once bound: an `adapter` over the
    /// null-audio-sink factory.
    fn null_sink(name: &str) -> NodeEntry {
        node(&[
            ("node.name", name),
            ("media.class", "Audio/Sink"),
            ("factory.name", "support.null-audio-sink"),
        ])
    }

    /// A combined sink left by the `pactl`-era server (#78), as a live daemon
    /// reports one: pipewire-pulse's null sink, carrying its module id.
    fn pactl_null_sink(name: &str) -> NodeEntry {
        node(&[
            ("node.name", name),
            ("media.class", "Audio/Sink"),
            ("factory.name", "support.null-audio-sink"),
            ("pulse.module.id", "536870916"),
        ])
    }

    fn speaker_sink() -> NodeEntry {
        node(&[
            ("node.name", SPEAKER),
            ("media.class", "Audio/Sink"),
            ("device.api", "bluez5"),
        ])
    }

    // Criterion: the combined sink this graph created is listed, so the
    // router reuses it rather than rebuilding under a playing stream.
    #[test]
    fn test_listed_sinks_keeps_the_combined_sink_the_graph_owns() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        state.set_null_sink(COMBINED, "proxy");
        state.mirror_mut().nodes.insert(57, speaker_sink());
        state.mirror_mut().nodes.insert(61, null_sink(COMBINED));

        assert_eq!(
            state.listed_sinks(),
            vec![SPEAKER.to_string(), COMBINED.to_string()]
        );
    }

    // Criterion: a null sink the graph does not own — a `pactl`-era leftover —
    // is hidden, so the router builds and its teardown clears the leftover;
    // the speakers next to it stay listed.
    #[test]
    fn test_listed_sinks_hides_a_leftover_null_sink() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        state.mirror_mut().nodes.insert(57, speaker_sink());
        state
            .mirror_mut()
            .nodes
            .insert(61, pactl_null_sink(COMBINED));

        assert_eq!(state.listed_sinks(), vec![SPEAKER.to_string()]);
    }

    // Criterion: a sink the graph does not own but that no null-sink factory
    // made — a real device — is listed, even when it carries the combined
    // sink's name.
    #[test]
    fn test_listed_sinks_keeps_a_sink_no_null_sink_factory_made() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        state.mirror_mut().nodes.insert(57, speaker_sink());
        state.mirror_mut().nodes.insert(
            61,
            node(&[
                ("node.name", COMBINED),
                ("media.class", "Audio/Sink"),
                ("device.api", "alsa"),
            ]),
        );

        assert_eq!(
            state.listed_sinks(),
            vec![SPEAKER.to_string(), COMBINED.to_string()]
        );
    }

    // Criterion: a lost connection forgets the graph's ownership with its
    // proxies, so a combined sink seen after reconnecting is a leftover, never
    // the graph's own.
    #[test]
    fn test_listed_sinks_after_a_lost_connection_owns_no_combined_sink() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        state.set_null_sink(COMBINED, "proxy");

        state.on_disconnect();
        assert!(state.connection().is_ok());
        state.mirror_mut().nodes.insert(57, speaker_sink());
        state.mirror_mut().nodes.insert(61, null_sink(COMBINED));

        assert_eq!(state.listed_sinks(), vec![SPEAKER.to_string()]);
    }

    // ─── Branches waiting for their ports ───────────────────────────────────

    // Criterion: a branch whose monitor links are made reports its liveness.
    #[test]
    fn test_branch_live_of_a_wired_branch_is_its_liveness() {
        let both = [MONITOR_INTO_1, BRANCH_1_INTO_SPEAKER].concat();
        assert_eq!(
            branch_live(&liveness_mirror(&both), 1, COMBINED, SPEAKER, None),
            Some(true)
        );
        assert_eq!(
            branch_live(
                &liveness_mirror(&BRANCH_1_INTO_SPEAKER),
                1,
                COMBINED,
                SPEAKER,
                None
            ),
            Some(false),
            "made, then lost its monitor links: dead"
        );
    }

    // Criterion (guard): a branch still waiting for the ports of a combined
    // sink created a moment ago is "cannot tell", never dead — reading it dead
    // unloads and reloads it on every tick until the ports appear. The near
    // miss: the same mirror, the same missing monitor links, but no wait
    // recorded, reads dead.
    #[test]
    fn test_branch_live_of_a_branch_waiting_for_its_ports_is_unknown() {
        let mirror = liveness_mirror(&BRANCH_1_INTO_SPEAKER);

        assert_eq!(
            branch_live(&mirror, 1, COMBINED, SPEAKER, Some(Duration::from_secs(1))),
            None
        );
        assert_eq!(
            branch_live(&mirror, 1, COMBINED, SPEAKER, None),
            Some(false)
        );
    }

    // Criterion (guard): the wait is bounded — a branch whose ports never
    // appear is dead once the grace has run out, so it is reloaded rather than
    // kept silent for good. The near miss: one millisecond short of the grace.
    #[test]
    fn test_branch_live_of_a_branch_waiting_past_the_grace_is_dead() {
        let mirror = liveness_mirror(&BRANCH_1_INTO_SPEAKER);
        let just_short = PENDING_LINKS_GRACE - Duration::from_millis(1);

        assert_eq!(
            branch_live(&mirror, 1, COMBINED, SPEAKER, Some(PENDING_LINKS_GRACE)),
            Some(false)
        );
        assert_eq!(
            branch_live(&mirror, 1, COMBINED, SPEAKER, Some(just_short)),
            None
        );
    }

    // Criterion: a waiting branch whose playback side is gone (its speaker
    // vanished, the module destroyed itself) is dead at once, not after the
    // grace. The near miss: branch 10's `.out` node is still there.
    #[test]
    fn test_branch_live_of_a_waiting_branch_without_its_out_node_is_dead() {
        let mut mirror = liveness_mirror(&[]);
        mirror.nodes.remove(&91);
        let waiting = Some(Duration::from_secs(1));

        assert_eq!(
            branch_live(&mirror, 1, COMBINED, SPEAKER, waiting),
            Some(false)
        );
        assert_eq!(
            branch_live(&liveness_mirror(&[]), 1, COMBINED, SPEAKER, waiting),
            None,
            "its `.out` node is still there: still waiting"
        );
    }

    // Criterion: a waiting branch can be linked once both channel pairs are
    // in the mirror. The near misses: one input port missing, and branch 30's
    // ports present while branch 3's are not.
    #[test]
    fn test_ready_to_wire_needs_both_channel_pairs_of_that_branch() {
        assert!(ready_to_wire(&ports_mirror(), COMBINED, 3));

        let mut one_missing = ports_mirror();
        one_missing.ports.remove(&131);
        assert!(!ready_to_wire(&one_missing, COMBINED, 3));

        let mut only_branch_30 = ports_mirror();
        only_branch_30.ports.remove(&130);
        only_branch_30.ports.remove(&131);
        assert!(!ready_to_wire(&only_branch_30, COMBINED, 3));
        assert!(ready_to_wire(&only_branch_30, COMBINED, 30));
    }

    // Criterion: the combined sink's own monitor ports are required too — a
    // sink created a moment ago has none yet.
    #[test]
    fn test_ready_to_wire_waits_for_the_combined_sinks_monitor_ports() {
        let mut no_monitor = ports_mirror();
        no_monitor.ports.remove(&112);
        no_monitor.ports.remove(&113);

        assert!(!ready_to_wire(&no_monitor, COMBINED, 3));
        assert!(
            !ready_to_wire(&ports_mirror(), "", 3),
            "an empty sink name is no sink"
        );
    }

    // Criterion: the loop remembers since when each branch has waited, and
    // which combined sink it waits on.
    #[test]
    fn test_loop_state_reports_how_long_a_branchs_links_have_waited() {
        let (mut state, _) = fake_state(0);
        let id = state.add_module(COMBINED, branch(SPEAKER, 0), "m1");
        let other = state.add_module(COMBINED, branch("bluez_output.11.1", 0), "m2");
        let since = Instant::now();

        state.mark_links_pending(id, since);

        assert_eq!(
            state.links_pending_for(id, since + Duration::from_secs(3)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(state.links_pending_for(other, since), None);
        assert_eq!(
            state.pending_link_branches(),
            vec![(id, COMBINED.to_string())]
        );
    }

    // Criterion: the wait ends when the links are made, when the module is
    // taken back, and when the connection is lost.
    #[test]
    fn test_loop_state_forgets_a_wait_once_linked_unloaded_or_disconnected() {
        let (mut state, _) = fake_state(0);
        assert!(state.connection().is_ok());
        let since = Instant::now();
        let linked = state.add_module(COMBINED, branch(SPEAKER, 0), "m1");
        let unloaded = state.add_module(COMBINED, branch(SPEAKER, 0), "m2");
        let dropped = state.add_module(COMBINED, branch(SPEAKER, 0), "m3");
        for id in [linked, unloaded, dropped] {
            state.mark_links_pending(id, since);
        }

        state.mark_links_made(linked);
        assert_eq!(state.links_pending_for(linked, since), None);

        assert_eq!(state.take_module(unloaded), Some("m2"));
        assert_eq!(state.links_pending_for(unloaded, since), None);
        assert_eq!(
            state.pending_link_branches(),
            vec![(dropped, COMBINED.to_string())]
        );

        state.on_disconnect();
        assert!(state.pending_link_branches().is_empty());
    }

    // Criterion (guard): once a branch's module is loaded and kept, its load
    // is `Ok` whatever the sync or the wiring after it did — a load reported
    // as failed is never armed for its confirming reload, while the branch it
    // left behind stays listed and is never loaded again. The near miss: the
    // same call with a follow-up that succeeded.
    #[test]
    fn test_kept_branch_load_is_ok_whatever_followed() {
        assert!(kept_branch_load(3, Ok(())).is_ok());
        assert!(
            kept_branch_load(3, Err(AudioError::PipeWire("sync timed out".to_string()))).is_ok(),
            "a kept branch whose follow-up failed still reports its load"
        );
    }
}
