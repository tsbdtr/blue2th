// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`PipeWireGraph`]: the [`Graph`] that drives PipeWire natively, from a
//! `pw_main_loop` running on a thread of its own (#79).
//!
//! The PipeWire objects are `Rc`-based and never leave that thread. The handle
//! the router owns only holds a [`pipewire::channel`] sender into it: every
//! [`Graph`] method is one [`Command`], sent in an [`Envelope`] carrying the
//! instant past which the loop thread no longer starts it (#146), and answered
//! through a reply channel. The handle waits on that channel until
//! [`COMMAND_TIMEOUT`] and [`REPLY_MARGIN`] past that instant, so a command the
//! thread started answers before its caller stops waiting.
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
use pw::link::Link;
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::metadata::Metadata;
use pw::node::{Node, NodeListener};
use pw::properties::PropertiesBox;
use pw::registry::{GlobalObject, RegistryRc};
use pw::types::ObjectType;
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{AudioError, CombineBranch};
use crate::graph::{Graph, LoadedBranch};

/// How long the loop thread gives the round trips of one command, all of them
/// together, counted from the instant it starts the command. The handle waits
/// [`REPLY_MARGIN`] longer than that past the command's `start_by`, so a
/// command answers with the daemon's error rather than with the handle's
/// timeout.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1600);

/// How long after it was sent the loop thread may still start a command
/// (#146). Time spent in the queue counts against it.
const START_BUDGET: Duration = Duration::from_millis(300);

/// How much longer than a started command's round trips the handle waits
/// (#146): what lets the answer of a command started at its `start_by` reach a
/// caller that is still waiting.
const REPLY_MARGIN: Duration = Duration::from_millis(100);

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
    ClearStaleDefaultSink {
        sink_name: String,
        reply: Reply<bool>,
    },
    RetargetStreams {
        sink_name: String,
        reply: Reply<usize>,
    },
    SinkVolume {
        sink: String,
        reply: Reply<Option<f32>>,
    },
    SetSinkVolume {
        sink: String,
        level: f32,
        reply: Reply<()>,
    },
}

impl Command {
    /// Answer [`AudioError::Expired`] on the command's own reply instead of
    /// running it, and name its variant for the log.
    ///
    /// The `match` has no wildcard arm on purpose: a new variant does not
    /// compile until it answers its expiry, where a wildcard would drop its
    /// reply and its caller would read a thread that died.
    fn expire(self) -> &'static str {
        // A reply nobody waits for any more is dropped, as in `handle`.
        match self {
            Command::Sinks { reply } => {
                let _ = reply.send(Err(AudioError::Expired));
                "Sinks"
            },
            Command::Branches { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "Branches"
            },
            Command::CreateCombinedSink { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "CreateCombinedSink"
            },
            Command::LoadBranch { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "LoadBranch"
            },
            Command::UnloadBranch { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "UnloadBranch"
            },
            Command::SetBranchDelay { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "SetBranchDelay"
            },
            Command::Teardown { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "Teardown"
            },
            Command::ClearStaleDefaultSink { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "ClearStaleDefaultSink"
            },
            Command::RetargetStreams { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "RetargetStreams"
            },
            Command::SinkVolume { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "SinkVolume"
            },
            Command::SetSinkVolume { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
                "SetSinkVolume"
            },
        }
    }
}

/// A [`Command`] on its way to the loop thread, with the instant past which
/// the thread no longer starts it (#146). The deadline travels beside the
/// command rather than in it, so reading it needs no knowledge of the variants.
pub(crate) struct Envelope {
    start_by: Instant,
    command: Command,
}

/// The handle's end of the channel into the loop thread.
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

/// The handle the router owns: a sender into the loop thread, and the means to
/// start a new thread when the previous one has died.
pub struct PipeWireGraph {
    spawn_loop: SpawnLoop,
    sender: Option<Box<dyn LoopSender>>,
    /// Where every loop thread started for this graph reports its events.
    events: Option<UnboundedSender<GraphEvent>>,
}

impl PipeWireGraph {
    /// A graph over the PipeWire daemon of the current session. Starts nothing:
    /// the loop thread is spawned, and connects, on the first command.
    pub fn spawn() -> Self {
        Self::with_loop(Box::new(spawn_loop_thread))
    }

    /// Start the loop thread now, connected at once, reporting its
    /// [`GraphEvent`]s to `events`.
    ///
    /// Called once, before any command. A thread an earlier command started is
    /// not stopped by it: dropping a `pw::channel::Sender` neither closes the
    /// channel nor wakes its loop, so that thread would keep running — and
    /// keep what it created — beside the watched one.
    pub fn watch(&mut self, events: UnboundedSender<GraphEvent>) {
        self.events = Some(events);
        // Cloned: each thread started, including one replacing a dead one,
        // holds a sender of its own.
        self.sender = Some((self.spawn_loop)(self.events.clone()));
    }

    /// A graph with no loop thread at all: every command errs at once, as when
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
        }
    }

    /// Hand `envelope` to the loop thread, starting one when there is none and
    /// replacing one that has died.
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
        // The thread is gone: a new one answers this very command.
        let fresh = spawn_loop(events.clone());
        let sent = fresh.send(envelope);
        self.sender = Some(fresh);
        sent.map_err(|_| AudioError::PipeWire("the PipeWire graph thread is not running".into()))
    }

    /// Send the command `make` builds around a fresh reply channel, and wait for
    /// the answer.
    ///
    /// The envelope is stamped here, once, before any thread is started for
    /// it: a command resent to a replacement thread keeps its `start_by`. The
    /// wait is counted from that stamp rather than from the send, so a thread
    /// that was slow to start does not lengthen it.
    ///
    /// The handle's own timeout is not [`AudioError::Expired`]: it cannot tell
    /// whether the command ran, and `Expired` says it did not.
    fn ask<R>(&mut self, make: impl FnOnce(mpsc::Sender<R>) -> Command) -> Result<R, AudioError> {
        let (reply, answer) = mpsc::channel();
        let start_by = Instant::now() + START_BUDGET;
        self.send(Envelope {
            start_by,
            command: make(reply),
        })?;
        let give_up = start_by + COMMAND_TIMEOUT + REPLY_MARGIN;
        answer
            .recv_timeout(give_up.saturating_duration_since(Instant::now()))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => AudioError::PipeWire(format!(
                    "PipeWire graph thread did not answer within {} s",
                    (START_BUDGET + COMMAND_TIMEOUT + REPLY_MARGIN).as_secs()
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
        self.ask(|reply| Command::SetBranchDelay {
            id,
            delay_ms,
            reply,
        })?
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::Teardown {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::ClearStaleDefaultSink {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError> {
        named("sink", sink_name)?;
        self.ask(|reply| Command::RetargetStreams {
            sink_name: sink_name.to_string(),
            reply,
        })?
    }

    fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError> {
        named("sink", sink)?;
        self.ask(|reply| Command::SinkVolume {
            sink: sink.to_string(),
            reply,
        })?
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
        // context stays, so the next command reconnects from it.
        self.null_sinks.clear();
        self.modules.clear();
        self.pending_links.clear();
        self.connection = None;
        self.mirror = Mirror::default();
    }
}

// ─── The production loop thread ─────────────────────────────────────────────

/// A loop that is not there: every command comes back, so the handle knows.
struct NoLoop;

impl LoopSender for NoLoop {
    fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
        Err(envelope)
    }
}

/// The handle's end into a real loop thread.
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

/// Start a loop thread; [`NoLoop`] when the thread cannot even be started.
fn spawn_loop_thread(events: Option<UnboundedSender<GraphEvent>>) -> Box<dyn LoopSender> {
    let (sender, receiver) = pw::channel::channel::<Envelope>();
    match std::thread::Builder::new()
        .name("pipewire-graph".into())
        .spawn(move || run_loop_thread(receiver, events))
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
///
/// A watched loop — one handed `events` — connects at once and, once the
/// connection is lost, reconnects on its own after [`reconnect_delay`], so a
/// restarted daemon is noticed without waiting for a command (#80). An
/// unwatched one connects on its first command, as before.
fn run_loop_thread(
    receiver: pw::channel::Receiver<Envelope>,
    events: Option<UnboundedSender<GraphEvent>>,
) {
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
    let inbox: Rc<RefCell<VecDeque<Envelope>>> = Rc::default();
    let _attached = receiver.attach(mainloop.loop_(), {
        let inbox = Rc::clone(&inbox);
        move |envelope| inbox.borrow_mut().push_back(envelope)
    });
    let watched = events.is_some();
    let mut state = LoopState::new(PwConnector {
        mainloop: mainloop.clone(),
        events,
    });
    let mut reconnect = ReconnectWatch::new(Instant::now());
    loop {
        if watched && reconnect.attempt_due(state.is_connected(), Instant::now()) {
            match state.reconnect() {
                Ok(()) => reconnect.connected(state.connector.events.as_ref()),
                // Connected, but the daemon did not answer the re-read in time:
                // the connection is kept, as a command keeps it, and the next
                // command reads the registry again.
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
        let timeout = match reconnect.wait(state.is_connected(), Instant::now()) {
            Some(left) if watched => Timeout::Finite(left),
            _ => Timeout::Infinite,
        };
        mainloop.loop_().iterate(timeout);
        if state.forget_a_lost_connection() {
            reconnect.lost(Instant::now());
        }
        state.wire_waiting_branches();
        // A command that waited behind the reconnect attempt or the wiring above
        // has spent that time out of its start budget (#146).
        drain_inbox(&inbox, Instant::now, |command| {
            handle(&mut state, command);
            if state.forget_a_lost_connection() {
                reconnect.lost(Instant::now());
            }
        });
        // A command reconnects on its own: that is a reconnection too.
        if state.is_connected() {
            reconnect.connected(state.connector.events.as_ref());
        }
    }
}

/// The command of `envelope` when the loop thread may still start it at `now`.
/// Past its `start_by` there is none: the command is not run, and its own
/// reply receives [`AudioError::Expired`].
fn start_or_expire(envelope: Envelope, now: Instant) -> Option<Command> {
    let Envelope { start_by, command } = envelope;
    if now <= start_by {
        return Some(command);
    }
    let late = now.duration_since(start_by);
    let name = command.expire();
    tracing::warn!(
        "graph command {name} expired: taken out of the queue {} ms past its start_by",
        late.as_millis()
    );
    None
}

/// Take the envelopes out of `inbox` one at a time until it is empty, reading
/// `now` once for each as it comes out, and hand `run` the commands
/// [`start_or_expire`] yields. `inbox` is not borrowed while `run` runs: a
/// command iterates the main loop, and the channel callback queues into it.
fn drain_inbox(
    inbox: &RefCell<VecDeque<Envelope>>,
    mut now: impl FnMut() -> Instant,
    mut run: impl FnMut(Command),
) {
    loop {
        let next = inbox.borrow_mut().pop_front();
        let Some(envelope) = next else {
            break;
        };
        if let Some(command) = start_or_expire(envelope, now()) {
            run(command);
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
    /// the count matters only once a connection is lost, and a command can
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
    /// channel is ignored — the consumer is gone.
    fn connected(&mut self, events: Option<&UnboundedSender<GraphEvent>>) {
        if !self.owes_reconnected {
            return;
        }
        self.owes_reconnected = false;
        tracing::info!("PipeWire connection back");
        if let Some(events) = events {
            let _ = events.send(GraphEvent::Reconnected);
        }
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
    /// event that completes it rather than on the next command.
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
            let _ = reply.send(state.set_branch_delay(id, delay_ms));
        },
        Command::Teardown { sink_name, reply } => {
            let _ = reply.send(state.teardown(&sink_name));
        },
        Command::ClearStaleDefaultSink { sink_name, reply } => {
            let _ = reply.send(state.clear_stale_default_sink(&sink_name));
        },
        Command::RetargetStreams { sink_name, reply } => {
            let _ = reply.send(state.retarget_streams(&sink_name));
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
    use std::cell::Cell;
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
    // which is a delay of zero and not "no branch", up to `MAX_OFFSET_MS`. The
    // near miss: 5 ms is `0.005`, which an unpadded millisecond part writes
    // as `0.5`, a hundred times too long.
    #[test]
    fn test_delay_chain_module_args_carries_the_offset_in_seconds() {
        for (delay_ms, expected) in [(0, "0.000"), (5, "0.005"), (120, "0.120"), (750, "0.750")] {
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

    // Criterion (non-nominal): a delay the branch's `delay` node cannot hold is
    // refused before a param is built — below zero, beyond
    // `MAX_DELAY_SECONDS`, an infinity, and NaN, which a check written as
    // `s < 0.0 || s > MAX` lets through. The near misses: both ends of the
    // range are accepted.
    #[test]
    fn test_delay_props_pod_refuses_a_delay_the_branch_cannot_hold() {
        for seconds in [
            -0.001_f32,
            MAX_DELAY_SECONDS + 0.001,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert!(
                matches!(delay_props_pod(seconds), Err(AudioError::PipeWire(_))),
                "{seconds} s was accepted"
            );
        }
        for seconds in [0.0_f32, MAX_DELAY_SECONDS] {
            assert!(delay_props_pod(seconds).is_ok(), "{seconds} s was refused");
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

    // ─── teardown_globals ────────────────────────────────────────────────────

    // Criterion (teardown): a teardown destroys both nodes of each of
    // the graph's own branches, found by their exact names, next to the
    // combined sink the foreign rule takes. The near misses: branch 10, whose
    // names open with `blue2th_delay.1`, and `blue2th_combined_old`, which
    // opens with the combined sink's name, are spared; and without the listed
    // ids the branches are not found at all — their capture side targets
    // nothing, so the foreign rule alone never reaches them.
    #[test]
    fn test_teardown_globals_takes_the_listed_branches_and_the_combined_sink() {
        let mirror = liveness_mirror(&[]);

        assert_eq!(teardown_globals(&mirror, COMBINED, &[1]), vec![61, 90, 91]);
        assert_eq!(
            teardown_globals(&mirror, COMBINED, &[]),
            vec![61],
            "the foreign rule alone never finds a delay branch"
        );
        assert_eq!(
            teardown_globals(&mirror, "", &[1]),
            vec![90, 91],
            "an empty sink name takes no combined sink"
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

    // ─── configured_default_names ────────────────────────────────────────────

    /// `default.configured.audio.sink` as `pw-metadata -n default 0` showed it
    /// on the dev PC on 2026-09-24 and 2026-09-26, left there by earlier
    /// versions of blue2th: `update: id:0 key:'default.configured.audio.sink'
    /// value:'{"name":"blue2th_combined"}' type:'Spa:String:JSON'`.
    const CAPTURED_STALE_DEFAULT: &str = r#"{"name":"blue2th_combined"}"#;

    /// `default.audio.sink` from the same capture, once the server had
    /// stopped: the shape of a value naming a real speaker.
    const CAPTURED_SPEAKER_DEFAULT: &str = r#"{"name":"bluez_output.80_99_E7_63_50_29.1"}"#;

    // Criterion: the value an earlier version of blue2th left, naming the
    // combined sink exactly, is one to clear.
    #[test]
    fn test_configured_default_names_the_combined_sink_exactly() {
        assert!(configured_default_names(
            Some(CAPTURED_STALE_DEFAULT),
            COMBINED
        ));
    }

    // Criterion (non-nominal): a configured default naming another sink — a
    // speaker, the PC's own output — is never cleared.
    #[test]
    fn test_configured_default_names_leaves_another_sink() {
        assert!(!configured_default_names(
            Some(CAPTURED_SPEAKER_DEFAULT),
            COMBINED
        ));
        assert!(!configured_default_names(
            Some(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#),
            COMBINED
        ));
        // The same value is recognised when it is the one asked about: the
        // answer above depends on the name, not on the shape.
        assert!(configured_default_names(
            Some(CAPTURED_SPEAKER_DEFAULT),
            "bluez_output.80_99_E7_63_50_29.1"
        ));
    }

    // Criterion (guard, exact name, never a prefix): a sink whose name starts
    // with, ends with or contains the combined sink's is the user's other
    // sink. A `starts_with` or `contains` check would clear it.
    #[test]
    fn test_configured_default_names_never_matches_a_longer_name() {
        for longer in [
            r#"{"name":"blue2th_combined_old"}"#,
            r#"{"name":"old_blue2th_combined"}"#,
            r#"{"name":"xblue2th_combinedx"}"#,
        ] {
            assert!(
                !configured_default_names(Some(longer), COMBINED),
                "{longer} names another sink"
            );
        }
        // Nor the other way round: a shorter name is not the combined sink.
        assert!(!configured_default_names(
            Some(r#"{"name":"blue2th"}"#),
            COMBINED
        ));
        // The exact name, in the same shape, is.
        assert!(configured_default_names(
            Some(r#"{"name":"blue2th_combined"}"#),
            COMBINED
        ));
    }

    // Criterion (non-nominal): an absent, empty or malformed value — anything
    // that is not the JSON object WirePlumber writes — touches nothing, even
    // when the combined sink's name appears in it verbatim.
    #[test]
    fn test_configured_default_names_of_a_malformed_or_absent_value_is_false() {
        for value in [
            None,
            Some(""),
            Some("not json"),
            Some(r#"{"nom":"blue2th_combined"}"#),
            // The bare name, and the name as a JSON string: a substring check
            // says yes to both, a reader of `{"name": …}` to neither.
            Some("blue2th_combined"),
            Some(r#""blue2th_combined""#),
            // Truncated.
            Some(r#"{"name":"blue2th_combined""#),
            // The name is not a string.
            Some(r#"{"name":["blue2th_combined"]}"#),
        ] {
            assert!(
                !configured_default_names(value, COMBINED),
                "{value:?} is not a configured default naming the combined sink"
            );
        }
        assert!(configured_default_names(
            Some(CAPTURED_STALE_DEFAULT),
            COMBINED
        ));
    }

    /// `default.configured.audio.sink` as `wpctl set-default` writes it, with
    /// spaces, captured with `pw-metadata -n default 0` on the dev PC on
    /// 2026-09-27. Earlier versions of blue2th wrote the same object without
    /// them.
    const CAPTURED_WPCTL_DEFAULT: &str =
        r#"{ "name": "alsa_output.pci-0000_c4_00.6.HiFi__Speaker__sink" }"#;

    // The value is read as JSON, not matched as text: the spaced form
    // `wpctl set-default` writes names a sink as surely as the compact one, so
    // a combined sink written that way is cleared, and the PC's speakers
    // written that way are left alone.
    #[test]
    fn test_configured_default_names_reads_the_spaced_form_wpctl_writes() {
        assert!(configured_default_names(
            Some(r#"{ "name": "blue2th_combined" }"#),
            COMBINED
        ));
        assert!(!configured_default_names(
            Some(CAPTURED_WPCTL_DEFAULT),
            COMBINED
        ));
        assert!(configured_default_names(
            Some(CAPTURED_WPCTL_DEFAULT),
            "alsa_output.pci-0000_c4_00.6.HiFi__Speaker__sink"
        ));
    }

    // Criterion (guard, empty sink name): two empty values must not compare
    // equal into a deletion, and an empty name matches nothing.
    #[test]
    fn test_configured_default_names_of_an_empty_sink_name_is_false() {
        assert!(!configured_default_names(Some(r#"{"name":""}"#), ""));
        assert!(!configured_default_names(Some(CAPTURED_STALE_DEFAULT), ""));
        assert!(!configured_default_names(None, ""));
        // A non-empty name in the same position is answered.
        assert!(configured_default_names(
            Some(CAPTURED_STALE_DEFAULT),
            COMBINED
        ));
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

    // Criterion (#148): the route of a sink is the one whose `device` is the
    // sink's `card.profile.device`, and a device without it has none — which
    // `sink_volume` answers as no level and `set_sink_volume` refuses. The
    // near miss is the first route: its `index` is the wanted value, so a
    // lookup on the index instead of the device picks it.
    #[test]
    fn test_route_of_matches_the_route_device_not_the_index() {
        let route = |index, device, volume| Route {
            index,
            device,
            channel_volumes: vec![volume],
        };
        let routes = || vec![route(1, 0, 0.1), route(0, 1, 0.2), route(2, 2, 0.3)];

        let found = route_of(routes(), 1);
        assert_eq!(
            found.map(|r| (r.index, r.device, r.channel_volumes)),
            Some((0, 1, vec![0.2])),
            "the route of device 1 is the second one"
        );
        assert!(route_of(routes(), 7).is_none(), "no route for device 7");
        assert!(
            route_of(Vec::new(), 1).is_none(),
            "no route on a device without any"
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
    // — which `set_sink_volume` refuses and `sink_volume` answers as a sink
    // with no level, `Ok(None)` (#148) — rather than failing to parse.
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

    impl LoopSender for mpsc::Sender<Envelope> {
        fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
            mpsc::Sender::send(self, envelope).map_err(|e| e.0)
        }
    }

    /// A loop thread that answers every command as a healthy graph holding
    /// `sinks` would, recording each command it received: its name, then its
    /// arguments in declaration order.
    fn answering_loop(
        sinks: Vec<String>,
        received: Arc<Mutex<Vec<String>>>,
    ) -> Box<dyn LoopSender> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        std::thread::spawn(move || {
            for envelope in rx {
                let mut log = received.lock().unwrap();
                match envelope.command {
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
                    Command::ClearStaleDefaultSink { sink_name, reply } => {
                        log.push(format!("clear_stale_default_sink {sink_name}"));
                        // `true`, which no stub answers: the handle must hand
                        // back what the loop said.
                        let _ = reply.send(Ok(true));
                    },
                    Command::RetargetStreams { sink_name, reply } => {
                        log.push(format!("retarget_streams {sink_name}"));
                        // 3, which no stub answers: the handle must hand back
                        // the count the loop said.
                        let _ = reply.send(Ok(3));
                    },
                    Command::SinkVolume { sink, reply } => {
                        log.push(format!("sink_volume {sink}"));
                        let _ = reply.send(Ok(Some(0.5)));
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
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert_eq!(graph.sinks().ok(), Some(vec![SPEAKER.to_string()]));
        assert_eq!(graph.sink_volume(SPEAKER).ok(), Some(Some(0.5)));
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
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert!(graph.sinks().is_ok());
        assert!(graph.branches(COMBINED).is_ok());
        assert!(graph.create_combined_sink(COMBINED).is_ok());
        assert!(graph.load_branch(COMBINED, SPEAKER, 170).is_ok());
        assert!(graph.unload_branch(7).is_ok());
        assert!(graph.set_branch_delay(9, 250).is_ok());
        assert!(graph.teardown(COMBINED).is_ok());
        assert_eq!(graph.clear_stale_default_sink(COMBINED).ok(), Some(true));
        assert_eq!(graph.retarget_streams(COMBINED).ok(), Some(3));
        assert_eq!(graph.sink_volume(SPEAKER).ok(), Some(Some(0.5)));
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
                format!("clear_stale_default_sink {COMBINED}"),
                format!("retarget_streams {COMBINED}"),
                format!("sink_volume {SPEAKER}"),
                format!("set_sink_volume {SPEAKER} 0.25"),
            ]
        );
    }

    // Criterion (#139, guard, the empty value): `retarget_streams` refuses an
    // empty sink name before the loop thread sees it, as every other named
    // command does; `streams_targeting` refuses it again on the loop side
    // (`test_streams_targeting_of_an_empty_sink_name_takes_nothing`). The
    // control: the combined sink's name reaches the loop, which answers.
    #[test]
    fn test_retarget_streams_refuses_an_empty_sink_name_before_the_loop() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            answering_loop(vec![SPEAKER.to_string()], Arc::clone(&log))
        }));

        assert!(matches!(
            graph.retarget_streams(""),
            Err(AudioError::PipeWire(_))
        ));
        assert!(
            received.lock().unwrap().is_empty(),
            "an empty name reached the loop: {:?}",
            received.lock().unwrap()
        );

        assert_eq!(graph.retarget_streams(COMBINED).ok(), Some(3));
        assert_eq!(
            *received.lock().unwrap(),
            vec![format!("retarget_streams {COMBINED}")],
            "control: the combined sink's name reaches the loop"
        );
    }

    // Criteria (#146): a loop thread that never answers is an
    // `AudioError::PipeWire` naming the 2 s wait, returned no earlier than 2 s
    // after the send and not much later; and the handle stops waiting 1.7 s
    // past the `start_by` it stamped — the 1.6 s a started command may take,
    // and the margin — which is what lets a command started in time answer
    // before its caller gives up. Every bound is a literal.
    //
    // Guard (the handle's own timeout is never `Expired`): the near miss is
    // this parked loop, which never answers. The handle cannot tell whether the
    // command ran, and `Expired` promises it did not: the answer is matched as
    // the `PipeWire` variant with its whole message, which `Expired` is not.
    //
    // The upper bounds leave 250 ms of scheduling slack. They sit under 2.3 s
    // on purpose: that is a wait counted from `start_by` instead of the send.
    #[test]
    fn test_a_command_without_an_answer_errs_2_s_after_the_send_and_is_not_expired() {
        // The receivers are kept alive and never read: the thread is "stuck".
        let parked: Arc<Mutex<Vec<mpsc::Receiver<Envelope>>>> = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&parked);
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            let (tx, rx) = mpsc::channel::<Envelope>();
            keep.lock().unwrap().push(rx);
            Box::new(tx) as Box<dyn LoopSender>
        }));

        let started = Instant::now();
        let answer = graph.sinks();
        let gave_up = Instant::now();
        let elapsed = gave_up.duration_since(started);

        assert!(
            elapsed >= Duration::from_secs(2),
            "the handle waited the whole 2 s, waited {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2250),
            "and not much more, waited {elapsed:?}"
        );
        assert!(
            matches!(
                &answer,
                Err(AudioError::PipeWire(m)) if m == "PipeWire graph thread did not answer within 2 s"
            ),
            "the handle's own timeout is an untyped error naming the wait, got {answer:?}"
        );

        // The command the parked loop never read still carries its stamp.
        let start_by = parked
            .lock()
            .unwrap()
            .first()
            .and_then(|rx| rx.try_recv().ok())
            .map(|envelope| envelope.start_by);
        assert!(start_by.is_some(), "the command reached the parked loop");
        let past_start_by = gave_up.duration_since(start_by.unwrap());
        assert!(
            past_start_by >= Duration::from_millis(1700),
            "the wait ends 1.6 s and the margin past start_by, ended {past_start_by:?} past it"
        );
        assert!(
            past_start_by < Duration::from_millis(1950),
            "and not much later, ended {past_start_by:?} past start_by"
        );
    }

    // Criterion: a loop thread that took the command and died without answering
    // is an error at once — a dropped reply is not a slow one.
    #[test]
    fn test_a_dropped_reply_errs_without_waiting_for_the_timeout() {
        let mut graph = PipeWireGraph::with_loop(Box::new(|_| {
            let (tx, rx) = mpsc::channel::<Envelope>();
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
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                // The first thread is already dead: its receiver is gone.
                let (tx, rx) = mpsc::channel::<Envelope>();
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
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
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
            graph.clear_stale_default_sink(""),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            graph.set_sink_volume("", 0.5),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            graph.sink_volume(""),
            Err(AudioError::PipeWire(_))
        ));

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
    // errs at once, a volume read included (#148): the graph cannot tell.
    #[test]
    fn test_detached_graph_errs_at_once_without_a_loop() {
        let mut graph = PipeWireGraph::detached();
        let started = Instant::now();

        let sinks = graph.sinks();
        let teardown = graph.teardown(COMBINED);
        let retune = graph.set_branch_delay(1, 120);
        let clear = graph.clear_stale_default_sink(COMBINED);
        let volume = graph.sink_volume(SPEAKER);

        assert!(
            matches!(&sinks, Err(AudioError::PipeWire(m)) if m.contains("not running")),
            "got {sinks:?}"
        );
        assert!(matches!(teardown, Err(AudioError::PipeWire(_))));
        assert!(
            matches!(retune, Err(AudioError::PipeWire(_))),
            "a retune goes through the loop like every command, got {retune:?}"
        );
        assert!(
            matches!(clear, Err(AudioError::PipeWire(_))),
            "a clear goes through the loop like every command, got {clear:?}"
        );
        assert!(
            matches!(&volume, Err(AudioError::PipeWire(m)) if m.contains("not running")),
            "a volume read goes through the loop like every command, got {volume:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no timeout is waited out, waited {:?}",
            started.elapsed()
        );
    }

    // ─── #148: a level read the graph could not answer is an `Err` ─────────

    /// A speaker sink the volume loop answers with a level.
    const LEVELLED: &str = "bluez_output.AA_BB_CC_DD_EE_0A.1";
    /// A speaker sink the volume loop answers with no level: no route, or a
    /// route with no channel volume.
    const NO_LEVEL: &str = "bluez_output.AA_BB_CC_DD_EE_0B.1";
    /// A speaker sink the volume loop answers with its own error.
    const BROKEN: &str = "bluez_output.AA_BB_CC_DD_EE_0C.1";
    /// The loop's own error for `BROKEN`, which the handle must hand back.
    const LOOP_ERROR: &str = "the speaker's Route enumeration timed out";

    /// A loop thread that answers `SinkVolume` by sink name and records each
    /// sink it was asked about. A name it does not know — the empty one
    /// included — is `Ok(None)`, as the real loop's mirror lookup, matching
    /// nothing, would answer. Every other command is dropped unanswered.
    fn volume_loop(received: Arc<Mutex<Vec<String>>>) -> Box<dyn LoopSender> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        std::thread::spawn(move || {
            for envelope in rx {
                if let Command::SinkVolume { sink, reply } = envelope.command {
                    let answer = match sink.as_str() {
                        LEVELLED => Ok(Some(0.42)),
                        BROKEN => Err(AudioError::PipeWire(LOOP_ERROR.to_string())),
                        _ => Ok(None),
                    };
                    received.lock().unwrap().push(sink);
                    let _ = reply.send(answer);
                }
            }
        });
        Box::new(tx)
    }

    /// A graph whose every loop thread is a [`volume_loop`] recording into
    /// the returned log.
    fn volume_graph() -> (PipeWireGraph, Arc<Mutex<Vec<String>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let graph = PipeWireGraph::with_loop(Box::new(move |_| volume_loop(Arc::clone(&log))));
        (graph, received)
    }

    // Criterion (#148): the handle hands the loop's own answer back as is —
    // `Ok(Some(_))`, `Ok(None)` and `Err` each, the loop's message included.
    // The near miss is `BROKEN`: swallowing the loop's `Err` into `Ok(None)`
    // passes the other two reads.
    #[test]
    fn test_sink_volume_hands_back_the_loop_s_answer_as_is() {
        let (mut graph, received) = volume_graph();

        assert_eq!(graph.sink_volume(LEVELLED).ok(), Some(Some(0.42)));
        assert_eq!(graph.sink_volume(NO_LEVEL).ok(), Some(None));
        let broken = graph.sink_volume(BROKEN);
        assert!(
            matches!(&broken, Err(AudioError::PipeWire(m)) if m == LOOP_ERROR),
            "the loop's own error is handed back, got {broken:?}"
        );
        assert_eq!(*received.lock().unwrap(), vec![LEVELLED, NO_LEVEL, BROKEN]);
    }

    // Criterion (#148): a loop thread that does not answer a level read
    // within the reply timeout is an `Err` naming the timeout — the graph
    // cannot tell — not a sink without a level.
    #[test]
    fn test_sink_volume_without_an_answer_errs_after_the_timeout() {
        // The receivers are kept alive and never read: the thread is "stuck".
        let parked: Arc<Mutex<Vec<mpsc::Receiver<Envelope>>>> = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&parked);
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            let (tx, rx) = mpsc::channel::<Envelope>();
            keep.lock().unwrap().push(rx);
            Box::new(tx) as Box<dyn LoopSender>
        }));

        let answer = graph.sink_volume(SPEAKER);

        assert!(
            matches!(&answer, Err(AudioError::PipeWire(m)) if m.contains("did not answer within 2 s")),
            "a timed-out read is an Err naming the timeout, got {answer:?}"
        );
    }

    // Criterion (#148): a loop thread that took the level read and dropped it
    // unanswered is an `Err`, at once.
    #[test]
    fn test_sink_volume_with_a_dropped_reply_errs_at_once() {
        let mut graph = PipeWireGraph::with_loop(Box::new(|_| {
            let (tx, rx) = mpsc::channel::<Envelope>();
            std::thread::spawn(move || {
                // Take one command and drop it, reply sender included.
                let _ = rx.recv();
            });
            Box::new(tx) as Box<dyn LoopSender>
        }));

        let started = Instant::now();
        let answer = graph.sink_volume(SPEAKER);

        assert!(
            matches!(&answer, Err(AudioError::PipeWire(m)) if m.contains("dropped the command")),
            "a dropped read is an Err, got {answer:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dropped reply is known at once, waited {:?}",
            started.elapsed()
        );
    }

    // Criterion (#148, guard, the empty value): an empty sink name is an
    // `Err` and sends no command. The near miss is the empty name itself:
    // without the guard it reaches the loop, which matches nothing and
    // answers `Ok(None)` — a sink with no level, to the caller. The control:
    // a real sink name reaches the loop, which answers.
    #[test]
    fn test_sink_volume_refuses_an_empty_sink_name_before_the_loop() {
        let (mut graph, received) = volume_graph();

        let refused = graph.sink_volume("");
        assert!(
            matches!(&refused, Err(AudioError::PipeWire(m)) if m.contains("empty")),
            "an empty name is refused, got {refused:?}"
        );
        assert!(
            received.lock().unwrap().is_empty(),
            "an empty name reached the loop: {:?}",
            received.lock().unwrap()
        );

        assert_eq!(graph.sink_volume(LEVELLED).ok(), Some(Some(0.42)));
        assert_eq!(
            *received.lock().unwrap(),
            vec![LEVELLED],
            "control: a real sink name reaches the loop"
        );
    }

    // ─── #146: a command not started by its deadline expires ────────────────
    //
    // Every instant below is one base `Instant` plus a literal offset, and
    // every expected duration is a literal: none is computed from
    // `START_BUDGET` or `REPLY_MARGIN`, which a test would then agree with
    // whatever they are worth.

    // Criterion (#146): `START_BUDGET` is 300 ms.
    #[test]
    fn test_start_budget_is_300_ms() {
        assert_eq!(START_BUDGET, Duration::from_millis(300));
    }

    // Criterion (#146): `REPLY_MARGIN` is 100 ms.
    #[test]
    fn test_reply_margin_is_100_ms() {
        assert_eq!(REPLY_MARGIN, Duration::from_millis(100));
    }

    // Criterion (#146): with `COMMAND_TIMEOUT` the two add up to a wait of
    // exactly 2 s. What the handle really waits is measured in
    // `test_a_command_without_an_answer_errs_2_s_after_the_send_and_is_not_expired`.
    #[test]
    fn test_start_budget_command_timeout_and_reply_margin_add_up_to_2_s() {
        assert_eq!(
            START_BUDGET + COMMAND_TIMEOUT + REPLY_MARGIN,
            Duration::from_secs(2)
        );
    }

    // Criterion (#146): `REPLY_MARGIN` is greater than zero, so the handle's
    // wait ends strictly after `start_by + COMMAND_TIMEOUT` and a command
    // started in time answers before its caller stops waiting. The invariant
    // itself, beside the literal above — it holds for many wrong values.
    #[test]
    fn test_a_started_command_s_round_trips_end_before_the_handle_stops_waiting() {
        assert!(REPLY_MARGIN > Duration::ZERO);
    }

    /// The eleven variants of [`Command`], in declaration order.
    const VARIANTS: [&str; 11] = [
        "Sinks",
        "Branches",
        "CreateCombinedSink",
        "LoadBranch",
        "UnloadBranch",
        "SetBranchDelay",
        "Teardown",
        "ClearStaleDefaultSink",
        "RetargetStreams",
        "SinkVolume",
        "SetSinkVolume",
    ];

    /// The name of `command`'s variant. Exhaustive on purpose, with no
    /// wildcard arm: a twelfth variant does not compile here until
    /// [`VARIANTS`] and the tables reading it name that variant too.
    fn variant(command: &Command) -> &'static str {
        match command {
            Command::Sinks { .. } => "Sinks",
            Command::Branches { .. } => "Branches",
            Command::CreateCombinedSink { .. } => "CreateCombinedSink",
            Command::LoadBranch { .. } => "LoadBranch",
            Command::UnloadBranch { .. } => "UnloadBranch",
            Command::SetBranchDelay { .. } => "SetBranchDelay",
            Command::Teardown { .. } => "Teardown",
            Command::ClearStaleDefaultSink { .. } => "ClearStaleDefaultSink",
            Command::RetargetStreams { .. } => "RetargetStreams",
            Command::SinkVolume { .. } => "SinkVolume",
            Command::SetSinkVolume { .. } => "SetSinkVolume",
        }
    }

    /// What a reply channel holds right now.
    #[derive(Debug, PartialEq)]
    enum Held {
        /// Nothing was sent, and the command — its reply sender — is alive.
        Nothing,
        /// Nothing was sent, and the command is gone.
        Dropped,
        /// `Err(AudioError::Expired)`.
        Expired,
        /// Any other answer.
        Other(String),
    }

    /// Take what `answer` holds, without waiting.
    fn held<R: std::fmt::Debug>(answer: &mpsc::Receiver<Result<R, AudioError>>) -> Held {
        match answer.try_recv() {
            Err(mpsc::TryRecvError::Empty) => Held::Nothing,
            Err(mpsc::TryRecvError::Disconnected) => Held::Dropped,
            Ok(Err(AudioError::Expired)) => Held::Expired,
            Ok(other) => Held::Other(format!("{other:?}")),
        }
    }

    /// An envelope around a `SetSinkVolume` of [`SPEAKER`] to `level`, to start
    /// by `start_by`, and the receiver of its reply. The level tells the
    /// commands of one queue apart.
    fn volume_envelope(
        start_by: Instant,
        level: f32,
    ) -> (Envelope, mpsc::Receiver<Result<(), AudioError>>) {
        let (reply, answer) = mpsc::channel();
        let envelope = Envelope {
            start_by,
            command: Command::SetSinkVolume {
                sink: SPEAKER.to_string(),
                level,
                reply,
            },
        };
        (envelope, answer)
    }

    /// The level of each command in `ran`, in order; `None` for one that is
    /// not a `SetSinkVolume`.
    fn levels(ran: &[Command]) -> Vec<Option<f32>> {
        ran.iter()
            .map(|command| match command {
                Command::SetSinkVolume { level, .. } => Some(*level),
                _ => None,
            })
            .collect()
    }

    // Criterion (#146): a command taken out of the queue at `now > start_by`
    // is not run and its reply receives `Err(AudioError::Expired)` — here one
    // nanosecond past, the smallest "strictly past" an `Instant` can hold. It
    // answers once, and the command is gone with it.
    #[test]
    fn test_start_or_expire_one_nanosecond_past_start_by_answers_expired_and_yields_nothing() {
        let base = Instant::now();
        let (envelope, answer) = volume_envelope(base + Duration::from_millis(300), 0.25);

        let yielded = start_or_expire(envelope, base + Duration::from_nanos(300_000_001));

        assert!(yielded.is_none(), "an expired command is not handed on");
        assert_eq!(held(&answer), Held::Expired);
        assert_eq!(
            held(&answer),
            Held::Dropped,
            "one answer, then the command is gone"
        );
    }

    /// Hand `start_or_expire` a `LoadBranch` to start by `start_by`, at `now`,
    /// and require it back as it went in — every field, and the reply channel
    /// it came with — with nothing sent on that reply.
    fn assert_handed_on_untouched(start_by: Instant, now: Instant) {
        let (reply, answer) = mpsc::channel();
        let envelope = Envelope {
            start_by,
            command: Command::LoadBranch {
                sink_name: COMBINED.to_string(),
                real_sink: SPEAKER.to_string(),
                latency_ms: 170,
                reply,
            },
        };

        let yielded = start_or_expire(envelope, now);

        let fields = match yielded {
            Some(Command::LoadBranch {
                sink_name,
                real_sink,
                latency_ms,
                reply,
            }) => Some((sink_name, real_sink, latency_ms, reply)),
            _ => None,
        };
        assert!(
            fields.is_some(),
            "a command still in time is handed on, as the variant it went in as"
        );
        let (sink_name, real_sink, latency_ms, reply) = fields.unwrap();
        assert_eq!(
            held(&answer),
            Held::Nothing,
            "the check itself sends nothing on the reply"
        );
        assert_eq!(
            (sink_name.as_str(), real_sink.as_str(), latency_ms),
            (COMBINED, SPEAKER, 170)
        );
        // The reply handed on is the one the command came with.
        assert!(reply.send(Ok(())).is_ok());
        assert!(matches!(answer.try_recv(), Ok(Ok(()))));
    }

    // Criterion (#146, guard): expired means strictly past `start_by`. The
    // near miss is `now == start_by`, which a `>=` check expires: it is handed
    // on untouched, and the check sends nothing on its reply.
    #[test]
    fn test_start_or_expire_at_start_by_hands_the_command_on_untouched_and_answers_nothing() {
        let base = Instant::now();

        assert_handed_on_untouched(
            base + Duration::from_millis(300),
            base + Duration::from_millis(300),
        );
    }

    // Criterion (#146): a command taken out earlier than `start_by` is handed
    // on untouched — one nanosecond before it.
    #[test]
    fn test_start_or_expire_one_nanosecond_before_start_by_hands_the_command_on_untouched() {
        let base = Instant::now();

        assert_handed_on_untouched(
            base + Duration::from_millis(300),
            base + Duration::from_nanos(299_999_999),
        );
    }

    // Criterion (#146): a command taken out earlier than `start_by` is handed
    // on untouched — with its whole budget left, the nominal case.
    #[test]
    fn test_start_or_expire_with_its_whole_budget_left_hands_the_command_on_untouched() {
        let base = Instant::now();

        assert_handed_on_untouched(base + Duration::from_millis(300), base);
    }

    /// Expire the command `make` builds — taken out one nanosecond past its
    /// `start_by` — and report its variant, whether the check handed it on,
    /// and what its reply holds.
    fn expire<R: std::fmt::Debug>(
        make: impl FnOnce(Reply<R>) -> Command,
    ) -> (&'static str, bool, Held) {
        let base = Instant::now();
        let (reply, answer) = mpsc::channel();
        let command = make(reply);
        let name = variant(&command);
        let yielded = start_or_expire(
            Envelope {
                start_by: base + Duration::from_millis(300),
                command,
            },
            base + Duration::from_nanos(300_000_001),
        );
        // Read while `yielded` is alive: a command handed on anyway still
        // holds its reply sender, and reads as `Nothing`, not as `Dropped`.
        let reply_holds = held(&answer);
        (name, yielded.is_some(), reply_holds)
    }

    // Criterion (#146, guard): every `Command` variant, all eleven, answers
    // `Err(AudioError::Expired)` on its own reply channel when expired. The
    // near miss is a variant reaching an arm that drops its reply: its caller
    // would read "dropped the command without answering", a `PipeWire` error
    // — `Held::Dropped` here, where `Held::Expired` is wanted, per variant.
    // The replies carry six different types, so each row is its own channel.
    #[test]
    fn test_start_or_expire_answers_expired_on_the_reply_of_each_of_the_eleven_commands() {
        let expired = [
            expire(|reply| Command::Sinks { reply }),
            expire(|reply| Command::Branches {
                sink_name: COMBINED.to_string(),
                reply,
            }),
            expire(|reply| Command::CreateCombinedSink {
                sink_name: COMBINED.to_string(),
                reply,
            }),
            expire(|reply| Command::LoadBranch {
                sink_name: COMBINED.to_string(),
                real_sink: SPEAKER.to_string(),
                latency_ms: 170,
                reply,
            }),
            expire(|reply| Command::UnloadBranch { id: 7, reply }),
            expire(|reply| Command::SetBranchDelay {
                id: 9,
                delay_ms: 250,
                reply,
            }),
            expire(|reply| Command::Teardown {
                sink_name: COMBINED.to_string(),
                reply,
            }),
            expire(|reply| Command::ClearStaleDefaultSink {
                sink_name: COMBINED.to_string(),
                reply,
            }),
            expire(|reply| Command::RetargetStreams {
                sink_name: COMBINED.to_string(),
                reply,
            }),
            expire(|reply| Command::SinkVolume {
                sink: SPEAKER.to_string(),
                reply,
            }),
            expire(|reply| Command::SetSinkVolume {
                sink: SPEAKER.to_string(),
                level: 0.25,
                reply,
            }),
        ];

        let names: Vec<&str> = expired.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(names, VARIANTS, "the table names every variant, once");
        for (name, handed_on, reply) in &expired {
            assert!(!handed_on, "{name} was handed on although expired");
            assert_eq!(*reply, Held::Expired, "{name} did not answer its expiry");
        }
    }

    // Criterion (#146, guard): an expired command is never run — the runner
    // is never called for it. The near miss is a check that answers `Expired`
    // and then hands the command on anyway: the two replies alone would look
    // right, so the runner's own log is read, and holds only the command that
    // was still in time. That third command is the control: the drain goes on
    // past an expiry, and answers nothing for a command it handed to the
    // runner.
    #[test]
    fn test_drain_inbox_never_runs_an_expired_command_and_goes_on_to_the_next() {
        let base = Instant::now();
        let (late, late_answer) = volume_envelope(base + Duration::from_millis(300), 0.25);
        let (later, later_answer) = volume_envelope(base + Duration::from_millis(400), 0.5);
        let (in_time, in_time_answer) = volume_envelope(base + Duration::from_millis(900), 0.75);
        let inbox = RefCell::new(VecDeque::from([late, later, in_time]));
        let mut ran = Vec::new();

        drain_inbox(
            &inbox,
            || base + Duration::from_millis(500),
            |command| ran.push(command),
        );

        assert_eq!(
            levels(&ran),
            vec![Some(0.75)],
            "only the command still in time reaches the runner"
        );
        assert_eq!(held(&late_answer), Held::Expired);
        assert_eq!(held(&later_answer), Held::Expired);
        assert_eq!(
            held(&in_time_answer),
            Held::Nothing,
            "the drain answers nothing for a command it ran"
        );
        assert!(inbox.borrow().is_empty(), "every command was taken out");
    }

    // Criterion (#146, guard): the clock is read once per command, when that
    // command is taken out — not once for the whole drain. The clock stands
    // before both deadlines until a command runs, and running the first takes
    // it past the second one's `start_by`, as a slow command does. The near
    // miss is one `now` read before the drain, or one read per command made
    // up front: under either, both commands run.
    //
    // Pinned: exactly one read per command taken out. A drain that reads the
    // clock before finding the queue empty makes a third read.
    #[test]
    fn test_drain_inbox_reads_the_clock_once_per_command_so_one_overtaken_by_a_slow_one_expires() {
        let base = Instant::now();
        let (first, first_answer) = volume_envelope(base + Duration::from_millis(300), 0.25);
        let (second, second_answer) = volume_envelope(base + Duration::from_millis(310), 0.75);
        let inbox = RefCell::new(VecDeque::from([first, second]));
        let time = Cell::new(base + Duration::from_millis(100));
        let reads = Cell::new(0_u32);
        let mut ran = Vec::new();

        drain_inbox(
            &inbox,
            || {
                reads.set(reads.get() + 1);
                time.get()
            },
            |command| {
                ran.push(command);
                time.set(base + Duration::from_secs(2));
            },
        );

        assert_eq!(
            levels(&ran),
            vec![Some(0.25)],
            "the first ran, the second did not"
        );
        assert_eq!(held(&first_answer), Held::Nothing);
        assert_eq!(
            held(&second_answer),
            Held::Expired,
            "the second's start_by passed while the first ran"
        );
        assert_eq!(reads.get(), 2, "one clock read per command taken out");
        assert!(inbox.borrow().is_empty(), "every command was taken out");
    }

    // Criterion (#146): a command taken out at `now == start_by`, or earlier,
    // is handed to the runner untouched, in queue order, and the drain sends
    // nothing on its reply. The first command is the near miss of the strict
    // comparison, seen through the drain: its `start_by` is the very instant
    // the clock reads.
    #[test]
    fn test_drain_inbox_runs_every_command_in_time_in_queue_order_start_by_itself_included() {
        let base = Instant::now();
        let (at, at_answer) = volume_envelope(base + Duration::from_millis(500), 0.25);
        let (just, just_answer) = volume_envelope(base + Duration::from_nanos(500_000_001), 0.5);
        let (ample, ample_answer) = volume_envelope(base + Duration::from_secs(2), 0.75);
        let inbox = RefCell::new(VecDeque::from([at, just, ample]));
        let reads = Cell::new(0_u32);
        let mut ran = Vec::new();

        drain_inbox(
            &inbox,
            || {
                reads.set(reads.get() + 1);
                base + Duration::from_millis(500)
            },
            |command| ran.push(command),
        );

        assert_eq!(levels(&ran), vec![Some(0.25), Some(0.5), Some(0.75)]);
        assert_eq!(held(&at_answer), Held::Nothing);
        assert_eq!(held(&just_answer), Held::Nothing);
        assert_eq!(held(&ample_answer), Held::Nothing);
        assert_eq!(reads.get(), 3, "one clock read per command taken out");
        assert!(inbox.borrow().is_empty(), "every command was taken out");
    }

    // Constraint (#146, where the check sits): a command enters the inbox
    // while the loop iterates, and a running command iterates it. One queued
    // while an earlier command runs is taken out by the same drain, with a
    // clock read of its own — left behind, it would wait for the next event
    // of a loop that blocks without a timeout. The near misses: a drain over
    // a snapshot of the queue, which never sees it, and a drain that keeps
    // the queue borrowed while the runner runs, which the push below trips.
    #[test]
    fn test_drain_inbox_takes_out_a_command_queued_while_an_earlier_one_runs() {
        let base = Instant::now();
        let (first, first_answer) = volume_envelope(base + Duration::from_millis(300), 0.25);
        let (arriving, arriving_answer) = volume_envelope(base + Duration::from_millis(600), 0.75);
        let inbox = RefCell::new(VecDeque::from([first]));
        let mut arriving = Some(arriving);
        let reads = Cell::new(0_u32);
        let mut ran = Vec::new();

        drain_inbox(
            &inbox,
            || {
                reads.set(reads.get() + 1);
                base + Duration::from_millis(100)
            },
            |command| {
                ran.push(command);
                // As the channel callback does while a command iterates the loop.
                if let Some(envelope) = arriving.take() {
                    inbox.borrow_mut().push_back(envelope);
                }
            },
        );

        assert_eq!(levels(&ran), vec![Some(0.25), Some(0.75)]);
        assert_eq!(held(&first_answer), Held::Nothing);
        assert_eq!(held(&arriving_answer), Held::Nothing);
        assert_eq!(reads.get(), 2, "one clock read per command taken out");
        assert!(inbox.borrow().is_empty(), "every command was taken out");
    }

    /// A sender that records the `start_by` of every envelope it is handed,
    /// then passes the envelope on to `inner`.
    struct StampRecorder {
        stamps: Arc<Mutex<Vec<Instant>>>,
        inner: Box<dyn LoopSender>,
    }

    impl LoopSender for StampRecorder {
        fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
            self.stamps.lock().unwrap().push(envelope.start_by);
            self.inner.send(envelope)
        }
    }

    /// A healthy loop thread behind a [`StampRecorder`] writing into `stamps`.
    fn stamp_recording_loop(stamps: Arc<Mutex<Vec<Instant>>>) -> Box<dyn LoopSender> {
        Box::new(StampRecorder {
            stamps,
            inner: answering_loop(vec![SPEAKER.to_string()], Arc::new(Mutex::new(Vec::new()))),
        })
    }

    /// Call one method of `graph`, which the loop must answer, and return the
    /// instants read just before and just after the call.
    fn timed<T>(
        graph: &mut PipeWireGraph,
        call: impl FnOnce(&mut PipeWireGraph) -> Result<T, AudioError>,
    ) -> (Instant, Instant) {
        let before = Instant::now();
        let answered = call(graph).is_ok();
        let after = Instant::now();
        assert!(answered, "the healthy loop answers every command");
        (before, after)
    }

    // Criterion (#146): every command the handle sends reaches the loop in an
    // envelope whose `start_by` is the send instant plus 300 ms — each of the
    // eleven methods, bounded by the instants read around its own call. Both
    // bounds are literal: a budget of zero falls under the lower one, a
    // budget as long as the whole wait goes over the upper one.
    #[test]
    fn test_every_method_s_command_reaches_the_loop_stamped_300_ms_after_its_send() {
        let stamps = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&stamps);
        let mut graph =
            PipeWireGraph::with_loop(Box::new(move |_| stamp_recording_loop(Arc::clone(&log))));

        let sent = [
            ("sinks", timed(&mut graph, |g| g.sinks())),
            ("branches", timed(&mut graph, |g| g.branches(COMBINED))),
            (
                "create_combined_sink",
                timed(&mut graph, |g| g.create_combined_sink(COMBINED)),
            ),
            (
                "load_branch",
                timed(&mut graph, |g| g.load_branch(COMBINED, SPEAKER, 170)),
            ),
            ("unload_branch", timed(&mut graph, |g| g.unload_branch(7))),
            (
                "set_branch_delay",
                timed(&mut graph, |g| g.set_branch_delay(9, 250)),
            ),
            ("teardown", timed(&mut graph, |g| g.teardown(COMBINED))),
            (
                "clear_stale_default_sink",
                timed(&mut graph, |g| g.clear_stale_default_sink(COMBINED)),
            ),
            (
                "retarget_streams",
                timed(&mut graph, |g| g.retarget_streams(COMBINED)),
            ),
            ("sink_volume", timed(&mut graph, |g| g.sink_volume(SPEAKER))),
            (
                "set_sink_volume",
                timed(&mut graph, |g| g.set_sink_volume(SPEAKER, 0.25)),
            ),
        ];

        let stamps = stamps.lock().unwrap();
        assert_eq!(stamps.len(), 11, "one envelope per method called");
        for ((method, (before, after)), start_by) in sent.iter().zip(stamps.iter()) {
            assert!(
                *start_by >= *before + Duration::from_millis(300),
                "{method}: start_by is under 300 ms after the send, {:?} after the call began",
                start_by.saturating_duration_since(*before)
            );
            assert!(
                *start_by <= *after + Duration::from_millis(300),
                "{method}: start_by is over 300 ms after the send, {:?} after the call ended",
                start_by.saturating_duration_since(*after)
            );
        }
    }

    // Criterion (#146, guard): a command resent to a replacement thread
    // carries the `start_by` it was first stamped with — it is stamped once.
    // The near miss is a replacement thread that takes 50 ms to start: a
    // stamp made again once it is there lands, budget included, at least
    // 50 ms past the bound below.
    //
    // That bound is the instant the first thread was asked for, plus 300 ms:
    // `ask` stamps the envelope before it sends it, so before any thread is
    // started, and the stamp the replacement receives can only be at or under
    // it. The instant is read inside the spawner, so neither bound depends on
    // how the test thread is scheduled.
    #[test]
    fn test_a_command_resent_to_a_replacement_thread_keeps_the_start_by_it_was_stamped_with() {
        let stamps = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&stamps);
        // When each loop thread was asked for: the dead one, then its replacement.
        let asked: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&asked);
        let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
            let mut asked = record.lock().unwrap();
            asked.push(Instant::now());
            if asked.len() == 1 {
                // The first thread is already dead: its receiver is gone.
                let (tx, rx) = mpsc::channel::<Envelope>();
                drop(rx);
                return Box::new(tx) as Box<dyn LoopSender>;
            }
            std::thread::sleep(Duration::from_millis(50));
            stamp_recording_loop(Arc::clone(&log))
        }));

        let before = Instant::now();
        let answer = graph.sinks();

        assert_eq!(answer.ok(), Some(vec![SPEAKER.to_string()]));
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "the dead thread and its replacement");
        let first_asked = asked.first().copied().unwrap();
        let stamps = stamps.lock().unwrap();
        assert_eq!(stamps.len(), 1, "the replacement received the command once");
        let start_by = stamps.first().copied().unwrap();
        assert!(
            start_by >= before + Duration::from_millis(300),
            "the command was stamped 300 ms after its send, {:?} after the call began",
            start_by.saturating_duration_since(before)
        );
        assert!(
            start_by <= first_asked + Duration::from_millis(300),
            "start_by moved after the first thread was asked for: it is {:?} past that instant",
            start_by.saturating_duration_since(first_asked)
        );
    }

    /// Answer `command` as the loop does one it took out of its queue too
    /// late: `Err(AudioError::Expired)` on its own reply, without running it.
    fn answer_expired(command: Command) {
        match command {
            Command::Sinks { reply } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::Branches { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::CreateCombinedSink { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::LoadBranch { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::UnloadBranch { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::SetBranchDelay { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::Teardown { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::ClearStaleDefaultSink { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::RetargetStreams { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::SinkVolume { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
            Command::SetSinkVolume { reply, .. } => {
                let _ = reply.send(Err(AudioError::Expired));
            },
        }
    }

    /// A loop thread that expires every command it receives. It answers by
    /// its own `match`, not through the check under test.
    fn expiring_loop() -> Box<dyn LoopSender> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        std::thread::spawn(move || {
            for envelope in rx {
                answer_expired(envelope.command);
            }
        });
        Box::new(tx)
    }

    /// Require `answer` to be the typed expiry, naming `method` when it is not.
    fn assert_expired<T: std::fmt::Debug>(method: &str, answer: Result<T, AudioError>) {
        assert!(
            matches!(answer, Err(AudioError::Expired)),
            "{method} handed back {answer:?}"
        );
    }

    // Criterion (#146, guard): the handle hands an `Err(AudioError::Expired)`
    // answered by the loop back as is — still the typed variant, which is what
    // maps to a 503. The near miss is an answer passed through a catch-all
    // into `AudioError::PipeWire`: an `Err` all the same, so the variant is
    // matched, on each of the eleven methods.
    #[test]
    fn test_every_method_hands_back_an_expired_answer_as_the_typed_variant() {
        let mut graph = PipeWireGraph::with_loop(Box::new(|_| expiring_loop()));

        assert_expired("sinks", graph.sinks());
        assert_expired("branches", graph.branches(COMBINED));
        assert_expired("create_combined_sink", graph.create_combined_sink(COMBINED));
        assert_expired("load_branch", graph.load_branch(COMBINED, SPEAKER, 170));
        assert_expired("unload_branch", graph.unload_branch(7));
        assert_expired("set_branch_delay", graph.set_branch_delay(9, 250));
        assert_expired("teardown", graph.teardown(COMBINED));
        assert_expired(
            "clear_stale_default_sink",
            graph.clear_stale_default_sink(COMBINED),
        );
        assert_expired("retarget_streams", graph.retarget_streams(COMBINED));
        assert_expired("sink_volume", graph.sink_volume(SPEAKER));
        assert_expired("set_sink_volume", graph.set_sink_volume(SPEAKER, 0.25));
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
    // branch's nodes `blue2th_delay.<id>.in` / `.out` before the module is
    // added: a mismatch would leave liveness, the links and unload looking for
    // another branch.
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

    // ─── #80: speaker sink events ────────────────────────────────────────────
    //
    // The node names and classes below come from a live `pw-dump`, 2026-09-26/27
    // (JBL Xtreme 3 + WH-1000XM5 on PipeWire 1.x): the two `bluez_output.*`
    // nodes are `Audio/Sink`, `bluez_input.*` is the headset's source,
    // `bluez_capture_internal.*` a stream, and `blue2th_delay.<n>.out` is
    // `Stream/Output/Audio`. What no capture shows — a stream *named* like a
    // speaker sink, or a name that is only the prefix — is synthetic, and says so.

    /// The WH-1000XM5's sink, as `pw-dump` listed it.
    const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";
    /// The JBL Xtreme 3's sink, as `pw-dump` listed it.
    const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";

    fn props(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn sink_props(name: &str) -> BTreeMap<String, String> {
        props(&[
            ("node.name", name),
            ("media.class", "Audio/Sink"),
            ("device.api", "bluez5"),
        ])
    }

    // Criterion: a `bluez_output.*` `Audio/Sink` announced by the registry is
    // `SinkAppeared`, carrying its node name and the instant it was seen.
    #[test]
    fn test_speaker_sink_event_of_a_bluez_sink_added_is_sink_appeared() {
        let at = Instant::now();

        assert_eq!(
            speaker_sink_event(RegistryChange::Added, &sink_props(JBL_SINK), at),
            Some(GraphEvent::SinkAppeared {
                name: JBL_SINK.to_string(),
                at
            })
        );
    }

    // Criterion: the same sink removed is `SinkVanished`, never `SinkAppeared`
    // — the two changes are not confused.
    #[test]
    fn test_speaker_sink_event_of_a_bluez_sink_removed_is_sink_vanished() {
        let at = Instant::now();

        assert_eq!(
            speaker_sink_event(RegistryChange::Removed, &sink_props(SONY_SINK), at),
            Some(GraphEvent::SinkVanished {
                name: SONY_SINK.to_string(),
                at
            })
        );
    }

    // Criterion (guard, only `bluez_output.`): an `Audio/Sink` of any other
    // name emits nothing, added or removed. The near misses pass the class
    // check and only the name guard excludes them: the PC's own ALSA output,
    // and the combined null sink this server creates. The control: a speaker
    // sink under the same class does emit.
    #[test]
    fn test_speaker_sink_event_ignores_a_non_bluez_sink() {
        let at = Instant::now();
        assert!(
            speaker_sink_event(RegistryChange::Added, &sink_props(SONY_SINK), at).is_some(),
            "control: a speaker sink of the same class emits"
        );

        for name in ["alsa_output.pci-0000_00_1f.3.analog-stereo", COMBINED] {
            let near_miss = props(&[("node.name", name), ("media.class", "Audio/Sink")]);
            for change in [RegistryChange::Added, RegistryChange::Removed] {
                assert_eq!(
                    speaker_sink_event(change, &near_miss, at),
                    None,
                    "{name} ({change:?}) is not a speaker sink"
                );
            }
        }
    }

    // Criterion (guard, only `Audio/Sink`): a node named like a speaker that
    // is not a sink emits nothing. The near miss only the class guard
    // excludes: a `Stream/Output/Audio` named `bluez_output.<MAC>.1`
    // (synthetic — the class of the real `blue2th_delay.<n>.out` streams, under
    // a speaker's name). Also the real headset source and capture stream.
    #[test]
    fn test_speaker_sink_event_ignores_a_bluez_node_that_is_not_a_sink() {
        let at = Instant::now();
        assert!(
            speaker_sink_event(RegistryChange::Added, &sink_props(SONY_SINK), at).is_some(),
            "control: the same name as an Audio/Sink emits"
        );

        let not_sinks = [
            props(&[
                ("node.name", SONY_SINK),
                ("media.class", "Stream/Output/Audio"),
            ]),
            props(&[("node.name", SONY_SINK)]),
            props(&[
                ("node.name", "bluez_input.80:99:E7:63:50:29"),
                ("media.class", "Audio/Source"),
            ]),
            props(&[
                ("node.name", "bluez_capture_internal.80:99:E7:63:50:29"),
                ("media.class", "Stream/Input/Audio"),
            ]),
            props(&[
                ("node.name", "blue2th_delay.3.out"),
                ("media.class", "Stream/Output/Audio"),
            ]),
        ];
        for near_miss in &not_sinks {
            for change in [RegistryChange::Added, RegistryChange::Removed] {
                assert_eq!(
                    speaker_sink_event(change, near_miss, at),
                    None,
                    "{near_miss:?} ({change:?}) is not a speaker sink"
                );
            }
        }
    }

    // Criterion (guard, the empty name): no event for an `Audio/Sink` whose
    // name is empty or missing, nor for one that is the bare prefix
    // `bluez_output.` with no address after it. The bare prefix is the near
    // miss a plain `starts_with("bluez_output.")` accepts; it names no speaker
    // (it is `bluez_sink_prefix("")`), so it is the empty value in the
    // address's position. Synthetic: no capture holds such a node.
    #[test]
    fn test_speaker_sink_event_of_an_empty_name_is_none() {
        let at = Instant::now();
        assert!(
            speaker_sink_event(RegistryChange::Added, &sink_props(JBL_SINK), at).is_some(),
            "control: a named speaker sink emits"
        );

        let nameless = [
            sink_props(""),
            props(&[("media.class", "Audio/Sink")]),
            sink_props("bluez_output."),
        ];
        for near_miss in &nameless {
            for change in [RegistryChange::Added, RegistryChange::Removed] {
                assert_eq!(
                    speaker_sink_event(change, near_miss, at),
                    None,
                    "{near_miss:?} ({change:?}) names no speaker"
                );
            }
        }
    }

    // Criterion: the reconnect backoff walks 1 s, 2 s, 5 s, 10 s, then 30 s for
    // every later attempt. `failures` counts the attempts that already failed,
    // so the first retry after the loss (`0`) waits 1 s.
    #[test]
    fn test_reconnect_delay_walks_one_two_five_ten_then_thirty() {
        let delays: Vec<u64> = (0..=6).map(|f| reconnect_delay(f).as_secs()).collect();

        assert_eq!(delays, vec![1, 2, 5, 10, 30, 30, 30]);
    }

    // Criterion (guard, bounded backoff): however many attempts failed, the
    // delay is 30 s — it never overflows and never exceeds it.
    #[test]
    fn test_reconnect_delay_never_exceeds_thirty_seconds() {
        assert_eq!(reconnect_delay(50), Duration::from_secs(30));
        assert_eq!(reconnect_delay(u32::MAX), Duration::from_secs(30));
    }

    // ─── #80: the registry callbacks emit the events ─────────────────────────

    /// A `Shared` whose events go to the returned receiver.
    fn watched_shared() -> (Shared, tokio::sync::mpsc::UnboundedReceiver<GraphEvent>) {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let shared = Shared {
            events: Some(events),
            ..Shared::default()
        };
        (shared, receiver)
    }

    /// Deliver a `global` event for `id` to `shared`, as the registry listener
    /// does. Builds real PipeWire properties: no daemon is involved.
    fn announce(shared: &mut Shared, id: u32, type_: ObjectType, pairs: &[(&str, &str)]) {
        let mut properties = PropertiesBox::new();
        for (key, value) in pairs {
            properties.insert(*key, *value);
        }
        let global: GlobalObject<&libspa::utils::dict::DictRef> = GlobalObject {
            id,
            permissions: pw::permissions::PermissionFlags::empty(),
            type_,
            version: 3,
            props: Some(properties.dict()),
        };
        shared.add_global(&global);
    }

    /// Every event waiting in `receiver`, without waiting for more.
    fn pending(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<GraphEvent>) -> Vec<GraphEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    /// The name an event carries, and whether it appeared: the `at` is the
    /// callback's own `Instant::now()`, which a test cannot name.
    fn named_change(event: &GraphEvent) -> Option<(&'static str, String)> {
        match event {
            GraphEvent::SinkAppeared { name, .. } => Some(("appeared", name.clone())),
            GraphEvent::SinkVanished { name, .. } => Some(("vanished", name.clone())),
            GraphEvent::CombinedSinkVanished { name, .. } => {
                Some(("combined sink vanished", name.clone()))
            },
            GraphEvent::Reconnected => None,
        }
    }

    // Criterion: `Shared::add_global` emits `SinkAppeared` for a speaker sink
    // the registry announces, and still mirrors the node. The near miss in the
    // same registry burst: a stream named like the speaker, which the mirror
    // keeps but which emits nothing.
    #[test]
    fn test_add_global_of_a_speaker_sink_emits_sink_appeared() {
        let (mut shared, mut receiver) = watched_shared();

        announce(
            &mut shared,
            61,
            ObjectType::Node,
            &[
                ("node.name", SONY_SINK),
                ("media.class", "Stream/Output/Audio"),
            ],
        );
        announce(
            &mut shared,
            62,
            ObjectType::Node,
            &[("node.name", JBL_SINK), ("media.class", "Audio/Sink")],
        );

        let events = pending(&mut receiver);
        assert_eq!(
            events.iter().filter_map(named_change).collect::<Vec<_>>(),
            vec![("appeared", JBL_SINK.to_string())]
        );
        assert_eq!(events.len(), 1, "one event, got {events:?}");
        assert!(shared.mirror.nodes.contains_key(&61));
        assert!(shared.mirror.nodes.contains_key(&62));
    }

    // Criterion: `Shared::remove_global` emits `SinkVanished`, reading the
    // node's props from the mirror **before** forgetting them — the removal
    // itself carries only an id. Removing it first would leave nothing to
    // read, and no event. The near miss: a non-speaker sink removed in the
    // same burst emits nothing.
    #[test]
    fn test_remove_global_of_a_mirrored_speaker_sink_emits_sink_vanished() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(
            70,
            node(&[("node.name", SONY_SINK), ("media.class", "Audio/Sink")]),
        );
        shared.mirror.nodes.insert(
            71,
            node(&[
                ("node.name", "alsa_output.pci-0000_00_1f.3.analog-stereo"),
                ("media.class", "Audio/Sink"),
            ]),
        );

        shared.remove_global(71);
        shared.remove_global(70);

        let events = pending(&mut receiver);
        assert_eq!(
            events.iter().filter_map(named_change).collect::<Vec<_>>(),
            vec![("vanished", SONY_SINK.to_string())]
        );
        assert_eq!(events.len(), 1, "one event, got {events:?}");
        assert!(shared.mirror.nodes.is_empty(), "both nodes are forgotten");
    }

    // Criterion: a removal of an id the mirror does not know — a link, a
    // port, or a global it never saw — emits nothing. The control: the
    // speaker sink the mirror does know emits, in the same run.
    #[test]
    fn test_remove_global_of_an_unknown_id_emits_nothing() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(
            70,
            node(&[("node.name", JBL_SINK), ("media.class", "Audio/Sink")]),
        );
        shared.mirror.links.insert(
            80,
            LinkEntry {
                output_node: 70,
                input_node: 12,
            },
        );

        shared.remove_global(80);
        shared.remove_global(999);
        assert!(
            pending(&mut receiver).is_empty(),
            "a link and an unknown id emit nothing"
        );

        shared.remove_global(70);
        assert_eq!(
            pending(&mut receiver)
                .iter()
                .filter_map(named_change)
                .collect::<Vec<_>>(),
            vec![("vanished", JBL_SINK.to_string())],
            "control: the mirrored speaker sink emits"
        );
    }

    // Criterion (non-nominal): the event consumer is gone — the loop drops
    // events silently, never blocks and never panics, and still keeps its
    // mirror, which routing depends on.
    #[test]
    fn test_add_global_with_the_consumer_gone_still_mirrors_the_node() {
        let (mut shared, receiver) = watched_shared();
        drop(receiver);

        announce(
            &mut shared,
            62,
            ObjectType::Node,
            &[("node.name", JBL_SINK), ("media.class", "Audio/Sink")],
        );
        shared.remove_global(62);
        announce(
            &mut shared,
            63,
            ObjectType::Node,
            &[("node.name", JBL_SINK), ("media.class", "Audio/Sink")],
        );

        assert!(shared.mirror.nodes.contains_key(&63));
        assert!(!shared.mirror.nodes.contains_key(&62));
    }

    // Criterion: a graph nobody watches — `detached()`, the route tests'
    // graph — holds no sender, so its callbacks emit nothing and nothing
    // fails for the lack of one.
    #[test]
    fn test_add_global_of_an_unwatched_graph_only_mirrors() {
        let mut shared = Shared::default();

        announce(
            &mut shared,
            62,
            ObjectType::Node,
            &[("node.name", JBL_SINK), ("media.class", "Audio/Sink")],
        );
        shared.remove_global(62);

        assert!(shared.mirror.nodes.is_empty());
    }

    // ─── #80: `watch` ────────────────────────────────────────────────────────

    /// A graph whose loop threads answer like a healthy graph, recording for
    /// each thread started whether it was handed an event sender. A thread
    /// handed one reports `Reconnected` through it, so a test can tell the
    /// watched sender from any other.
    fn recording_graph(handed: Arc<Mutex<Vec<bool>>>) -> PipeWireGraph {
        PipeWireGraph::with_loop(Box::new(
            move |events: Option<UnboundedSender<GraphEvent>>| {
                handed.lock().unwrap().push(events.is_some());
                if let Some(events) = events {
                    let _ = events.send(GraphEvent::Reconnected);
                }
                answering_loop(vec![SPEAKER.to_string()], Arc::new(Mutex::new(Vec::new())))
            },
        ))
    }

    // Criterion: `watch` starts the loop thread at once — no command needed —
    // and hands it the sender it was given, so the thread's events reach
    // that receiver. Checked on the handle's state, not by timing.
    #[test]
    fn test_watch_starts_the_loop_thread_at_once_with_the_event_sender() {
        let handed = Arc::new(Mutex::new(Vec::new()));
        let mut graph = recording_graph(Arc::clone(&handed));
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        graph.watch(events);

        assert!(
            graph.sender.is_some(),
            "the loop thread was started by watch"
        );
        assert_eq!(
            *handed.lock().unwrap(),
            vec![true],
            "one thread, handed the sender"
        );
        assert_eq!(receiver.try_recv().ok(), Some(GraphEvent::Reconnected));

        // A command reuses the thread `watch` started.
        assert!(graph.sinks().is_ok());
        assert_eq!(handed.lock().unwrap().len(), 1, "no second thread");
    }

    // Criterion: a watched graph keeps reporting after its thread died — the
    // thread started in its place is handed the same sender. Without it, the
    // events would stop for good after the first thread's death.
    #[test]
    fn test_a_loop_thread_replacing_a_dead_one_keeps_the_event_sender() {
        let handed = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&handed);
        let starts = Arc::new(AtomicUsize::new(0));
        let mut graph = PipeWireGraph::with_loop(Box::new(
            move |events: Option<UnboundedSender<GraphEvent>>| {
                record.lock().unwrap().push(events.is_some());
                if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                    // The first thread dies at once: its receiver is gone.
                    let (tx, rx) = mpsc::channel::<Envelope>();
                    drop(rx);
                    return Box::new(tx) as Box<dyn LoopSender>;
                }
                if let Some(events) = events {
                    let _ = events.send(GraphEvent::Reconnected);
                }
                answering_loop(vec![SPEAKER.to_string()], Arc::new(Mutex::new(Vec::new())))
            },
        ));
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        graph.watch(events);
        let answer = graph.sinks();

        assert_eq!(answer.ok(), Some(vec![SPEAKER.to_string()]));
        assert_eq!(
            *handed.lock().unwrap(),
            vec![true, true],
            "the dead thread and its replacement were both handed the sender"
        );
        assert_eq!(receiver.try_recv().ok(), Some(GraphEvent::Reconnected));
    }

    // Criterion: a graph nobody watches hands its loop thread no event sender,
    // and still starts it only on the first command (#79).
    #[test]
    fn test_an_unwatched_graph_hands_its_loop_no_event_sender() {
        let handed = Arc::new(Mutex::new(Vec::new()));
        let mut graph = recording_graph(Arc::clone(&handed));
        assert!(graph.sender.is_none(), "nothing started before a command");

        assert!(graph.sinks().is_ok());

        assert_eq!(*handed.lock().unwrap(), vec![false]);
    }

    // Criterion: `detached()` emits nothing, even watched, and reaches no
    // daemon: its commands still err at once. The control that the channel
    // works at all is the watch test above.
    #[test]
    fn test_detached_graph_watched_emits_nothing() {
        let mut graph = PipeWireGraph::detached();
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        graph.watch(events);
        let sinks = graph.sinks();

        assert!(
            matches!(&sinks, Err(AudioError::PipeWire(m)) if m.contains("not running")),
            "got {sinks:?}"
        );
        assert!(
            receiver.try_recv().is_err(),
            "no event from a detached graph"
        );
    }

    // ─── #80: the watched loop's reconnect schedule ──────────────────────────

    /// Every `Reconnected` waiting in `receiver`.
    fn reconnections(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<GraphEvent>) -> usize {
        pending(receiver)
            .iter()
            .filter(|event| **event == GraphEvent::Reconnected)
            .count()
    }

    // Criterion: once the connection is lost, the loop retries after 1 s, 2 s,
    // 5 s, 10 s, then every 30 s — each delay counted from the attempt that
    // failed, and each attempt due exactly then, not a moment before.
    #[test]
    fn test_reconnect_watch_after_a_loss_retries_at_one_two_five_ten_then_thirty() {
        let lost_at = Instant::now();
        let mut watch = ReconnectWatch::new(lost_at);
        watch.lost(lost_at);

        let mut now = lost_at;
        let mut waits = Vec::new();
        for _ in 0..6 {
            let wait = watch.wait(false, now).unwrap_or_default();
            assert!(!watch.attempt_due(false, now + wait - Duration::from_millis(1)));
            now += wait;
            assert!(watch.attempt_due(false, now), "due after {wait:?}");
            waits.push(wait.as_secs());
            watch.failed(now);
        }

        assert_eq!(waits, vec![1, 2, 5, 10, 30, 30]);
    }

    // Criterion: a loop that holds a connection neither retries nor wakes to
    // retry — it blocks until the daemon or a command wakes it. A loop that
    // holds none never blocks past its next attempt, and not at all once that
    // is past: an infinite wait there would leave a restarted daemon unnoticed
    // until the next command.
    #[test]
    fn test_reconnect_watch_waits_for_ever_only_while_connected() {
        let start = Instant::now();
        let mut watch = ReconnectWatch::new(start);
        watch.lost(start);

        assert_eq!(watch.wait(true, start), None);
        assert!(!watch.attempt_due(true, start + Duration::from_secs(60)));

        assert_eq!(watch.wait(false, start), Some(Duration::from_secs(1)));
        assert_eq!(
            watch.wait(false, start + Duration::from_secs(5)),
            Some(Duration::ZERO),
            "an attempt already due waits for nothing"
        );
    }

    // Criterion: a fresh thread tries at once, and its first connection is
    // not a reconnection — it emits nothing, so startup wakes no pass.
    #[test]
    fn test_reconnect_watch_first_connection_emits_nothing() {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let start = Instant::now();
        let mut watch = ReconnectWatch::new(start);
        assert!(
            watch.attempt_due(false, start),
            "a fresh thread tries at once"
        );

        watch.connected(Some(&events));

        assert_eq!(reconnections(&mut receiver), 0);
    }

    // Criterion (guard, exactly one): a connection back after a loss emits one
    // `Reconnected`, however many failed attempts came first — and no more
    // while it holds, although the loop reports "connected" on every wake-up.
    // A second loss owes a second one.
    #[test]
    fn test_reconnect_watch_emits_exactly_one_reconnected_per_loss() {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let now = Instant::now();
        let mut watch = ReconnectWatch::new(now);
        watch.connected(Some(&events));

        watch.lost(now);
        watch.failed(now);
        watch.failed(now);
        for _ in 0..3 {
            watch.connected(Some(&events));
        }
        assert_eq!(reconnections(&mut receiver), 1);

        watch.lost(now);
        watch.connected(Some(&events));
        watch.connected(Some(&events));
        assert_eq!(reconnections(&mut receiver), 1, "the second loss");
    }

    // Criterion: a success resets the count, so the next loss starts the walk
    // again at 1 s and 2 s rather than at 30 s — `lost` restarts it, as a
    // connection a command made and lost at once never reaches `connected`.
    // Also: the consumer gone, the `Reconnected` is dropped without a panic.
    #[test]
    fn test_reconnect_watch_success_resets_the_backoff() {
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let now = Instant::now();
        let mut watch = ReconnectWatch::new(now);
        watch.lost(now);
        for _ in 0..5 {
            watch.failed(now);
        }
        assert_eq!(watch.wait(false, now), Some(Duration::from_secs(30)));

        watch.connected(Some(&events));
        watch.lost(now);

        assert_eq!(watch.wait(false, now), Some(Duration::from_secs(1)));
        assert_eq!(watch.failed(now), Duration::from_secs(2));

        // A command connects and loses the connection within one wake-up:
        // `connected` never runs in between, and the walk still restarts.
        for _ in 0..5 {
            watch.failed(now);
        }
        watch.lost(now);
        assert_eq!(watch.wait(false, now), Some(Duration::from_secs(1)));
        assert_eq!(watch.failed(now), Duration::from_secs(2));
    }

    // ─── #139: the streams that asked for the combined sink ──────────────────

    /// The nodes `streams_targeting` has to tell apart, as `pw-dump` listed
    /// them on the dev PC on 2026-09-29 with Spotify playing on one speaker
    /// (`librespot --name Lpt --backend pulseaudio --device blue2th_combined`,
    /// librespot 0.8.0 through `pipewire-pulse`), props trimmed to the keys
    /// that matter. Node 127, a delay branch's output, is the captured near
    /// miss: a `Stream/Output/Audio` of our own that carries a
    /// `target.object`, naming a speaker sink.
    fn captured_streams() -> Vec<(u32, NodeEntry)> {
        vec![
            (
                131,
                node(&[
                    ("media.class", "Stream/Output/Audio"),
                    ("node.name", "librespot - Lpt"),
                    ("application.name", "librespot - Lpt"),
                    ("application.process.binary", "librespot"),
                    ("client.api", "pipewire-pulse"),
                    ("media.role", "Music"),
                    ("target.object", COMBINED),
                ]),
            ),
            (
                112,
                node(&[
                    ("media.class", "Audio/Sink"),
                    ("node.name", COMBINED),
                    ("factory.name", "support.null-audio-sink"),
                ]),
            ),
            (
                127,
                node(&[
                    ("media.class", "Stream/Output/Audio"),
                    ("node.name", "blue2th_delay.1.out"),
                    ("node.dont-reconnect", "true"),
                    ("target.object", "bluez_output.2C_FD_B4_D3_AC_21.1"),
                ]),
            ),
            (
                119,
                node(&[
                    ("media.class", "Stream/Input/Audio"),
                    ("node.name", "blue2th_delay.1.in"),
                ]),
            ),
            (
                106,
                node(&[
                    ("media.class", "Stream/Output/Video"),
                    ("node.name", "kwin_wayland"),
                ]),
            ),
        ]
    }

    /// The captured graph, plus `extra`.
    fn streams_mirror(extra: &[(u32, NodeEntry)]) -> Mirror {
        let mut nodes = captured_streams();
        nodes.extend(extra.iter().cloned());
        mirror_of(&nodes, &[])
    }

    // Criterion (#139): on the captured graph, the one stream that asked for
    // the combined sink is `librespot`'s. Neither the combined sink itself nor
    // the delay branch's output — a `Stream/Output/Audio` carrying a
    // `target.object` of its own — is taken.
    #[test]
    fn test_streams_targeting_takes_librespot_s_stream_from_the_captured_graph() {
        let mirror = streams_mirror(&[]);

        assert_eq!(streams_targeting(&mirror, COMBINED), vec![131]);
    }

    // Criterion (#139): every output stream that asked for the combined sink is
    // taken, not the first one — here a second one, a browser's, beside
    // `librespot`'s. Two streams naming it are re-targeted together.
    #[test]
    fn test_streams_targeting_takes_every_stream_naming_the_sink() {
        let mirror = streams_mirror(&[(
            160,
            node(&[
                ("media.class", "Stream/Output/Audio"),
                ("node.name", "Firefox"),
                ("target.object", COMBINED),
            ]),
        )]);

        assert_eq!(sorted(streams_targeting(&mirror, COMBINED)), vec![131, 160]);
    }

    // Criterion (guard, exactly the combined sink): a stream whose
    // `target.object` is `blue2th_combined_old` is not taken — a `starts_with`
    // or a `contains` would take it, and only the whole-name comparison
    // refuses it. The control: `librespot`'s stream in the same graph is.
    #[test]
    fn test_streams_targeting_skips_a_stream_naming_a_longer_sink() {
        let mirror = streams_mirror(&[(
            140,
            node(&[
                ("media.class", "Stream/Output/Audio"),
                ("node.name", "librespot - Old"),
                ("target.object", "blue2th_combined_old"),
            ]),
        )]);

        assert_eq!(streams_targeting(&mirror, COMBINED), vec![131]);
    }

    // Criterion (guard, only output audio streams): a node naming the
    // combined sink exactly in its `target.object` is still skipped when it is
    // not a `Stream/Output/Audio` — an `Audio/Sink`, a `Stream/Input/Audio`,
    // and a `Stream/Output/Video`, the last one a near miss for a prefix check
    // on `Stream/Output`. Only the class check refuses them. The control:
    // `librespot`'s stream is taken.
    #[test]
    fn test_streams_targeting_skips_a_node_that_is_not_an_output_audio_stream() {
        let mirror = streams_mirror(&[
            (
                141,
                node(&[
                    ("media.class", "Audio/Sink"),
                    ("node.name", "a_sink_with_a_target"),
                    ("target.object", COMBINED),
                ]),
            ),
            (
                142,
                node(&[
                    ("media.class", "Stream/Input/Audio"),
                    ("node.name", "a_capture"),
                    ("target.object", COMBINED),
                ]),
            ),
            (
                143,
                node(&[
                    ("media.class", "Stream/Output/Video"),
                    ("node.name", "a_video_stream"),
                    ("target.object", COMBINED),
                ]),
            ),
        ]);

        assert_eq!(streams_targeting(&mirror, COMBINED), vec![131]);
    }

    // Criterion (guard, the empty value is not a wildcard): an empty sink name
    // takes nothing, even a stream whose `target.object` is itself empty — an
    // equality alone would take it, only the explicit emptiness guard refuses
    // it. A stream with no `target.object` at all is taken for no name. The
    // control: the combined sink's name, on the same graph, takes
    // `librespot`'s stream.
    #[test]
    fn test_streams_targeting_of_an_empty_sink_name_takes_nothing() {
        let mirror = streams_mirror(&[
            (
                150,
                node(&[
                    ("media.class", "Stream/Output/Audio"),
                    ("node.name", "a_stream_with_an_empty_target"),
                    ("target.object", ""),
                ]),
            ),
            (
                151,
                node(&[
                    ("media.class", "Stream/Output/Audio"),
                    ("node.name", "a_stream_without_a_target"),
                ]),
            ),
        ]);

        assert_eq!(streams_targeting(&mirror, ""), Vec::<u32>::new());
        assert_eq!(
            streams_targeting(&mirror, COMBINED),
            vec![131],
            "control: the combined sink's name takes librespot's stream"
        );
    }

    // ─── #139: the combined sink removed from outside ────────────────────────

    /// The combined sink's node as the registry announced it.
    fn combined_sink_node() -> NodeEntry {
        node(&[
            ("media.class", "Audio/Sink"),
            ("node.name", COMBINED),
            ("factory.name", "support.null-audio-sink"),
        ])
    }

    /// The combined-sink events among `events`, by the name they carry.
    fn combined_vanished(events: &[GraphEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                GraphEvent::CombinedSinkVanished { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    // Criterion (#139): the registry removes a combined sink this connection
    // still holds the proxy for — someone else destroyed it — and
    // `remove_global` emits `CombinedSinkVanished` naming it, once, and
    // nothing else: the combined sink is no speaker sink.
    #[test]
    fn test_remove_global_of_a_held_combined_sink_emits_combined_sink_vanished() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(112, combined_sink_node());
        shared.hold_combined_sink(112, COMBINED);

        shared.remove_global(112);

        let events = pending(&mut receiver);
        assert_eq!(combined_vanished(&events), vec![COMBINED.to_string()]);
        assert_eq!(events.len(), 1, "one event, got {events:?}");
        assert!(!shared.mirror.nodes.contains_key(&112), "still forgotten");
    }

    // Criterion (guard, only an external removal): once the proxy is released
    // — the server's own teardown drops it before the registry reports the
    // removal — the same node's removal emits nothing. The near miss is node
    // 112 itself: named `blue2th_combined`, from the null-sink factory, held a
    // moment before; only the held-proxy check tells it from an external
    // destruction. The control: the rebuilt sink, still held, emits.
    #[test]
    fn test_remove_global_after_the_combined_sink_s_release_emits_nothing() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(112, combined_sink_node());
        shared.hold_combined_sink(112, COMBINED);

        shared.release_combined_sink(COMBINED);
        shared.remove_global(112);
        assert_eq!(
            pending(&mut receiver),
            Vec::<GraphEvent>::new(),
            "our own teardown emits nothing"
        );

        shared.mirror.nodes.insert(117, combined_sink_node());
        shared.hold_combined_sink(117, COMBINED);
        shared.remove_global(117);
        assert_eq!(
            combined_vanished(&pending(&mut receiver)),
            vec![COMBINED.to_string()],
            "control: the rebuilt sink, still held, emits"
        );
    }

    // Criterion (guard, only a combined sink it created): the removal of a
    // node this connection holds no proxy for emits nothing — the PC's own
    // sink, a null sink named `blue2th_combined_old`, and a leftover named
    // `blue2th_combined` exactly, from the null-sink factory, that an earlier
    // run left (#78): only the held-proxy check refuses that last one. The
    // control: the combined sink it does hold emits.
    #[test]
    fn test_remove_global_of_a_sink_it_holds_no_proxy_for_emits_nothing() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(117, combined_sink_node());
        shared.hold_combined_sink(117, COMBINED);
        shared.mirror.nodes.insert(
            71,
            node(&[
                ("media.class", "Audio/Sink"),
                (
                    "node.name",
                    "alsa_output.pci-0000_c4_00.6.HiFi__Speaker__sink",
                ),
            ]),
        );
        shared.mirror.nodes.insert(
            200,
            node(&[
                ("media.class", "Audio/Sink"),
                ("node.name", "blue2th_combined_old"),
                ("factory.name", "support.null-audio-sink"),
            ]),
        );
        shared.mirror.nodes.insert(202, combined_sink_node());

        shared.remove_global(71);
        shared.remove_global(200);
        shared.remove_global(202);
        assert_eq!(
            pending(&mut receiver),
            Vec::<GraphEvent>::new(),
            "no removal of a sink it holds no proxy for emits"
        );

        shared.remove_global(117);
        assert_eq!(
            combined_vanished(&pending(&mut receiver)),
            vec![COMBINED.to_string()],
            "control: the held combined sink emits"
        );
    }

    // Criterion (#139): a vanished combined sink is forgotten with its node.
    // PipeWire reuses ids, so a later node given id 112 — here a stream — is
    // not the combined sink, and its removal emits nothing.
    #[test]
    fn test_remove_global_forgets_a_vanished_combined_sink_so_a_reused_id_emits_nothing() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(112, combined_sink_node());
        shared.hold_combined_sink(112, COMBINED);
        shared.remove_global(112);
        assert_eq!(
            combined_vanished(&pending(&mut receiver)),
            vec![COMBINED.to_string()]
        );

        shared.mirror.nodes.insert(
            112,
            node(&[
                ("media.class", "Stream/Output/Audio"),
                ("node.name", "librespot - Lpt"),
            ]),
        );
        shared.remove_global(112);
        assert_eq!(pending(&mut receiver), Vec::<GraphEvent>::new());
    }

    // Criterion (#139, non-regression): with a combined sink held, a speaker
    // sink's removal still emits `SinkVanished`, and only that — no
    // `CombinedSinkVanished` rides along with it.
    #[test]
    fn test_remove_global_of_a_speaker_sink_still_emits_only_sink_vanished() {
        let (mut shared, mut receiver) = watched_shared();
        shared.mirror.nodes.insert(117, combined_sink_node());
        shared.hold_combined_sink(117, COMBINED);
        shared.mirror.nodes.insert(
            70,
            node(&[("node.name", JBL_SINK), ("media.class", "Audio/Sink")]),
        );

        shared.remove_global(70);

        let events = pending(&mut receiver);
        assert_eq!(
            events.iter().filter_map(named_change).collect::<Vec<_>>(),
            vec![("vanished", JBL_SINK.to_string())]
        );
        assert_eq!(events.len(), 1, "one event, got {events:?}");
    }

    // Criterion (guard, bounded backoff): the failure count never overflows —
    // after `u32::MAX` failures one more is still a 30 s wait.
    #[test]
    fn test_reconnect_watch_failure_count_saturates() {
        let now = Instant::now();
        let mut watch = ReconnectWatch::new(now);
        watch.failures = u32::MAX;

        assert_eq!(watch.failed(now), Duration::from_secs(30));
        assert_eq!(watch.failures, u32::MAX);
    }
}
