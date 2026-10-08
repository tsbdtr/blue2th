// SPDX-License-Identifier: MIT OR Apache-2.0
//! The test tone (#66): a sine generated in-process and played as a PipeWire
//! stream pinned to the combined sink with `target.object`, so the server never
//! has to make that sink the PC's default.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libspa::param::audio::{AudioFormat, AudioInfoRaw};
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Pod, Value};
use libspa::utils::Direction;
use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::loop_::Timeout;
use pw::main_loop::MainLoopRc;
use pw::properties::PropertiesBox;
use pw::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};

use crate::audio::{AudioError, AudioOutput};

/// The tone's sample rate, in frames per second.
pub const TONE_RATE: u32 = 48_000;
/// The tone's frequency.
pub const TONE_FREQUENCY_HZ: f32 = 440.0;
/// The tone's peak amplitude, as a fraction of full scale.
pub const TONE_AMPLITUDE: f32 = 0.25;
/// The tone's whole length, in frames.
pub const TONE_FRAMES: usize = 96_000;
/// The length of the linear fade-in, and of the fade-out, in frames.
pub const TONE_FADE_FRAMES: usize = 480;

/// The tone's channel count: stereo, interleaved.
const TONE_CHANNELS: usize = 2;

/// The size of one frame in the stream's buffers: one F32 sample per channel.
const TONE_STRIDE: usize = std::mem::size_of::<f32>() * TONE_CHANNELS;

/// The node name the tone's stream carries.
const TONE_NODE_NAME: &str = "blue2th_tone";

/// How long the handle waits for the tone thread to answer one command. A
/// missing daemon fails the connect at once; this only bounds a stuck thread.
const TONE_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the thread waits between two turns of its loop when nothing
/// happens, so the end of the tone is noticed without a command.
const TONE_TICK: Duration = Duration::from_millis(100);

/// How long a finished stream is kept before it is closed, so the buffers
/// already queued — the end of the fade-out — are played rather than cut.
const TONE_TAIL: Duration = Duration::from_millis(500);

/// The envelope of frame `n`: a linear fade-in over the first
/// [`TONE_FADE_FRAMES`], a linear fade-out over the last, and 1 in between.
fn envelope(n: usize) -> f64 {
    let ramp = (TONE_FADE_FRAMES - 1) as f64;
    let from_end = TONE_FRAMES - 1 - n;
    if n < TONE_FADE_FRAMES {
        n as f64 / ramp
    } else if from_end < TONE_FADE_FRAMES {
        from_end as f64 / ramp
    } else {
        1.0
    }
}

/// The sample of frame `n`, the same on every channel.
fn tone_sample(n: usize) -> f32 {
    // The phase is taken from the frame index, not accumulated, so a chunk
    // read anywhere is exactly the same samples.
    let cycles = n as f64 * f64::from(TONE_FREQUENCY_HZ) / f64::from(TONE_RATE);
    let phase = std::f64::consts::TAU * cycles.fract();
    (f64::from(TONE_AMPLITUDE) * envelope(n) * phase.sin()) as f32
}

/// The frames `position..` of the tone that `count` frames reach, cut at its
/// end.
fn tone_range(position: usize, count: usize) -> std::ops::Range<usize> {
    position..position.saturating_add(count).min(TONE_FRAMES)
}

/// Up to `count` frames of the tone from frame `position`, stereo interleaved.
pub fn tone_frames(position: usize, count: usize) -> Vec<f32> {
    tone_range(position, count)
        .flat_map(|n| std::iter::repeat_n(tone_sample(n), TONE_CHANNELS))
        .collect()
}

/// Write the tone from frame `position` into `out`, as many whole frames as
/// fit, in the stream's format (F32LE, interleaved), and answer how many
/// frames were written. Nothing is allocated: this runs on the realtime
/// thread.
fn write_tone(out: &mut [u8], position: usize) -> usize {
    let range = tone_range(position, out.len() / TONE_STRIDE);
    let written = range.len();
    let (frames, _) = out.as_chunks_mut::<TONE_STRIDE>();
    for (frame, n) in frames.iter_mut().zip(range) {
        let bytes = tone_sample(n).to_le_bytes();
        for sample in frame.chunks_exact_mut(bytes.len()) {
            sample.copy_from_slice(&bytes);
        }
    }
    written
}

/// The properties of the tone's stream, pinning it to `target`.
///
/// An empty target is refused: it would leave the stream to autoconnect to the
/// default sink, the very fallback the pin exists to prevent.
pub(crate) fn tone_stream_props(target: &str) -> Result<Vec<(String, String)>, AudioError> {
    if target.is_empty() {
        return Err(AudioError::PipeWire(
            "empty tone target refused".to_string(),
        ));
    }
    Ok([
        ("media.type", "Audio"),
        ("media.category", "Playback"),
        ("node.name", TONE_NODE_NAME),
        ("target.object", target),
        // Never moved to the PC's own speakers when the target goes (#67)...
        ("node.dont-reconnect", "true"),
        // ...nor sent there when the target is missing at the first link.
        ("node.dont-fallback", "true"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect())
}

/// One request from the handle to the tone thread, carrying its reply channel.
enum ToneCommand {
    Start { target: String, reply: ToneReply },
    Pause { reply: ToneReply },
    Resume { reply: ToneReply },
    Stop { reply: ToneReply },
}

/// Where the tone thread sends the answer to one command.
type ToneReply = mpsc::Sender<Result<(), AudioError>>;

/// The real tone output: a PipeWire stream on a thread of its own.
///
/// The thread is started by the first `start`, never before, so building the
/// app touches no daemon.
pub struct PipeWireToneOutput {
    target: String,
    sender: Option<pw::channel::Sender<ToneCommand>>,
    thread: Option<JoinHandle<()>>,
    /// Raised by the thread once the tone ran out, or once its stream was lost
    /// without a `stop`.
    finished: Arc<AtomicBool>,
}

impl PipeWireToneOutput {
    /// An output playing into `target`, starting nothing until the first
    /// `start`.
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            sender: None,
            thread: None,
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the output's thread has been started.
    #[cfg(test)]
    pub(crate) fn has_thread(&self) -> bool {
        self.thread.is_some()
    }

    /// Forget a thread that has exited, so the next `start` starts another.
    fn forget_a_dead_thread(&mut self) {
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            self.thread = None;
            self.sender = None;
        }
    }

    /// Start the tone thread.
    fn spawn(&mut self) -> Result<(), AudioError> {
        let (sender, receiver) = pw::channel::channel::<ToneCommand>();
        let finished = Arc::clone(&self.finished);
        let thread = std::thread::Builder::new()
            .name("pipewire-tone".into())
            .spawn(move || run_tone_thread(receiver, finished))
            .map_err(|e| AudioError::PipeWire(format!("cannot start the tone thread: {e}")))?;
        self.sender = Some(sender);
        self.thread = Some(thread);
        Ok(())
    }

    /// Send the command `make` builds around a fresh reply channel to the
    /// thread, and wait for the answer. With no thread there is nothing to act
    /// on, and the answer is `Ok`.
    fn ask(&mut self, make: impl FnOnce(ToneReply) -> ToneCommand) -> Result<(), AudioError> {
        self.forget_a_dead_thread();
        let Some(sender) = &self.sender else {
            return Ok(());
        };
        let (reply, answer) = mpsc::channel();
        sender.send(make(reply)).map_err(|_| {
            AudioError::PipeWire("the PipeWire tone thread is not running".to_string())
        })?;
        answer
            .recv_timeout(TONE_REPLY_TIMEOUT)
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => AudioError::PipeWire(format!(
                    "PipeWire tone thread did not answer within {} s",
                    TONE_REPLY_TIMEOUT.as_secs()
                )),
                mpsc::RecvTimeoutError::Disconnected => AudioError::PipeWire(
                    "PipeWire tone thread dropped the command without answering".to_string(),
                ),
            })?
    }
}

impl AudioOutput for PipeWireToneOutput {
    fn start(&mut self) -> Result<(), AudioError> {
        // Refused before any thread, so an empty target creates no context.
        tone_stream_props(&self.target)?;
        self.forget_a_dead_thread();
        if self.sender.is_none() {
            self.spawn()?;
        }
        // Cloned: the command carries its own copy to the thread.
        let target = self.target.clone();
        self.ask(|reply| ToneCommand::Start { target, reply })
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        self.ask(|reply| ToneCommand::Resume { reply })
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        self.ask(|reply| ToneCommand::Pause { reply })
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        self.ask(|reply| ToneCommand::Stop { reply })
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

fn pw_error(what: &'static str) -> impl Fn(pw::Error) -> AudioError {
    move |e| AudioError::PipeWire(format!("{what}: {e}"))
}

/// The `EnumFormat` param the tone's stream offers: F32 interleaved, stereo,
/// at [`TONE_RATE`].
fn tone_format_pod() -> Result<Vec<u8>, AudioError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(TONE_RATE);
    info.set_channels(TONE_CHANNELS as u32);
    let mut position = [0; libspa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = libspa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = libspa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let value = Value::Object(Object {
        type_: libspa::sys::SPA_TYPE_OBJECT_Format,
        id: libspa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    PodSerializer::serialize(Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| AudioError::PipeWire(format!("cannot build the tone format: {e:?}")))
}

/// Fill the next buffer of `stream` from the tone at `position`, and raise
/// `finished` once the tone has nothing left.
fn fill_buffer(stream: &Stream, position: &AtomicUsize, finished: &AtomicBool) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let from = position.load(Ordering::Relaxed);
    let written = data.data().map_or(0, |out| write_tone(out, from));
    let reached = position.fetch_add(written, Ordering::Relaxed) + written;
    if written == 0 && reached >= TONE_FRAMES {
        finished.store(true, Ordering::Relaxed);
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = TONE_STRIDE as i32;
    *chunk.size_mut() = (written * TONE_STRIDE) as u32;
}

/// The stream playing the tone, and the listener carrying its callbacks.
struct Playing {
    listener: StreamListener<()>,
    stream: StreamRc,
}

impl Playing {
    /// Disconnect the stream, then drop its callbacks. With `RT_PROCESS`
    /// libpipewire keeps its own pointer to the `process` callback, so the
    /// callbacks outlive the stream's link to the data thread rather than the
    /// other way round. A deliberate close can raise `finished` through the
    /// `Unconnected` it causes; every caller resets or wants it afterwards.
    fn close(self) {
        let _ = self.stream.disconnect();
        drop(self.listener);
    }
}

/// A connection to the daemon, and whether it has been lost.
struct Connection {
    // Field order is drop order: the listener before the core that owns it.
    _listener: pw::core::Listener,
    core: CoreRc,
    lost: Rc<Cell<bool>>,
}

/// What the tone thread owns. It keeps one context for its whole life: #79
/// showed that destroying a context joins `module-rt`, which can block on
/// RTKit for 25 s.
struct ToneLoop {
    // Field order is drop order: the stream before the core, the core before
    // the context.
    playing: Option<Playing>,
    connection: Option<Connection>,
    context: ContextRc,
    finished: Arc<AtomicBool>,
    ended_at: Option<Instant>,
    /// Where the tone is, shared with the process callback on the data loop,
    /// so a resume can reconnect the stream without losing its place.
    position: Arc<AtomicUsize>,
    /// The node the tone is pinned to, kept for a resume.
    target: Option<String>,
}

impl ToneLoop {
    /// The live core, connecting again when there is none or it was lost.
    fn core(&mut self) -> Result<CoreRc, AudioError> {
        if self.connection.as_ref().is_some_and(|c| c.lost.get()) {
            self.close_stream();
            self.connection = None;
        }
        if let Some(connection) = &self.connection {
            // Cloned: the stream keeps a reference to the core it lives on.
            return Ok(connection.core.clone());
        }
        let core = self
            .context
            .connect_rc(None)
            .map_err(pw_error("cannot connect to PipeWire"))?;
        let lost = Rc::new(Cell::new(false));
        let listener = core
            .add_listener_local()
            .error({
                let lost = Rc::clone(&lost);
                move |id, _seq, res, message| {
                    if id == pw::core::PW_ID_CORE {
                        tracing::warn!("PipeWire tone connection lost ({res}): {message}");
                        lost.set(true);
                    }
                }
            })
            .register();
        // Cloned: the connection keeps one reference, the stream another.
        let handed = core.clone();
        self.connection = Some(Connection {
            _listener: listener,
            core,
            lost,
        });
        Ok(handed)
    }

    fn close_stream(&mut self) {
        if let Some(playing) = self.playing.take() {
            playing.close();
        }
        self.ended_at = None;
    }

    /// (Re)connect the tone's stream from frame 0.
    fn start(&mut self, target: &str) -> Result<(), AudioError> {
        self.target = Some(target.to_string());
        self.open(target, 0)
    }

    /// Resume a paused tone where it stopped, on a fresh stream. The one it
    /// was paused on may have lost its link meanwhile: deselecting the last
    /// speaker pauses the tone, then tears the combined sink down, and
    /// `node.dont-reconnect` keeps the stream off the sink built next — so
    /// reactivating it left an unlinked stream that nothing drove, silent
    /// and never finishing (#66).
    fn resume(&mut self) -> Result<(), AudioError> {
        let (Some(_), Some(target)) = (&self.playing, self.target.clone()) else {
            return Ok(());
        };
        let from = self.position.load(Ordering::Relaxed);
        self.open(&target, from)
    }

    /// Connect a stream pinned to `target`, playing the tone from frame `from`.
    fn open(&mut self, target: &str, from: usize) -> Result<(), AudioError> {
        self.close_stream();
        self.finished.store(false, Ordering::Relaxed);
        self.position.store(from, Ordering::Relaxed);
        let mut properties = PropertiesBox::new();
        for (key, value) in tone_stream_props(target)? {
            properties.insert(key, value);
        }
        let core = self.core()?;
        let stream = StreamRc::new(core, TONE_NODE_NAME, properties)
            .map_err(pw_error("cannot create the tone stream"))?;
        let on_state = Arc::clone(&self.finished);
        let on_process = Arc::clone(&self.finished);
        let position = Arc::clone(&self.position);
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _, new| {
                // A stream that went away, on its own or closed on purpose;
                // see `Playing::close` for why the latter is harmless.
                if matches!(new, StreamState::Unconnected | StreamState::Error(_)) {
                    on_state.store(true, Ordering::Relaxed);
                }
            })
            .process(move |stream, _| fill_buffer(stream, &position, &on_process))
            .register()
            .map_err(pw_error("cannot listen to the tone stream"))?;
        let format = tone_format_pod()?;
        let pod = Pod::from_bytes(&format)
            .ok_or_else(|| AudioError::PipeWire("malformed tone format".to_string()))?;
        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut [pod],
            )
            .map_err(pw_error("cannot connect the tone stream"))?;
        self.playing = Some(Playing { listener, stream });
        Ok(())
    }

    fn set_active(&mut self, active: bool) -> Result<(), AudioError> {
        let Some(playing) = &self.playing else {
            return Ok(());
        };
        playing
            .stream
            .set_active(active)
            .map_err(pw_error("cannot pause or resume the tone stream"))
    }

    fn stop(&mut self) {
        self.close_stream();
        self.finished.store(false, Ordering::Relaxed);
    }

    /// Close a finished stream once its tail has had time to play.
    fn close_a_finished_stream(&mut self) {
        if self.playing.is_none() || !self.finished.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let ended_at = *self.ended_at.get_or_insert(now);
        if now.duration_since(ended_at) >= TONE_TAIL {
            self.close_stream();
        }
    }

    fn handle(&mut self, command: ToneCommand) {
        // A reply nobody waits for any more (the handle timed out) is dropped.
        match command {
            ToneCommand::Start { target, reply } => {
                let _ = reply.send(self.start(&target));
            },
            ToneCommand::Pause { reply } => {
                let _ = reply.send(self.set_active(false));
            },
            ToneCommand::Resume { reply } => {
                let _ = reply.send(self.resume());
            },
            ToneCommand::Stop { reply } => {
                self.stop();
                let _ = reply.send(Ok(()));
            },
        }
    }
}

/// The tone thread: one main loop and one context for its whole life,
/// answering commands between turns of the loop.
fn run_tone_thread(receiver: pw::channel::Receiver<ToneCommand>, finished: Arc<AtomicBool>) {
    pw::init();
    let mainloop = match MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(e) => {
            tracing::error!("cannot create the PipeWire tone loop: {e}");
            return;
        },
    };
    let context = match ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(e) => {
            tracing::error!("cannot create the PipeWire tone context: {e}");
            return;
        },
    };
    let inbox: Rc<RefCell<VecDeque<ToneCommand>>> = Rc::default();
    let _attached = receiver.attach(mainloop.loop_(), {
        let inbox = Rc::clone(&inbox);
        move |command| inbox.borrow_mut().push_back(command)
    });
    let mut state = ToneLoop {
        playing: None,
        connection: None,
        context,
        finished,
        ended_at: None,
        position: Arc::default(),
        target: None,
    };
    loop {
        mainloop.loop_().iterate(Timeout::Finite(TONE_TICK));
        loop {
            let next = inbox.borrow_mut().pop_front();
            let Some(command) = next else {
                break;
            };
            state.handle(command);
        }
        state.close_a_finished_stream();
    }
}

#[cfg(test)]
mod tests;
