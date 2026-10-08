// SPDX-License-Identifier: MIT OR Apache-2.0
//! The [`Graph`] that drives PipeWire natively, from a `pw_main_loop` running
//! on a thread of its own (#79), and [`PipeWireGraph`], the way into it.
//!
//! The PipeWire objects are `Rc`-based and never leave that thread, which also
//! owns the audio router and runs it as an actor (#147, see
//! [`crate::router_actor`]): the router reaches the loop's [`LoopState`]
//! through [`Graph`], with no channel in between. [`PipeWireGraph`] only holds
//! a [`pipewire::channel`] sender into the thread: it is the
//! [`Transport`] a [`crate::router_handle::RouterHandle`] sends its messages
//! through, each in an [`Envelope`] carrying the instant past which the thread
//! no longer starts it (#146).
//!
//! The decisions are pure functions over a [`Mirror`] of the registry, so the
//! tests pin them without a daemon; the loop side only applies them.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Cursor;
use std::rc::Rc;
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
use pw::link::Link;
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::metadata::Metadata;
use pw::node::{Node, NodeListener};
use pw::properties::PropertiesBox;
use pw::registry::{GlobalObject, RegistryRc};
use pw::types::ObjectType;
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{AudioError, AudioRouter, CombineBranch};
use crate::graph::{named, Graph, LoadedBranch, NamedGuard};
use crate::router_actor::{Actor, Envelope, Queue, Shared as RouterShared, Transport};

/// How long the loop thread gives the round trips of one router message, all
/// of them together, counted from the instant it starts the message (#147).
/// The handle waits [`REPLY_MARGIN`] longer than that past the message's
/// `start_by`, so a message answers with the daemon's error rather than with
/// the handle's timeout.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_millis(1600);

/// How long after it was sent the loop thread may still start a request's
/// message (#146). Time spent in the queue counts against it.
pub(crate) const START_BUDGET: Duration = Duration::from_millis(300);

/// How much longer than a started message's round trips the handle waits
/// (#146): what lets the answer of a message started at its `start_by` reach a
/// caller that is still waiting.
pub(crate) const REPLY_MARGIN: Duration = Duration::from_millis(100);

/// The factory the combined sink's node is created from.
const NULL_SINK_FACTORY: &str = "support.null-audio-sink";

/// The graph's end of the channel into the loop thread.
pub(crate) trait LoopSender: Send {
    /// Hand `envelope` to the loop thread; give it back when the thread is gone.
    fn send(&self, envelope: Envelope) -> Result<(), Envelope>;
}

/// Starts a loop thread and returns the sender into it. The argument is where
/// that thread reports its [`GraphEvent`]s: `None` for a graph nobody watches.
pub(crate) type SpawnLoop =
    Box<dyn FnMut(Option<UnboundedSender<GraphEvent>>) -> Box<dyn LoopSender> + Send>;

/// What the registry reports to the rest of the server (#80): a speaker's
/// `bluez_output.*` sink appearing or vanishing, the combined sink this
/// connection created removed by someone else (#139), and the connection to
/// the daemon coming back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphEvent {
    SinkAppeared { name: String, at: Instant },
    SinkVanished { name: String, at: Instant },
    CombinedSinkVanished { name: String, at: Instant },
    Reconnected,
}

/// Which registry callback a global came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryChange {
    Added,
    Removed,
}

/// The prefix every speaker sink's node name opens with.
const SPEAKER_SINK_PREFIX: &str = "bluez_output.";

/// The event a registry change of a node with `props` is, if it is a speaker
/// sink's: an `Audio/Sink` whose `node.name` is `bluez_output.` followed by
/// something. The bare prefix names no speaker, so it emits nothing.
pub(crate) fn speaker_sink_event(
    change: RegistryChange,
    props: &BTreeMap<String, String>,
    at: Instant,
) -> Option<GraphEvent> {
    if props.get("media.class").map(String::as_str) != Some("Audio/Sink") {
        return None;
    }
    let name = props.get("node.name")?;
    let address = name.strip_prefix(SPEAKER_SINK_PREFIX)?;
    if address.is_empty() {
        return None;
    }
    // Cloned: the event leaves the loop thread, the props stay in the mirror.
    let name = name.clone();
    Some(match change {
        RegistryChange::Added => GraphEvent::SinkAppeared { name, at },
        RegistryChange::Removed => GraphEvent::SinkVanished { name, at },
    })
}

/// How long the loop waits before its next reconnect attempt, after
/// `failures` failed ones.
pub(crate) fn reconnect_delay(failures: u32) -> Duration {
    let seconds = match failures {
        0 => 1,
        1 => 2,
        2 => 5,
        3 => 10,
        _ => 30,
    };
    Duration::from_secs(seconds)
}

/// The way into the loop thread: a sender into it, and the means to start a
/// new thread when the previous one has died.
pub struct PipeWireGraph {
    spawn_loop: SpawnLoop,
    sender: Option<Box<dyn LoopSender>>,
    /// Where every loop thread started for this graph reports its events.
    events: Option<UnboundedSender<GraphEvent>>,
    /// What every actor started for this graph shares with the router handle
    /// (#147): the routing generation and the confirmation due time.
    shared: RouterShared,
}

impl PipeWireGraph {
    /// A graph over the PipeWire daemon of the current session. Starts nothing:
    /// the loop thread is spawned, and connects, on the first message.
    pub fn spawn() -> Self {
        let shared = RouterShared::new();
        // Cloned: every thread started runs an actor sharing the same state.
        let for_threads = shared.clone();
        Self {
            // Cloned: each thread started holds a handle of its own onto it.
            spawn_loop: Box::new(move |events| spawn_loop_thread(events, for_threads.clone())),
            sender: None,
            events: None,
            shared,
        }
    }

    /// Start the loop thread now, connected at once, reporting its
    /// [`GraphEvent`]s to `events`.
    ///
    /// Called once, before any message. A thread an earlier message started is
    /// not stopped by it: dropping a `pw::channel::Sender` neither closes the
    /// channel nor wakes its loop, so that thread would keep running — and
    /// keep what it created — beside the watched one.
    pub fn watch(&mut self, events: UnboundedSender<GraphEvent>) {
        self.events = Some(events);
        // Cloned: each thread started, including one replacing a dead one,
        // holds a sender of its own.
        self.sender = Some((self.spawn_loop)(self.events.clone()));
    }

    /// A graph with no loop thread at all: every message errs at once, as when
    /// the thread cannot be started. It never reaches a daemon, which is what
    /// the store-free routers the integration tests build need: a graph over
    /// the session's daemon would tear down the operator's live combined sink.
    pub fn detached() -> Self {
        Self::with_loop(Box::new(|_| Box::new(NoLoop)))
    }

    /// A graph whose loop threads are started by `spawn_loop`.
    pub(crate) fn with_loop(spawn_loop: SpawnLoop) -> Self {
        Self {
            spawn_loop,
            sender: None,
            events: None,
            shared: RouterShared::new(),
        }
    }

    /// What the actors started for this graph share with the handle that
    /// sends through it.
    pub(crate) fn shared(&self) -> RouterShared {
        // Cloned: a handle onto the same state.
        self.shared.clone()
    }
}

/// The graph is the router handle's way to the loop thread (#147).
impl Transport for PipeWireGraph {
    /// Hand `envelope` to the loop thread, starting one when there is none and
    /// replacing one that has died. The envelope goes as it came: one resent
    /// to a replacement thread keeps the `start_by` it was stamped with.
    fn send(&mut self, envelope: Envelope) -> Result<(), AudioError> {
        let spawn_loop = &mut self.spawn_loop;
        let events = &self.events;
        // Cloned: every thread started holds a sender of its own.
        let sender = self
            .sender
            .get_or_insert_with(|| spawn_loop(events.clone()));
        let Err(envelope) = sender.send(envelope) else {
            return Ok(());
        };
        // The thread is gone: a new one answers this very message. Replacing
        // the sender drops what the dead thread never took out of its
        // channel, and each of those replies with it.
        let fresh = spawn_loop(events.clone());
        let sent = fresh.send(envelope);
        self.sender = Some(fresh);
        sent.map_err(|_| AudioError::PipeWire("the PipeWire graph thread is not running".into()))
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
        self.nodes.is_empty()
            && self.links.is_empty()
            && self.ports.is_empty()
            && self.devices.is_empty()
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

impl PortEntry {
    fn prop(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    /// The node the port belongs to.
    fn node(&self) -> Option<u32> {
        self.prop("node.id")?.parse().ok()
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
    format!("{}.{end}", branch_group(id))
}

/// The `node.group` both stream nodes of branch `id` carry.
fn branch_group(id: u32) -> String {
    format!("blue2th_delay.{id}")
}

/// The ids of the nodes of branch `id`, both sides, as the mirror lists them.
fn branch_node_ids(mirror: &Mirror, id: u32) -> Vec<u32> {
    let ins = branch_node_name(id, "in");
    let outs = branch_node_name(id, "out");
    mirror
        .node_ids_named(&ins)
        .chain(mirror.node_ids_named(&outs))
        .collect()
}

/// The globals a teardown of `sink_name` destroys: both nodes of each of the
/// graph's own `branches`, and whatever [`foreign_combined_globals`] takes.
/// A branch's capture side targets nothing, so the foreign rule cannot find
/// it: its nodes are named by id instead.
pub(crate) fn teardown_globals(mirror: &Mirror, sink_name: &str, branches: &[u32]) -> Vec<u32> {
    let mut doomed: BTreeSet<u32> = branches
        .iter()
        .flat_map(|id| branch_node_ids(mirror, *id))
        .collect();
    doomed.extend(foreign_combined_globals(mirror, sink_name));
    doomed.into_iter().collect()
}

/// `value` as a quoted SPA-JSON string.
fn spa_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The largest delay a branch's `delay` node can be tuned to, in seconds: its
/// `max-delay`, fixed when the branch is loaded.
pub(crate) const MAX_DELAY_SECONDS: f32 = 1.0;

/// The name of the `delay` node inside a branch's filter graph, and of its
/// control: [`delay_props_pod`] addresses the control as `<node>:<control>`.
const DELAY_NODE: &str = "delay";
const DELAY_CONTROL: &str = "Delay (s)";

/// How many channels a branch carries: the combined sink's `FL,FR`, linked
/// port to port.
const BRANCH_CHANNELS: usize = 2;

/// How long a branch may wait for the ports its monitor links need before it
/// counts as dead. A combined sink created a moment ago announces its monitor
/// ports only once the session manager has configured it, which takes seconds
/// on a server that has just started.
pub(crate) const PENDING_LINKS_GRACE: Duration = Duration::from_secs(10);

/// The `libpipewire-module-filter-chain` argument string for one delay branch
/// into `real_sink`, delayed by `delay_ms` (#81).
///
/// The capture side is left for the server to link (`node.autoconnect =
/// false`), and the playback side is pinned to the speaker without reconnect,
/// so a speaker that goes away never moves its branch onto another sink (#67).
pub(crate) fn delay_chain_module_args(
    real_sink: &str,
    delay_ms: u32,
    id: u32,
) -> Result<String, AudioError> {
    named("target sink", real_sink)?;
    let group = spa_string(&branch_group(id));
    let delay = format!("{}.{:03}", delay_ms / 1000, delay_ms % 1000);
    Ok(format!(
        "{{ audio.channels = {BRANCH_CHANNELS} audio.position = [ FL FR ] \
         filter.graph = {{ nodes = [ {{ type = builtin name = {node} label = delay \
         config = {{ \"max-delay\" = {MAX_DELAY_SECONDS:.1} }} \
         control = {{ {control} = {delay} }} }} ] }} \
         capture.props = {{ node.name = {capture} node.group = {group} \
         node.autoconnect = false }} \
         playback.props = {{ node.name = {playback} node.group = {group} \
         target.object = {real} node.dont-reconnect = true }} }}",
        node = spa_string(DELAY_NODE),
        control = spa_string(DELAY_CONTROL),
        capture = spa_string(&branch_node_name(id, "in")),
        playback = spa_string(&branch_node_name(id, "out")),
        real = spa_string(real_sink),
    ))
}

/// The `Props` param that sets a branch's `delay` node to `seconds`: the
/// `params` struct `[ "delay:Delay (s)", seconds ]` a filter-chain reads its
/// controls from. A delay outside `0..=MAX_DELAY_SECONDS` is refused.
pub(crate) fn delay_props_pod(seconds: f32) -> Result<Vec<u8>, AudioError> {
    if !(0.0..=MAX_DELAY_SECONDS).contains(&seconds) {
        return Err(AudioError::PipeWire(format!(
            "a delay of {seconds} s is outside the branch's range"
        )));
    }
    let value = Value::Object(Object {
        type_: libspa::sys::SPA_TYPE_OBJECT_Props,
        id: libspa::sys::SPA_PARAM_Props,
        properties: vec![Property {
            key: libspa::sys::SPA_PROP_params,
            flags: PropertyFlags::empty(),
            value: Value::Struct(vec![
                Value::String(format!("{DELAY_NODE}:{DELAY_CONTROL}")),
                Value::Float(seconds),
            ]),
        }],
    });
    PodSerializer::serialize(Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| AudioError::PipeWire(format!("cannot build the Props param: {e:?}")))
}

/// The `(output port, input port)` pairs linking `out_node`'s outputs to
/// `in_node`'s inputs, channel to channel: matched by `audio.channel`, never by
/// position, and a port with no channel pairs with nothing.
pub(crate) fn channel_port_pairs(
    mirror: &Mirror,
    out_node: &str,
    in_node: &str,
) -> Vec<(u32, u32)> {
    let outs: BTreeSet<u32> = mirror.node_ids_named(out_node).collect();
    let ins: BTreeSet<u32> = mirror.node_ids_named(in_node).collect();
    let channel_ports = |nodes: &BTreeSet<u32>, direction: &str| -> Vec<(u32, String)> {
        mirror
            .ports
            .iter()
            .filter(|(_, port)| port.node().is_some_and(|node| nodes.contains(&node)))
            .filter(|(_, port)| port.prop("port.direction") == Some(direction))
            .filter_map(|(id, port)| {
                let channel = port.prop("audio.channel").filter(|c| !c.is_empty())?;
                Some((*id, channel.to_string()))
            })
            .collect()
    };
    let inputs = channel_ports(&ins, "in");
    channel_ports(&outs, "out")
        .into_iter()
        .filter_map(|(output, channel)| {
            inputs
                .iter()
                .find(|(_, other)| *other == channel)
                .map(|(input, _)| (output, *input))
        })
        .collect()
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

/// Whether branch `id` is fed by `sink_name` and feeds `real_sink`: at least
/// one link on each side, between nodes named exactly so.
pub(crate) fn branch_liveness(mirror: &Mirror, id: u32, sink_name: &str, real_sink: &str) -> bool {
    let ids = |name: &str| -> BTreeSet<u32> { mirror.node_ids_named(name).collect() };
    let linked = |from: &BTreeSet<u32>, to: &BTreeSet<u32>| {
        mirror
            .links
            .values()
            .any(|link| from.contains(&link.output_node) && to.contains(&link.input_node))
    };
    linked(&ids(sink_name), &ids(&branch_node_name(id, "in")))
        && linked(&ids(&branch_node_name(id, "out")), &ids(real_sink))
}

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
    if let Err(e) = follow_up {
        tracing::warn!("delay branch {id} is loaded, but settling it failed: {e}");
    }
    Ok(())
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
    let Some(waited) = pending_for else {
        return Some(branch_liveness(mirror, id, sink_name, real_sink));
    };
    let out_gone = mirror
        .node_ids_named(&branch_node_name(id, "out"))
        .next()
        .is_none();
    if waited >= PENDING_LINKS_GRACE || out_gone {
        return Some(false);
    }
    None
}

/// Whether branch `id`'s capture side can be linked from `sink_name` now:
/// both channel pairs are in the mirror.
pub(crate) fn ready_to_wire(mirror: &Mirror, sink_name: &str, id: u32) -> bool {
    channel_port_pairs(mirror, sink_name, &branch_node_name(id, "in")).len() >= BRANCH_CHANNELS
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

/// The ids of the output streams that asked for `sink_name` (#139): the
/// `Stream/Output/Audio` nodes whose `target.object` equals it exactly.
pub(crate) fn streams_targeting(mirror: &Mirror, sink_name: &str) -> Vec<u32> {
    if sink_name.is_empty() {
        return Vec::new();
    }
    mirror
        .nodes
        .iter()
        .filter(|(_, node)| {
            node.prop("media.class") == Some("Stream/Output/Audio")
                && node.prop("target.object") == Some(sink_name)
        })
        .map(|(id, _)| *id)
        .collect()
}

/// The metadata key WirePlumber keeps the user's chosen default sink in.
const CONFIGURED_DEFAULT_SINK_KEY: &str = "default.configured.audio.sink";

/// Whether `value`, read from `default.configured.audio.sink`, names
/// `sink_name` exactly (#66).
///
/// The value is read as the JSON object WirePlumber writes, `{"name": …}`, and
/// the name is compared whole: a substring or prefix check would also match the
/// user's own sink whose name merely contains the combined sink's.
pub(crate) fn configured_default_names(value: Option<&str>, sink_name: &str) -> bool {
    if sink_name.is_empty() {
        return false;
    }
    let Some(parsed) = value.and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok()) else {
        return false;
    };
    parsed.get("name").and_then(serde_json::Value::as_str) == Some(sink_name)
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

/// The entry of a device's `routes` whose `device` is `route_device` — the
/// sink node's `card.profile.device`, not the route's own `index`.
fn route_of(routes: Vec<Route>, route_device: i32) -> Option<Route> {
    routes
        .into_iter()
        .find(|route| route.device == route_device)
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
    /// One delay branch module loaded into the server process.
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
    /// When the message being run runs out of time for its round trips.
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
        let (_, branch, _) = self
            .modules
            .get_mut(&id)
            .ok_or_else(|| AudioError::PipeWire(format!("no delay branch {id}")))?;
        branch.latency_ms = delay_ms;
        Ok(())
    }

    /// Record that branch `id`'s monitor links wait for their ports since `since`.
    pub(crate) fn mark_links_pending(&mut self, id: u32, since: Instant) {
        self.pending_links.insert(id, since);
    }

    /// Record that branch `id`'s monitor links are made.
    pub(crate) fn mark_links_made(&mut self, id: u32) {
        self.pending_links.remove(&id);
    }

    /// How long branch `id`'s monitor links have waited at `now`; `None` when
    /// they are not waiting.
    pub(crate) fn links_pending_for(&self, id: u32, now: Instant) -> Option<Duration> {
        self.pending_links
            .get(&id)
            .map(|since| now.saturating_duration_since(*since))
    }

    /// The branches whose monitor links wait, with the combined sink each one
    /// is fed from.
    pub(crate) fn pending_link_branches(&self) -> Vec<(u32, String)> {
        self.pending_links
            .keys()
            .filter_map(|id| {
                self.modules
                    .get(id)
                    // Cloned: the sink name leaves the state with its id.
                    .map(|(sink, _, _)| (*id, sink.clone()))
            })
            .collect()
    }

    /// Forget the module `id` and hand it back for destruction.
    pub(crate) fn take_module(&mut self, id: u32) -> Option<C::Module> {
        self.pending_links.remove(&id);
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
        // forgotten: a filter-chain unloads itself on its core's error. The
        // context stays, so the next message reconnects from it.
        self.null_sinks.clear();
        self.modules.clear();
        self.pending_links.clear();
        self.connection = None;
        self.mirror = Mirror::default();
    }
}

// ─── The production loop thread ─────────────────────────────────────────────

/// A loop that is not there: every message comes back, so the graph knows.
struct NoLoop;

impl LoopSender for NoLoop {
    fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
        Err(envelope)
    }
}

/// The graph's end into a real loop thread.
struct PwLoopSender {
    sender: pw::channel::Sender<Envelope>,
    thread: JoinHandle<()>,
}

impl LoopSender for PwLoopSender {
    fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
        // The channel's queue outlives the thread, so a send to a dead thread
        // would succeed and wait out the timeout: ask the thread instead.
        if self.thread.is_finished() {
            return Err(envelope);
        }
        self.sender.send(envelope)
    }
}

/// Start a loop thread whose actor shares `shared`; [`NoLoop`] when the
/// thread cannot even be started.
fn spawn_loop_thread(
    events: Option<UnboundedSender<GraphEvent>>,
    shared: RouterShared,
) -> Box<dyn LoopSender> {
    let (sender, receiver) = pw::channel::channel::<Envelope>();
    match std::thread::Builder::new()
        .name("pipewire-graph".into())
        .spawn(move || run_loop_thread(receiver, events, shared))
    {
        Ok(thread) => Box::new(PwLoopSender { sender, thread }),
        Err(e) => {
            tracing::error!("cannot start the PipeWire graph thread: {e}");
            Box::new(NoLoop)
        },
    }
}

/// The loop thread: own the audio router, run each message it receives against
/// the daemon, and drop the connection's state when the daemon goes away.
///
/// The router is created here and dies with the thread (#147): the branches it
/// armed a confirmation for are modules of this thread's own connection, so a
/// thread replacing a dead one starts from a router that knows of none.
///
/// A watched loop — one handed `events` — connects at once and, once the
/// connection is lost, reconnects on its own after [`reconnect_delay`], so a
/// restarted daemon is noticed without waiting for a message (#80). An
/// unwatched one connects on its first message, as before.
fn run_loop_thread(
    receiver: pw::channel::Receiver<Envelope>,
    events: Option<UnboundedSender<GraphEvent>>,
    shared: RouterShared,
) {
    pw::init();
    let mainloop = match MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(e) => {
            tracing::error!("cannot create the PipeWire main loop: {e}");
            return;
        },
    };
    // Messages are queued by the channel callback and run outside of it, so a
    // message can iterate the loop while it waits for the daemon.
    let inbox = Rc::new(RefCell::new(Queue::new()));
    let _attached = receiver.attach(mainloop.loop_(), {
        let inbox = Rc::clone(&inbox);
        move |envelope| inbox.borrow_mut().push(envelope)
    });
    let watched = events.is_some();
    let state = NamedGuard::new(LoopState::new(PwConnector {
        mainloop: mainloop.clone(),
        events,
    }));
    let mut actor = Actor::new(
        AudioRouter::over(Box::new(state), Box::new(Instant::now)),
        shared,
    );
    let mut reconnect = ReconnectWatch::new(Instant::now());
    loop {
        let state = actor.graph_mut().inner_mut();
        let mut answers_again = false;
        if watched && reconnect.attempt_due(state.is_connected(), Instant::now()) {
            match state.reconnect() {
                Ok(()) => {
                    answers_again = reconnect.connected(state.connector.events.as_ref());
                },
                // Connected, but the daemon did not answer the re-read in time:
                // the connection is kept, as a message keeps it, and the next
                // message reads the registry again.
                Err(e) if state.is_connected() => {
                    tracing::warn!("PipeWire reconnected, but the registry re-read failed: {e}");
                },
                Err(e) => {
                    let delay = reconnect.failed(Instant::now());
                    tracing::warn!(
                        "cannot reach PipeWire ({e}); next attempt in {} s",
                        delay.as_secs()
                    );
                },
            }
        }
        if answers_again {
            actor.graph_answers_again();
        }
        let state = actor.graph_mut().inner_mut();
        let timeout = match reconnect.wait(state.is_connected(), Instant::now()) {
            Some(left) if watched => Timeout::Finite(left),
            _ => Timeout::Infinite,
        };
        mainloop.loop_().iterate(timeout);
        if state.forget_a_lost_connection() {
            reconnect.lost(Instant::now());
        }
        // The late `done` of a stalled sync is acted on here, while the loop
        // is otherwise idle: waiting for the next message to see it would
        // leave the owed re-apply unpaid until something else happens (#152).
        let thawed = state.take_thaw();
        state.wire_waiting_branches();
        if thawed {
            actor.graph_answers_again();
        }
        // The clock is read once per message, as it is taken out: one that
        // waited behind the reconnect attempt, the wiring above or a slow
        // message has spent that time out of its start budget (#146).
        while actor.run_next(&inbox, Instant::now()) {
            if actor.graph_mut().inner_mut().forget_a_lost_connection() {
                reconnect.lost(Instant::now());
            }
        }
        // A message reconnects on its own: that is a reconnection too.
        let state = actor.graph_mut().inner_mut();
        let regained = state.is_connected() && reconnect.connected(state.connector.events.as_ref());
        // A message's own round trip can deliver the late `done` too.
        let thawed = state.take_thaw();
        if regained || thawed {
            actor.graph_answers_again();
        }
    }
}

/// When a watched loop next tries to reconnect, and whether it owes a
/// [`GraphEvent::Reconnected`] once it has.
struct ReconnectWatch {
    failures: u32,
    next_attempt: Instant,
    /// Set once a held connection was lost: the first connection of the
    /// thread is not a reconnection.
    owes_reconnected: bool,
}

impl ReconnectWatch {
    /// A thread that has not connected yet, and tries at once.
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            next_attempt: now,
            owes_reconnected: false,
        }
    }

    /// Whether a watched loop tries to reconnect at `now`: only while it holds
    /// no connection, and once the backoff has run out.
    fn attempt_due(&self, connected: bool, now: Instant) -> bool {
        !connected && now >= self.next_attempt
    }

    /// How long a watched loop may block waiting for the daemon at `now`:
    /// until its next attempt while it holds no connection — zero once that
    /// is past — and for as long as it takes (`None`) while it holds one.
    fn wait(&self, connected: bool, now: Instant) -> Option<Duration> {
        (!connected).then(|| self.next_attempt.saturating_duration_since(now))
    }

    /// The connection was lost at `now`: the walk starts again, and the first
    /// retry waits `reconnect_delay(0)`. The only place the count is reset —
    /// the count matters only once a connection is lost, and a message can
    /// connect and lose one before [`Self::connected`] ever sees it.
    fn lost(&mut self, now: Instant) {
        self.failures = 0;
        self.next_attempt = now + reconnect_delay(0);
        self.owes_reconnected = true;
    }

    /// An attempt at `now` failed; returns how long until the next one.
    fn failed(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay = reconnect_delay(self.failures);
        self.next_attempt = now + delay;
        delay
    }

    /// A connection is held: report the reconnection, once per loss. A closed
    /// channel is ignored — the consumer is gone. Answers whether this is a
    /// connection regained after a loss, which counts as the graph answering
    /// again (#152).
    fn connected(&mut self, events: Option<&UnboundedSender<GraphEvent>>) -> bool {
        if !self.owes_reconnected {
            return false;
        }
        self.owes_reconnected = false;
        tracing::info!("PipeWire connection back");
        if let Some(events) = events {
            let _ = events.send(GraphEvent::Reconnected);
        }
        true
    }
}

/// Whether the daemon answers again after a `core.sync` went unanswered
/// (#152): the first `done` whose seq is at least the seq of the stalled sync
/// is the thaw. Pure: the loop thread feeds it the seqs, no daemon needed.
#[derive(Debug, Default)]
struct StallWatch {
    /// The seq of the sync that went unanswered, while no `done` has caught
    /// up with it.
    stalled_at: Option<i32>,
    /// Set by the `done` that thawed a stall, until the loop side takes it.
    thawed: bool,
}

impl StallWatch {
    /// The `core.sync` of seq `seq` went unanswered past its deadline. With
    /// a stall already outstanding, the earliest one sets the threshold: its
    /// `done` is the first to arrive once the daemon resumes. A thaw not yet
    /// taken is dropped: the daemon stalls again, and acting on it would
    /// re-apply into a frozen graph.
    fn sync_unanswered(&mut self, seq: i32) {
        self.stalled_at = Some(self.stalled_at.map_or(seq, |stalled| stalled.min(seq)));
        self.thawed = false;
    }

    /// A `done` of seq `seq` arrived: answers whether it is the thaw, and
    /// holds that thaw for [`Self::take_thaw`].
    fn done(&mut self, seq: i32) -> bool {
        match self.stalled_at {
            Some(stalled) if seq >= stalled => {
                self.stalled_at = None;
                self.thawed = true;
                true
            },
            _ => false,
        }
    }

    /// Whether a thaw arrived since the last call. Taken once.
    fn take_thaw(&mut self) -> bool {
        std::mem::take(&mut self.thawed)
    }
}

fn pw_error(what: &'static str) -> impl Fn(pw::Error) -> AudioError {
    move |e| AudioError::PipeWire(format!("{what}: {e}"))
}

/// Opens [`PwConnection`]s on the loop thread's main loop.
struct PwConnector {
    mainloop: MainLoopRc,
    /// Handed to every connection's registry callbacks.
    events: Option<UnboundedSender<GraphEvent>>,
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
        // Cloned: each connection's callbacks hold a sender of their own.
        PwConnection::open(&self.mainloop, context, self.events.clone())
    }
}

/// A delay branch loaded into this process. The module owns itself and goes
/// away with its streams — see [`PwConnection::unload`]; what is kept here is
/// what the server created around it: the links feeding its capture side,
/// which die with their proxies, and a proxy of that capture node, which the
/// delay is set on.
#[derive(Default)]
struct InProcessModule {
    links: Vec<Link>,
    in_node: Option<Node>,
}

/// What the registry and core callbacks report, shared with the loop side.
#[derive(Default)]
struct Shared {
    mirror: Mirror,
    /// Where speaker sink events go; `None` for a graph nobody watches.
    events: Option<UnboundedSender<GraphEvent>>,
    globals: BTreeMap<u32, GlobalObject<PropertiesBox>>,
    done: Option<i32>,
    lost: bool,
    /// The syncs of this connection that went unanswered, and the thaw a
    /// late `done` brought (#152). Per connection: the seqs it compares are
    /// this connection's own.
    stall: StallWatch,
    /// The combined sinks this connection holds the proxy for, by node id
    /// (#139): only their removal is someone else's doing.
    combined_sinks: BTreeMap<u32, String>,
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
    fn open(
        mainloop: &MainLoopRc,
        context: &ContextRc,
        events: Option<UnboundedSender<GraphEvent>>,
    ) -> Result<Self, AudioError> {
        let core = context
            .connect_rc(None)
            .map_err(pw_error("cannot connect to PipeWire"))?;
        let registry = core
            .get_registry_rc()
            .map_err(pw_error("cannot read the PipeWire registry"))?;
        let shared = Rc::new(RefCell::new(Shared {
            events,
            ..Shared::default()
        }));
        let core_listener = core
            .add_listener_local()
            .done({
                let shared = Rc::clone(&shared);
                move |id, seq| {
                    if id == pw::core::PW_ID_CORE {
                        let mut shared = shared.borrow_mut();
                        shared.done = Some(seq.seq());
                        // A thaw is held by the watch until the loop takes it.
                        shared.stall.done(seq.seq());
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

    /// The mirror as the registry callbacks have left it, without a round trip.
    fn mirror_now(&self) -> Mirror {
        // Cloned: the loop side reads it while the callbacks keep writing theirs.
        self.shared.borrow().mirror.clone()
    }

    fn is_lost(&self) -> bool {
        self.shared.borrow().lost
    }

    /// One `core.sync` round trip: every event the daemon emitted before it has
    /// been delivered when this returns `Ok`. It errs once `deadline` — the
    /// message's, not its own — has passed.
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
                self.shared.borrow_mut().stall.sync_unanswered(pending);
                return Err(AudioError::Unanswered);
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

    /// Load one `libpipewire-module-filter-chain` into this process.
    fn load_filter_chain(&self, args: &str) -> Result<(), AudioError> {
        let name = CString::new("libpipewire-module-filter-chain")
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
                "cannot load the delay module: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Link output port `out_port` of node `out_node` to input port `in_port`
    /// of node `in_node`. No `object.linger`: the link is owned by this
    /// connection and dies with its proxy, or with the connection.
    fn create_link(
        &self,
        out_node: u32,
        out_port: u32,
        in_node: u32,
        in_port: u32,
    ) -> Result<Link, AudioError> {
        let mut props = PropertiesBox::new();
        props.insert("link.output.node", out_node.to_string());
        props.insert("link.output.port", out_port.to_string());
        props.insert("link.input.node", in_node.to_string());
        props.insert("link.input.port", in_port.to_string());
        self.core
            .create_object::<Link>("link-factory", &props)
            .map_err(pw_error("cannot link the delay branch"))
    }

    /// Bind the node global `id`, for a proxy of its own.
    fn bind_node(&self, id: u32) -> Result<Node, AudioError> {
        let shared = self.shared.borrow();
        let global = shared
            .globals
            .get(&id)
            .filter(|global| global.type_ == ObjectType::Node)
            .ok_or_else(|| AudioError::PipeWire(format!("no node {id}")))?;
        self.registry
            .bind::<Node, _>(global)
            .map_err(pw_error("cannot bind the delay node"))
    }

    fn destroy_global(&self, id: u32) {
        let _ = self.registry.destroy_global(id);
    }

    /// Delete the `default` metadata's configured sink when it names
    /// `sink_name` exactly, and answer whether it did. Nothing else is ever
    /// written: the server only removes a preference it left there itself.
    fn clear_stale_default_sink(
        &self,
        sink_name: &str,
        deadline: Instant,
    ) -> Result<bool, AudioError> {
        let metadata = self.default_metadata()?;
        // A freshly bound metadata replays every property it holds, so one
        // round trip delivers the current value, or none when the key is absent.
        let configured: Rc<RefCell<Option<String>>> = Rc::default();
        let listener = metadata
            .add_listener_local()
            .property({
                let configured = Rc::clone(&configured);
                move |subject, key, _type, value| {
                    if subject == 0 && key == Some(CONFIGURED_DEFAULT_SINK_KEY) {
                        *configured.borrow_mut() = value.map(str::to_string);
                    }
                    0
                }
            })
            .register();
        let synced = self.roundtrip(deadline);
        drop(listener);
        synced?;
        if !configured_default_names(configured.borrow().as_deref(), sink_name) {
            return Ok(false);
        }
        metadata.set_property(0, CONFIGURED_DEFAULT_SINK_KEY, None, None);
        self.roundtrip(deadline)?;
        Ok(true)
    }

    /// Bind the `default` metadata object, where WirePlumber keeps its
    /// preferences and the streams' targets.
    fn default_metadata(&self) -> Result<Metadata, AudioError> {
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
            .map_err(pw_error("cannot bind the default metadata"))
    }

    /// Write `sink_name` as the `target.object` of every stream in `streams`
    /// into the `default` metadata, the untyped write `pw-metadata` makes
    /// (#139). `target.node` is left alone.
    fn retarget_streams(
        &self,
        streams: &[u32],
        sink_name: &str,
        deadline: Instant,
    ) -> Result<(), AudioError> {
        let metadata = self.default_metadata()?;
        for id in streams {
            metadata.set_property(*id, "target.object", None, Some(sink_name));
        }
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
        let route = route_of(routes, target.route_device)
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
                self.emit(speaker_sink_event(
                    RegistryChange::Added,
                    &props,
                    Instant::now(),
                ));
                self.mirror.nodes.insert(global.id, NodeEntry { props });
            },
            ObjectType::Device => {
                self.mirror.devices.insert(global.id, DeviceEntry { props });
            },
            ObjectType::Port => {
                self.mirror.ports.insert(global.id, PortEntry { props });
                return;
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

    /// Record that this connection holds the proxy owning the combined sink
    /// `sink_name`, whose node is `id` (#139).
    fn hold_combined_sink(&mut self, id: u32, sink_name: &str) {
        self.combined_sinks.insert(id, sink_name.to_string());
    }

    /// Record that the proxy owning the combined sink `sink_name` has been
    /// dropped, so its removal is this server's own teardown (#139).
    fn release_combined_sink(&mut self, sink_name: &str) {
        self.combined_sinks.retain(|_, name| name != sink_name);
    }

    fn remove_global(&mut self, id: u32) {
        // Read before the node is forgotten: the removal carries only its id.
        let event = self.mirror.nodes.get(&id).and_then(|node| {
            speaker_sink_event(RegistryChange::Removed, &node.props, Instant::now())
        });
        self.emit(event);
        // Forgotten with its node: PipeWire reuses ids.
        let combined =
            self.combined_sinks
                .remove(&id)
                .map(|name| GraphEvent::CombinedSinkVanished {
                    name,
                    at: Instant::now(),
                });
        self.emit(combined);
        self.mirror.nodes.remove(&id);
        self.mirror.links.remove(&id);
        self.mirror.ports.remove(&id);
        self.mirror.devices.remove(&id);
        self.globals.remove(&id);
    }

    /// Send `event` to the watcher, if there is one. Never blocks; a closed
    /// channel means the consumer is gone, and the event is dropped.
    fn emit(&self, event: Option<GraphEvent>) {
        if let (Some(events), Some(event)) = (&self.events, event) {
            let _ = events.send(event);
        }
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
    /// Drop everything the connection held once the daemon has gone away,
    /// and answer whether it did.
    fn forget_a_lost_connection(&mut self) -> bool {
        let lost = self.connection.as_ref().is_some_and(PwConnection::is_lost);
        if lost {
            self.on_disconnect();
        }
        lost
    }

    /// Whether the late `done` of a stalled sync arrived since the last call
    /// (#152): the daemon answers again. Taken once.
    fn take_thaw(&mut self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|connection| connection.shared.borrow_mut().stall.take_thaw())
    }

    /// Connect, and re-read the whole registry in one round trip.
    fn reconnect(&mut self) -> Result<(), AudioError> {
        self.deadline = Instant::now() + COMMAND_TIMEOUT;
        let deadline = self.deadline;
        let result = self
            .connection()
            .and_then(|connection| connection.roundtrip(deadline));
        if result.is_err() {
            self.forget_a_lost_connection();
        }
        result
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
                live: branch_live(
                    &self.mirror,
                    id,
                    sink_name,
                    &branch.sink,
                    self.links_pending_for(id, Instant::now()),
                ),
                branch,
            })
            .collect())
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        let before: BTreeSet<u32> = self.mirror.node_ids_named(sink_name).collect();
        let node = self.connection()?.create_null_sink(sink_name)?;
        // A proxy this replaces is dropped here: its removal is ours.
        self.release_combined_sink(sink_name);
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
        // The node that appeared with the proxy is the one it owns.
        let created: Vec<u32> = self
            .mirror
            .node_ids_named(sink_name)
            .filter(|id| !before.contains(id))
            .collect();
        if let Some(connection) = self.connection.as_ref() {
            let mut shared = connection.shared.borrow_mut();
            for id in created {
                shared.hold_combined_sink(id, sink_name);
            }
        }
        Ok(())
    }

    /// Forget that the combined sink `sink_name` is held, before its proxy is
    /// dropped: the removal the registry reports next is this server's own.
    fn release_combined_sink(&mut self, sink_name: &str) {
        if let Some(connection) = self.connection.as_ref() {
            connection
                .shared
                .borrow_mut()
                .release_combined_sink(sink_name);
        }
    }

    /// Move every output stream that asked for `sink_name` back onto it (#139),
    /// and answer how many were asked.
    fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError> {
        self.sync_mirror()?;
        let streams = streams_targeting(&self.mirror, sink_name);
        if streams.is_empty() {
            return Ok(0);
        }
        let deadline = self.deadline;
        self.connection()?
            .retarget_streams(&streams, sink_name, deadline)?;
        Ok(streams.len())
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        let args = delay_chain_module_args(real_sink, latency_ms, self.next_module_id())?;
        self.connection()?.load_filter_chain(&args)?;
        // Kept before it is linked: a branch whose ports are not there yet
        // waits for them, loaded, and is linked as soon as they appear.
        let id = self.add_module(
            sink_name,
            CombineBranch {
                sink: real_sink.to_string(),
                latency_ms,
            },
            InProcessModule::default(),
        );
        tracing::debug!("loaded delay branch {id}: {args}");
        self.mark_links_pending(id, Instant::now());
        let follow_up = self.settle_new_branch(sink_name, id);
        kept_branch_load(id, follow_up)
    }

    /// Link a branch loaded a moment ago if its ports are already announced;
    /// otherwise leave it waiting for them.
    fn settle_new_branch(&mut self, sink_name: &str, id: u32) -> Result<(), AudioError> {
        self.sync_mirror()?;
        if ready_to_wire(&self.mirror, sink_name, id) {
            return self.wire_branch(sink_name, id);
        }
        // A combined sink created a moment ago has no monitor ports until the
        // session manager has configured it: the branch waits for them, and
        // `wire_waiting_branches` links it when they are announced.
        tracing::debug!("delay branch {id} waits for the ports of {sink_name}");
        Ok(())
    }

    /// Link `sink_name`'s monitor into branch `id`'s capture side, channel to
    /// channel, and keep the links and a proxy of that capture node with the
    /// module. Called once [`ready_to_wire`] holds on `self.mirror`.
    ///
    /// One attempt per branch: the wait ends before the first link, so a
    /// branch whose links cannot be made is judged by the links it has — dead
    /// — and reloaded by the router, rather than retried, and warned about, on
    /// every turn of the loop until the grace runs out.
    fn wire_branch(&mut self, sink_name: &str, id: u32) -> Result<(), AudioError> {
        self.mark_links_made(id);
        let in_name = branch_node_name(id, "in");
        let pairs = channel_port_pairs(&self.mirror, sink_name, &in_name);
        for (out_port, in_port) in pairs {
            let node_of = |port: u32| self.mirror.ports.get(&port).and_then(PortEntry::node);
            let (Some(out_node), Some(in_node)) = (node_of(out_port), node_of(in_port)) else {
                continue;
            };
            let link = self
                .connection()?
                .create_link(out_node, out_port, in_node, in_port)?;
            if let Some((_, _, module)) = self.modules.get_mut(&id) {
                module.links.push(link);
            }
        }
        let in_node_id = self
            .mirror
            .node_ids_named(&in_name)
            .next()
            .ok_or_else(|| AudioError::PipeWire(format!("no node {in_name}")))?;
        let in_node = self.connection()?.bind_node(in_node_id)?;
        if let Some((_, _, module)) = self.modules.get_mut(&id) {
            module.in_node = Some(in_node);
        }
        let deadline = self.deadline;
        self.connection()?.roundtrip(deadline)
    }

    /// Link every branch whose ports have been announced since it was loaded.
    /// Runs after each turn of the loop, so a branch is linked on the registry
    /// event that completes it rather than on the next message.
    fn wire_waiting_branches(&mut self) {
        let waiting = self.pending_link_branches();
        if waiting.is_empty() {
            return;
        }
        let Some(mirror) = self.connection.as_ref().map(PwConnection::mirror_now) else {
            return;
        };
        self.mirror = mirror;
        for (id, sink_name) in waiting {
            if !ready_to_wire(&self.mirror, &sink_name, id) {
                continue;
            }
            self.deadline = Instant::now() + COMMAND_TIMEOUT;
            match self.wire_branch(&sink_name, id) {
                Ok(()) => {
                    tracing::info!("linked delay branch {id} once {sink_name}'s ports appeared")
                },
                Err(e) => tracing::warn!("could not link delay branch {id} into {sink_name}: {e}"),
            }
        }
    }

    /// Unload branch `id` by destroying its two stream nodes: the filter-chain
    /// module destroys itself once its streams are gone.
    ///
    /// Not `pw_impl_module_destroy`: a module whose target vanished destroys
    /// itself too, with no notice to this code, so its handle can dangle at any
    /// time. Its nodes are looked up in the daemon's registry instead, where a
    /// module that is gone has none left to destroy.
    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        self.sync_mirror()?;
        // Dropping the module drops its link proxies first, which destroys the
        // links; its nodes go next.
        if self.take_module(id).is_none() {
            return Err(AudioError::PipeWire(format!("no delay branch {id}")));
        }
        let nodes = branch_node_ids(&self.mirror, id);
        tracing::debug!("unloading delay branch {id}: destroying nodes {nodes:?}");
        let connection = self.connection()?;
        for node in nodes {
            connection.destroy_global(node);
        }
        self.sync_mirror()
    }

    /// Set branch `id`'s delay on its capture node, in place: a `Props` param,
    /// nothing unloaded (#81). The delay is recorded once a round trip sent
    /// after the param has completed, so the branch reports what was last sent.
    ///
    /// That round trip proves the daemon has read the param, not that the node
    /// accepted it: a refusal comes back as a core `error` naming the node's
    /// proxy, which the core listener does not follow — it watches
    /// `PW_ID_CORE` only.
    fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
        let bytes = delay_props_pod(delay_ms as f32 / 1000.0)?;
        let pod = Pod::from_bytes(&bytes)
            .ok_or_else(|| AudioError::PipeWire("malformed Props param".into()))?;
        // A lost connection has cleared the modules, so a proxy found here
        // belongs to the live core.
        let (_, _, module) = self
            .modules
            .get(&id)
            .ok_or_else(|| AudioError::PipeWire(format!("no delay branch {id}")))?;
        let node = module
            .in_node
            .as_ref()
            .ok_or_else(|| AudioError::PipeWire(format!("delay branch {id} has no delay node")))?;
        node.set_param(ParamType::Props, 0, pod);
        let deadline = self.deadline;
        self.connection()?.roundtrip(deadline)?;
        self.record_module_delay(id, delay_ms)
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        // Dropping the proxy destroys the node this connection created, and
        // that removal is not someone else's (#139).
        self.release_combined_sink(sink_name);
        drop(self.take_null_sink(sink_name));
        // A partial view destroys nothing: the sync must succeed first, and
        // the modules are only forgotten once it has.
        self.sync_mirror()?;
        let branches: Vec<u32> = self
            .modules_for(sink_name)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let doomed = teardown_globals(&self.mirror, sink_name, &branches);
        for id in branches {
            self.take_module(id);
        }
        tracing::debug!("teardown of {sink_name}: destroying globals {doomed:?}");
        let connection = self.connection()?;
        for id in doomed {
            connection.destroy_global(id);
        }
        self.sync_mirror()
    }

    fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError> {
        self.sync_mirror()?;
        let deadline = self.deadline;
        self.connection()?
            .clear_stale_default_sink(sink_name, deadline)
    }

    fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError> {
        self.sync_mirror()?;
        let Some(target) = route_target(&self.mirror, sink) else {
            return Ok(None);
        };
        let deadline = self.deadline;
        let (_, routes) = self.connection()?.routes(target.device_id, deadline)?;
        // A device without the sink's route is a sink with no level, not a
        // graph that could not answer (#148): `route_of`, not `route`, which
        // errs on it for `set_sink_volume`.
        Ok(route_of(routes, target.route_device)
            .and_then(|route| volume_fraction_from_route(&route.channel_volumes)))
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

/// The loop's own state is the graph the router runs against (#147), behind
/// a [`NamedGuard`] that refuses an empty name before any of these calls
/// (#154): each method is a direct call on the loop thread, sharing the one
/// deadline the actor handed over for the message being run, and checks no
/// name itself.
impl Graph for LoopState<PwConnector> {
    fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        LoopState::sinks(self)
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        LoopState::branches(self, sink_name)
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        LoopState::create_combined_sink(self, sink_name)
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        LoopState::load_branch(self, sink_name, real_sink, latency_ms)
    }

    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        LoopState::unload_branch(self, id)
    }

    fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
        LoopState::set_branch_delay(self, id, delay_ms)
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        LoopState::teardown(self, sink_name)
    }

    fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError> {
        LoopState::clear_stale_default_sink(self, sink_name)
    }

    fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError> {
        LoopState::retarget_streams(self, sink_name)
    }

    fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError> {
        LoopState::sink_volume(self, sink)
    }

    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
        LoopState::set_sink_volume(self, sink, level)
    }
}

#[cfg(test)]
mod tests;
