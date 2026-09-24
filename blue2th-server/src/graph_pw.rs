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

/// Whether the branch whose playback node is `out_node` feeds `real_sink`.
pub(crate) fn branch_liveness(mirror: &Mirror, out_node: &str, real_sink: &str) -> bool {
    let outs: BTreeSet<u32> = mirror.node_ids_named(out_node).collect();
    let sinks: BTreeSet<u32> = mirror.node_ids_named(real_sink).collect();
    mirror
        .links
        .values()
        .any(|link| outs.contains(&link.output_node) && sinks.contains(&link.input_node))
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

    /// Whether `name` is a null sink this graph does not own — a leftover a
    /// build must replace, never reuse.
    fn is_foreign_null_sink(&self, name: &str) -> bool {
        !self.owns_null_sink(name)
            && self
                .mirror()
                .node_ids_named(name)
                .filter_map(|id| self.mirror().nodes.get(&id))
                .any(|node| node.prop("factory.name") == Some(NULL_SINK_FACTORY))
    }

    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        self.sync_mirror()?;
        Ok(sink_names(&self.mirror)
            .into_iter()
            .filter(|name| !self.is_foreign_null_sink(name))
            .collect())
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        self.sync_mirror()?;
        Ok(self
            .modules_for(sink_name)
            .into_iter()
            .map(|(id, branch)| LoadedBranch {
                id,
                live: Some(branch_liveness(
                    &self.mirror,
                    &branch_node_name(id, "out"),
                    &branch.sink,
                )),
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

    // Criterion: a node name is written as one quoted SPA-JSON string whatever it
    // carries — a quote in it cannot close the string early and leave the rest
    // of the name to be read as another key.
    #[test]
    fn test_loopback_module_args_escapes_a_quote_in_a_node_name() {
        let args = loopback_module_args(COMBINED, "odd\"sink", 50, 1).unwrap();

        assert!(
            args.contains(r#"target.object = "odd\"sink""#),
            "the quote is escaped inside the string, got {args}"
        );
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

    // Criterion (the empty value is a wildcard): an empty name matches no node,
    // not even one whose `node.name` is itself empty — otherwise a nameless
    // stream linked into the sink would vouch for a branch that is not there.
    #[test]
    fn test_branch_liveness_of_an_empty_name_ignores_a_nameless_node() {
        let mut mirror = liveness_mirror(&[(200, 91, 57), (201, 90, 92)]);
        mirror.nodes.insert(
            91,
            node(&[("node.name", ""), ("media.class", "Stream/Output/Audio")]),
        );
        mirror.nodes.insert(
            92,
            node(&[("node.name", ""), ("media.class", "Audio/Sink")]),
        );

        assert!(
            !branch_liveness(&mirror, "", SPEAKER),
            "a nameless stream linked into the sink is no branch"
        );
        assert!(
            !branch_liveness(&mirror, "blue2th_loop.3.out", ""),
            "a link into a nameless sink feeds no named speaker"
        );
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

    // Criterion: a hardware node is spared even when it would otherwise match —
    // named exactly like the sink, or sharing a pair's link group — since
    // destroying it switches its card's profile to `off`.
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

    // Criterion: a detached graph reaches no loop and no daemon — every command
    // errs at once, and a volume reads as unknown.
    #[test]
    fn test_detached_graph_errs_at_once_without_a_loop() {
        let mut graph = PipeWireGraph::detached();
        let started = Instant::now();

        let sinks = graph.sinks();
        let teardown = graph.teardown(COMBINED);

        assert!(
            matches!(&sinks, Err(AudioError::PipeWire(m)) if m.contains("not running")),
            "got {sinks:?}"
        );
        assert!(matches!(teardown, Err(AudioError::PipeWire(_))));
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
}
