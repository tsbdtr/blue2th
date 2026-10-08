// SPDX-License-Identifier: MIT OR Apache-2.0

use super::testing::describe;
use super::*;
use crate::audio::{bluez_sink_prefix, CONFIRM_GAP};
use crate::graph::fake::{FakeGraph, GraphCall, GraphOp};
use std::sync::Mutex;
use std::time::Duration;

/// The JBL Xtreme 3 and the WH-1000XM5, with their sinks, as `lib.rs`'s
/// tests name them.
const JBL: &str = "2C:FD:B4:D3:AC:21";
const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";
const SONY: &str = "80:99:E7:63:50:29";
const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";
const COMBINED: &str = "blue2th_combined";

/// The receiving end of a message's reply.
type Answer<T> = oneshot::Receiver<Result<T, RouterError>>;

fn target(mac: &str, offset_ms: u32) -> SpeakerTarget {
    SpeakerTarget {
        address: mac.to_string(),
        offset_ms,
    }
}

fn macs(list: &[&str]) -> Vec<String> {
    list.iter().map(|mac| mac.to_string()).collect()
}

// ─── Builders: one message of each kind, with the end of its reply ──────

fn route(speakers: &[SpeakerTarget], start_by: Option<Instant>) -> (Envelope, Answer<()>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::Route {
        speakers: speakers.to_vec(),
        reply,
    };
    (Envelope { start_by, message }, answer)
}

fn read(list: &[&str], start_by: Option<Instant>) -> (Envelope, Answer<Vec<Option<f32>>>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::SinkVolumes {
        macs: macs(list),
        reply,
    };
    (Envelope { start_by, message }, answer)
}

fn set(list: &[&str], level: f32, start_by: Option<Instant>) -> (Envelope, Answer<()>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::SetSinkVolumes {
        macs: macs(list),
        level,
        reply,
    };
    (Envelope { start_by, message }, answer)
}

/// A retune of `mac`'s branch inside [`COMBINED`] to `latency_ms`.
fn retune(mac: &str, latency_ms: u32, start_by: Option<Instant>) -> (Envelope, Answer<()>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::Retune {
        sink_name: COMBINED.to_string(),
        branch: CombineBranch {
            sink: bluez_sink_prefix(mac),
            latency_ms,
        },
        reply,
    };
    (Envelope { start_by, message }, answer)
}

fn spotify(speakers: &[SpeakerTarget], start_by: Option<Instant>) -> (Envelope, Answer<String>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::RouteForSpotify {
        speakers: speakers.to_vec(),
        reply,
    };
    (Envelope { start_by, message }, answer)
}

/// An apply-selection is a background message: it carries no `start_by`.
fn apply(speakers: &[SpeakerTarget], generation: u64) -> (Envelope, Answer<()>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::ApplySelection {
        speakers: speakers.to_vec(),
        generation,
        reply,
    };
    (
        Envelope {
            start_by: None,
            message,
        },
        answer,
    )
}

fn repair(
    speakers: &[SpeakerTarget],
    start_by: Option<Instant>,
) -> (Envelope, Answer<RepairOutcome>) {
    let (reply, answer) = oneshot::channel();
    let message = Message::Repair {
        speakers: speakers.to_vec(),
        reply,
    };
    (Envelope { start_by, message }, answer)
}

/// What a reply holds right now.
#[derive(Debug, PartialEq)]
enum Held {
    /// Nothing was sent, and the message — its reply sender — is alive.
    Nothing,
    /// Nothing was sent, and the message is gone.
    Dropped,
    /// `Err(RouterError::Audio(AudioError::Expired))`.
    Expired,
    /// `Err(RouterError::Superseded)`.
    Superseded,
    /// `Err(RouterError::Outdated)`.
    Outdated,
    /// Any other answer.
    Other(String),
}

/// Take what `answer` holds, without waiting.
fn held<T: std::fmt::Debug>(answer: &mut Answer<T>) -> Held {
    match answer.try_recv() {
        Err(oneshot::error::TryRecvError::Empty) => Held::Nothing,
        Err(oneshot::error::TryRecvError::Closed) => Held::Dropped,
        Ok(Err(RouterError::Audio(AudioError::Expired))) => Held::Expired,
        Ok(Err(RouterError::Superseded)) => Held::Superseded,
        Ok(Err(RouterError::Outdated)) => Held::Outdated,
        Ok(other) => Held::Other(format!("{other:?}")),
    }
}

/// A queue holding `envelopes`, in order.
fn queue_of(envelopes: Vec<Envelope>) -> Queue {
    let mut queue = Queue::new();
    for envelope in envelopes {
        queue.push(envelope);
    }
    queue
}

/// Take every message `queue` yields at `now` and `generation`, in order.
/// The messages are returned alive, so the reply of one that was yielded
/// reads as [`Held::Nothing`] rather than as dropped.
fn take_all(queue: &mut Queue, now: Instant, generation: u64) -> Vec<Message> {
    let mut yielded = Vec::new();
    while let Some(message) = queue.take_next(now, generation) {
        yielded.push(message);
        assert!(yielded.len() <= 64, "the queue never runs dry");
    }
    yielded
}

fn described(messages: &[Message]) -> Vec<String> {
    messages.iter().map(describe).collect()
}

// ─── The queue (pure: no thread, no runtime, explicit instants) ─────────

// Criterion: messages start in the order they were sent — one of each
// kind, requests and background messages mixed, every one in time. The
// queue answers nothing for a message it yields.
#[test]
fn test_take_next_yields_the_messages_in_the_order_they_were_sent() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let (set_it, mut set_answer) = set(&[SONY], 0.25, in_time);
    let (route_it, mut route_answer) = route(&[target(JBL, 40)], in_time);
    let (repair_it, mut repair_answer) = repair(&[target(JBL, 0)], None);
    let (read_it, mut read_answer) = read(&[JBL, SONY], in_time);
    let (apply_it, mut apply_answer) = apply(&[target(SONY, 70)], 3);
    let (retune_it, mut retune_answer) = retune(SONY, 120, in_time);
    let (spotify_it, mut spotify_answer) = spotify(&[target(JBL, 10)], None);
    let mut queue = queue_of(vec![
        set_it, route_it, repair_it, read_it, apply_it, retune_it, spotify_it,
    ]);
    assert_eq!(queue.len(), 7);

    let yielded = take_all(&mut queue, base, 3);

    assert_eq!(
        described(&yielded),
        vec![
            format!("SetSinkVolumes [{SONY}] 0.25"),
            format!("Route [{JBL}@40]"),
            format!("Repair [{JBL}@0]"),
            format!("SinkVolumes [{JBL},{SONY}]"),
            format!("ApplySelection [{SONY}@70] 3"),
            format!("Retune {COMBINED} bluez_output.80_99_E7_63_50_29 120"),
            format!("RouteForSpotify [{JBL}@10]"),
        ]
    );
    assert!(queue.is_empty());
    assert_eq!(held(&mut set_answer), Held::Nothing);
    assert_eq!(held(&mut route_answer), Held::Nothing);
    assert_eq!(held(&mut repair_answer), Held::Nothing);
    assert_eq!(held(&mut read_answer), Held::Nothing);
    assert_eq!(held(&mut apply_answer), Held::Nothing);
    assert_eq!(held(&mut retune_answer), Held::Nothing);
    assert_eq!(held(&mut spotify_answer), Held::Nothing);
}

// Edge case: an empty queue yields nothing.
#[test]
fn test_take_next_of_an_empty_queue_yields_nothing() {
    let mut queue = Queue::new();

    assert!(queue.take_next(Instant::now(), 0).is_none());
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);
}

// Criterion: a message whose reply is closed is not started and is taken
// out of the queue — a request and a background message alike. The near
// miss is the third message: the same route with its reply open, which is
// yielded. A queue holding only closed messages yields nothing and ends
// up empty.
#[test]
fn test_take_next_takes_out_a_message_whose_reply_is_closed_without_starting_it() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let (left, left_answer) = route(&[target(JBL, 40)], in_time);
    let (left_too, left_too_answer) = repair(&[target(JBL, 40)], None);
    let (waiting, mut waiting_answer) = route(&[target(JBL, 40)], in_time);
    drop(left_answer);
    drop(left_too_answer);
    let mut queue = queue_of(vec![left, left_too, waiting]);

    let first = queue.take_next(base, 0);

    assert_eq!(
        first.as_ref().map(describe),
        Some(format!("Route [{JBL}@40]"))
    );
    assert_eq!(
        held(&mut waiting_answer),
        Held::Nothing,
        "the message yielded is the one whose caller still waits"
    );
    assert!(queue.is_empty(), "the closed messages left the queue");
    assert!(queue.take_next(base, 0).is_none());

    let (alone, alone_answer) = set(&[JBL], 0.5, in_time);
    drop(alone_answer);
    let mut queue = queue_of(vec![alone]);
    assert!(queue.take_next(base, 0).is_none());
    assert!(queue.is_empty());
}

// Criterion (#146, kept): a request taken out at `now > start_by` is not
// started and its reply receives `AudioError::Expired` — here one
// nanosecond past, the smallest "strictly past" an `Instant` holds. It
// answers once, the message is gone with it, and the queue goes on to
// the message behind it.
#[test]
fn test_take_next_one_nanosecond_past_start_by_answers_expired_and_does_not_start_the_message() {
    let base = Instant::now();
    let (late, mut late_answer) = set(&[JBL], 0.25, Some(base + Duration::from_millis(300)));
    let (in_time, mut in_time_answer) =
        route(&[target(SONY, 0)], Some(base + Duration::from_millis(900)));
    let mut queue = queue_of(vec![late, in_time]);

    let yielded = queue.take_next(base + Duration::from_nanos(300_000_001), 0);

    assert_eq!(
        yielded.as_ref().map(describe),
        Some(format!("Route [{SONY}@0]")),
        "the expired set is not handed on; the route behind it is"
    );
    assert_eq!(held(&mut late_answer), Held::Expired);
    assert_eq!(
        held(&mut late_answer),
        Held::Dropped,
        "one answer, then the message is gone"
    );
    assert_eq!(held(&mut in_time_answer), Held::Nothing);
    assert!(queue.is_empty());
}

// Criterion (#146, guard): expired means strictly past `start_by`. The
// near miss is `now == start_by`, which a `>=` check expires: the message
// is yielded, and nothing is sent on its reply.
#[test]
fn test_take_next_at_start_by_starts_the_message_and_answers_nothing() {
    let base = Instant::now();
    let start_by = base + Duration::from_millis(300);
    let (at, mut at_answer) = set(&[JBL], 0.25, Some(start_by));
    let mut queue = queue_of(vec![at]);

    let yielded = queue.take_next(start_by, 0);

    assert_eq!(
        yielded.as_ref().map(describe),
        Some(format!("SetSinkVolumes [{JBL}] 0.25"))
    );
    assert_eq!(held(&mut at_answer), Held::Nothing);
}

/// Expire `envelope` — taken out one nanosecond past the `start_by` of
/// `base + 300 ms` the builders below stamp — and report the name it gives
/// the log, whether it was yielded all the same, and what its reply holds.
fn expire<T: std::fmt::Debug>(
    base: Instant,
    (envelope, mut answer): (Envelope, Answer<T>),
) -> (&'static str, bool, Held) {
    let name = envelope.message.name();
    let mut queue = queue_of(vec![envelope]);
    let yielded = queue.take_next(base + Duration::from_nanos(300_000_001), 5);
    // Read while `yielded` is alive: a message handed on anyway still
    // holds its reply sender, and reads as `Nothing`, not as `Dropped`.
    let reply_holds = held(&mut answer);
    (name, yielded.is_some(), reply_holds)
}

/// One message of each kind, expired, in declaration order. The start
/// deadline is a property of the send, so every kind can carry one —
/// the two background-only ones included.
fn expire_one_of_each() -> [(&'static str, bool, Held); 7] {
    let base = Instant::now();
    let by = Some(base + Duration::from_millis(300));
    let (mut stamped_apply, apply_answer) = apply(&[target(JBL, 0)], 5);
    stamped_apply.start_by = by;
    [
        expire(base, route(&[target(JBL, 0)], by)),
        expire(base, read(&[JBL], by)),
        expire(base, set(&[JBL], 0.25, by)),
        expire(base, retune(JBL, 120, by)),
        expire(base, spotify(&[target(JBL, 0)], by)),
        expire(base, (stamped_apply, apply_answer)),
        expire(base, repair(&[target(JBL, 0)], by)),
    ]
}

/// The seven variants of [`Message`], in declaration order.
const VARIANTS: [&str; 7] = [
    "Route",
    "SinkVolumes",
    "SetSinkVolumes",
    "Retune",
    "RouteForSpotify",
    "ApplySelection",
    "Repair",
];

// Criterion (guard): every `Message` variant, all seven, answers
// `AudioError::Expired` on its own reply when expired. The near miss is a
// variant reaching an arm that drops its reply: its caller would read a
// dropped reply, not an expiry — `Held::Dropped` here, where
// `Held::Expired` is wanted, per variant.
#[test]
fn test_take_next_answers_expired_on_the_reply_of_each_of_the_seven_messages() {
    let expired = expire_one_of_each();

    let names = expired.each_ref().map(|(name, _, _)| *name);
    assert_eq!(names, VARIANTS, "the table names every variant, once");
    for (name, yielded, reply) in &expired {
        assert!(!yielded, "{name} was handed on although expired");
        assert_eq!(*reply, Held::Expired, "{name} did not answer its expiry");
    }
}

// Criterion: the log of an expiry names the message by its variant, and
// nothing else — as #146 logs a command's. The line is compared whole,
// one message of each kind, each naming the JBL: a line built from an
// empty name, from another variant's, or carrying the message's
// arguments differs from it.
#[test]
fn test_the_expiry_line_names_the_message_by_its_variant_and_nothing_else() {
    let jbl = [target(JBL, 40)];
    let messages = [
        route(&jbl, None).0,
        read(&[JBL], None).0,
        set(&[JBL], 0.25, None).0,
        retune(JBL, 120, None).0,
        spotify(&jbl, None).0,
        apply(&jbl, 3).0,
        repair(&jbl, None).0,
    ]
    .map(|envelope| envelope.message);

    let lines = messages
        .each_ref()
        .map(|message| expiry_line(message, Duration::from_millis(7)));

    assert_eq!(
        lines,
        VARIANTS.map(|variant| format!(
            "router message {variant} expired: taken out of the queue 7 ms past its start_by"
        ))
    );
}

// Criterion (guard, a background message never expires): a background
// message carries no `start_by` and is started however long it waited.
// The near miss is the request queued beside it, which waited the very
// same hour and must expire: one queue, both messages. A queue that
// expired by age would drop the repair; one that never expired would run
// the route.
#[test]
fn test_take_next_starts_a_background_message_that_waited_an_hour_beside_a_request_that_expired() {
    let base = Instant::now();
    let (request, mut request_answer) =
        route(&[target(JBL, 0)], Some(base + Duration::from_millis(300)));
    let (background, mut background_answer) = repair(&[target(JBL, 0)], None);
    let (background_too, mut background_too_answer) = apply(&[target(JBL, 0)], 0);
    let mut queue = queue_of(vec![request, background, background_too]);

    let yielded = take_all(&mut queue, base + Duration::from_secs(3600), 0);

    assert_eq!(
        described(&yielded),
        vec![
            format!("Repair [{JBL}@0]"),
            format!("ApplySelection [{JBL}@0] 0"),
        ]
    );
    assert_eq!(held(&mut request_answer), Held::Expired);
    assert_eq!(held(&mut background_answer), Held::Nothing);
    assert_eq!(held(&mut background_too_answer), Held::Nothing);
}

// Criterion: a volume set is superseded when a later queued set, whose
// reply is still open, names every one of its sinks — it is not started
// and answers `Superseded`. Three shapes of "names every one": the same
// sink; a superset; the same sinks in another order. The later set need
// not be the next message: a route sits between two of them. The winner
// is yielded with its own level and answers nothing here.
#[test]
fn test_take_next_supersedes_a_set_whose_every_sink_a_later_open_set_names() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));

    let (older, mut older_answer) = set(&[JBL], 0.3, in_time);
    let (between, mut between_answer) = route(&[target(JBL, 0)], in_time);
    let (newer, mut newer_answer) = set(&[JBL], 0.5, in_time);
    let mut queue = queue_of(vec![older, between, newer]);
    let yielded = take_all(&mut queue, base, 0);
    assert_eq!(
        described(&yielded),
        vec![
            format!("Route [{JBL}@0]"),
            format!("SetSinkVolumes [{JBL}] 0.5"),
        ]
    );
    assert_eq!(held(&mut older_answer), Held::Superseded);
    assert_eq!(held(&mut between_answer), Held::Nothing);
    assert_eq!(held(&mut newer_answer), Held::Nothing);

    let (older, mut older_answer) = set(&[JBL], 0.3, in_time);
    let (wider, mut wider_answer) = set(&[SONY, JBL], 0.5, in_time);
    let mut queue = queue_of(vec![older, wider]);
    let yielded = take_all(&mut queue, base, 0);
    assert_eq!(
        described(&yielded),
        vec![format!("SetSinkVolumes [{SONY},{JBL}] 0.5")]
    );
    assert_eq!(held(&mut older_answer), Held::Superseded);
    assert_eq!(held(&mut wider_answer), Held::Nothing);

    let (older, mut older_answer) = set(&[JBL, SONY], 0.3, in_time);
    let (swapped, mut swapped_answer) = set(&[SONY, JBL], 0.5, in_time);
    let mut queue = queue_of(vec![older, swapped]);
    let yielded = take_all(&mut queue, base, 0);
    assert_eq!(
        described(&yielded),
        vec![format!("SetSinkVolumes [{SONY},{JBL}] 0.5")]
    );
    assert_eq!(held(&mut older_answer), Held::Superseded);
    assert_eq!(held(&mut swapped_answer), Held::Nothing);
}

// Criterion (guard, a set is superseded only when every sink is
// covered): `Set [JBL, SONY] 0.3` then `Set [JBL] 0.5`. A rule keyed on
// "any common sink" or "a later set exists" supersedes the first and
// leaves the Sony at its old level: the first is yielded whole, both
// sinks, then the second.
#[test]
fn test_take_next_runs_a_set_whole_when_a_later_set_names_only_part_of_its_sinks() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let (both, mut both_answer) = set(&[JBL, SONY], 0.3, in_time);
    let (one, mut one_answer) = set(&[JBL], 0.5, in_time);
    let mut queue = queue_of(vec![both, one]);

    let yielded = take_all(&mut queue, base, 0);

    assert_eq!(
        described(&yielded),
        vec![
            format!("SetSinkVolumes [{JBL},{SONY}] 0.3"),
            format!("SetSinkVolumes [{JBL}] 0.5"),
        ]
    );
    assert_eq!(held(&mut both_answer), Held::Nothing);
    assert_eq!(held(&mut one_answer), Held::Nothing);
}

// Criterion (guard, only a set whose reply is open supersedes):
// `Set [JBL] 0.3` then `Set [JBL] 0.5` whose caller already left.
// Superseding on it applies neither level: the first is yielded, and the
// second is taken out as any closed message is.
#[test]
fn test_take_next_does_not_supersede_a_set_on_a_later_set_whose_caller_left() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let (waiting, mut waiting_answer) = set(&[JBL], 0.3, in_time);
    let (left, left_answer) = set(&[JBL], 0.5, in_time);
    drop(left_answer);
    let mut queue = queue_of(vec![waiting, left]);

    let yielded = take_all(&mut queue, base, 0);

    assert_eq!(
        described(&yielded),
        vec![format!("SetSinkVolumes [{JBL}] 0.3")]
    );
    assert_eq!(held(&mut waiting_answer), Held::Nothing);
    assert!(queue.is_empty());
}

// Criterion (guard, only a later *set* supersedes): a set is not
// superseded by a later message of another kind naming the same speaker
// — a read, a route, a repair. The near miss is a rule keyed on "a later
// message for the same speakers".
#[test]
fn test_take_next_does_not_supersede_a_set_on_a_later_message_that_is_not_a_set() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let (the_set, mut set_answer) = set(&[JBL], 0.3, in_time);
    let (a_read, _read_answer) = read(&[JBL], in_time);
    let (a_route, _route_answer) = route(&[target(JBL, 0)], in_time);
    let (a_repair, _repair_answer) = repair(&[target(JBL, 0)], None);
    let mut queue = queue_of(vec![the_set, a_read, a_route, a_repair]);

    let first = queue.take_next(base, 0);

    assert_eq!(
        first.as_ref().map(describe),
        Some(format!("SetSinkVolumes [{JBL}] 0.3"))
    );
    assert_eq!(held(&mut set_answer), Held::Nothing);
    assert_eq!(queue.len(), 3);
}

// Criterion: the checks run in this order on the message at the head —
// reply closed, expired, superseded. An expired set answers `Expired`
// even when a later set covers it: the near miss is the later set, in
// time and open, which a supersede check run first would answer
// `Superseded` on.
#[test]
fn test_take_next_answers_expired_to_an_expired_set_a_later_set_covers() {
    let base = Instant::now();
    let (late, mut late_answer) = set(&[JBL], 0.3, Some(base + Duration::from_millis(300)));
    let (in_time, mut in_time_answer) = set(&[JBL], 0.5, Some(base + Duration::from_millis(900)));
    let mut queue = queue_of(vec![late, in_time]);

    let yielded = take_all(&mut queue, base + Duration::from_millis(500), 0);

    assert_eq!(
        described(&yielded),
        vec![format!("SetSinkVolumes [{JBL}] 0.5")]
    );
    assert_eq!(held(&mut late_answer), Held::Expired);
    assert_eq!(held(&mut in_time_answer), Held::Nothing);
}

// Criterion (guard, the empty value): `SetSinkVolumes []` is covered by
// nothing and covers nothing. "Every sink of the empty set is named by a
// later set" is true of any set, so an unguarded rule supersedes the
// empty set on the JBL's; and the empty set names none of the JBL's
// sinks, so it must not supersede it either. Each pair is yielded whole,
// in order.
#[test]
fn test_an_empty_set_is_superseded_by_no_later_set_and_supersedes_none() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));

    let (empty, mut empty_answer) = set(&[], 0.3, in_time);
    let (jbl, mut jbl_answer) = set(&[JBL], 0.5, in_time);
    let mut queue = queue_of(vec![empty, jbl]);
    let yielded = take_all(&mut queue, base, 0);
    assert_eq!(
        described(&yielded),
        vec![
            "SetSinkVolumes [] 0.3".to_string(),
            format!("SetSinkVolumes [{JBL}] 0.5"),
        ]
    );
    assert_eq!(held(&mut empty_answer), Held::Nothing);
    assert_eq!(held(&mut jbl_answer), Held::Nothing);

    let (jbl, mut jbl_answer) = set(&[JBL], 0.3, in_time);
    let (empty, mut empty_answer) = set(&[], 0.5, in_time);
    let mut queue = queue_of(vec![jbl, empty]);
    let yielded = take_all(&mut queue, base, 0);
    assert_eq!(
        described(&yielded),
        vec![
            format!("SetSinkVolumes [{JBL}] 0.3"),
            "SetSinkVolumes [] 0.5".to_string(),
        ]
    );
    assert_eq!(held(&mut jbl_answer), Held::Nothing);
    assert_eq!(held(&mut empty_answer), Held::Nothing);
}

// Criterion (guard, routing messages are never coalesced): two identical
// messages of each routing kind — route, apply-selection, repair, retune,
// route-for-Spotify — queued one behind the other. A rule that
// deduplicates "identical queued messages" yields five; all ten are
// yielded, in order, and none is answered by the queue.
#[test]
fn test_take_next_never_merges_or_supersedes_routing_messages() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(300));
    let speakers = [target(JBL, 0), target(SONY, 70)];
    let (route_a, mut route_a_answer) = route(&speakers, in_time);
    let (route_b, mut route_b_answer) = route(&speakers, in_time);
    let (apply_a, mut apply_a_answer) = apply(&speakers, 2);
    let (apply_b, mut apply_b_answer) = apply(&speakers, 2);
    let (repair_a, mut repair_a_answer) = repair(&speakers, None);
    let (repair_b, mut repair_b_answer) = repair(&speakers, None);
    let (retune_a, mut retune_a_answer) = retune(SONY, 120, in_time);
    let (retune_b, mut retune_b_answer) = retune(SONY, 120, in_time);
    let (spotify_a, mut spotify_a_answer) = spotify(&speakers, in_time);
    let (spotify_b, mut spotify_b_answer) = spotify(&speakers, in_time);
    let mut queue = queue_of(vec![
        route_a, route_b, apply_a, apply_b, repair_a, repair_b, retune_a, retune_b, spotify_a,
        spotify_b,
    ]);

    let yielded = take_all(&mut queue, base, 2);

    let selection = format!("{JBL}@0,{SONY}@70");
    let retuned = format!("Retune {COMBINED} bluez_output.80_99_E7_63_50_29 120");
    assert_eq!(
        described(&yielded),
        vec![
            format!("Route [{selection}]"),
            format!("Route [{selection}]"),
            format!("ApplySelection [{selection}] 2"),
            format!("ApplySelection [{selection}] 2"),
            format!("Repair [{selection}]"),
            format!("Repair [{selection}]"),
            // Cloned: the same line is expected twice.
            retuned.clone(),
            retuned,
            format!("RouteForSpotify [{selection}]"),
            format!("RouteForSpotify [{selection}]"),
        ]
    );
    assert_eq!(held(&mut route_a_answer), Held::Nothing);
    assert_eq!(held(&mut route_b_answer), Held::Nothing);
    assert_eq!(held(&mut apply_a_answer), Held::Nothing);
    assert_eq!(held(&mut apply_b_answer), Held::Nothing);
    assert_eq!(held(&mut repair_a_answer), Held::Nothing);
    assert_eq!(held(&mut repair_b_answer), Held::Nothing);
    assert_eq!(held(&mut retune_a_answer), Held::Nothing);
    assert_eq!(held(&mut retune_b_answer), Held::Nothing);
    assert_eq!(held(&mut spotify_a_answer), Held::Nothing);
    assert_eq!(held(&mut spotify_b_answer), Held::Nothing);
}

// Criterion: an apply-selection stamped with a routing generation older
// than the current one is not started and answers that it is outdated.
// The near miss is the message stamped with the current generation,
// queued right behind it, which is yielded.
#[test]
fn test_take_next_does_not_start_an_apply_selection_stamped_with_an_older_generation() {
    let base = Instant::now();
    let (stale, mut stale_answer) = apply(&[target(JBL, 0), target(SONY, 0)], 3);
    let (current, mut current_answer) = apply(&[target(JBL, 0)], 4);
    let mut queue = queue_of(vec![stale, current]);

    let yielded = take_all(&mut queue, base, 4);

    assert_eq!(
        described(&yielded),
        vec![format!("ApplySelection [{JBL}@0] 4")]
    );
    assert_eq!(held(&mut stale_answer), Held::Outdated);
    assert_eq!(
        held(&mut stale_answer),
        Held::Dropped,
        "one answer, then the message is gone"
    );
    assert_eq!(held(&mut current_answer), Held::Nothing);
}

// Criterion: taking the next message answers, on the way, every message
// that is not to run — closed, expired, superseded, outdated — each on
// its own reply, in one call, and yields the first one that is.
#[test]
fn test_take_next_answers_every_message_not_to_run_on_its_way_to_the_next_one() {
    let base = Instant::now();
    let in_time = Some(base + Duration::from_millis(900));
    let (left, left_answer) = read(&[JBL], in_time);
    drop(left_answer);
    let (late, mut late_answer) = route(&[target(JBL, 0)], Some(base));
    let (covered, mut covered_answer) = set(&[JBL], 0.3, in_time);
    let (stale, mut stale_answer) = apply(&[target(JBL, 0), target(SONY, 0)], 6);
    let (runs, mut runs_answer) = retune(JBL, 120, in_time);
    let (winner, mut winner_answer) = set(&[JBL], 0.5, in_time);
    let mut queue = queue_of(vec![left, late, covered, stale, runs, winner]);

    let yielded = queue.take_next(base + Duration::from_millis(500), 7);

    assert_eq!(
        yielded.as_ref().map(describe),
        Some(format!(
            "Retune {COMBINED} bluez_output.2C_FD_B4_D3_AC_21 120"
        ))
    );
    assert_eq!(held(&mut late_answer), Held::Expired);
    assert_eq!(held(&mut covered_answer), Held::Superseded);
    assert_eq!(held(&mut stale_answer), Held::Outdated);
    assert_eq!(held(&mut runs_answer), Held::Nothing);
    assert_eq!(held(&mut winner_answer), Held::Nothing);
    assert_eq!(queue.len(), 1, "only the winning set is left");
}

// Criterion: after a volume read ran, every queued read for the same
// speakers receives the same answer and leaves the queue, whatever its
// `start_by` — the second one's is an hour past: the read it gets is
// newer than its request. One whose reply is closed is dropped. The
// route queued among them stays, unanswered.
#[test]
fn test_answer_duplicate_reads_answers_every_queued_read_for_the_same_speakers_whatever_its_start_by(
) {
    let base = Instant::now() + Duration::from_secs(7200);
    let (in_time, mut in_time_answer) = read(&[JBL, SONY], Some(base + Duration::from_millis(300)));
    let (long_late, mut long_late_answer) =
        read(&[JBL, SONY], Some(base - Duration::from_secs(3600)));
    let (a_route, mut route_answer) = route(&[target(JBL, 0)], None);
    let (background, mut background_answer) = read(&[JBL, SONY], None);
    let (left, left_answer) = read(&[JBL, SONY], None);
    drop(left_answer);
    let mut queue = queue_of(vec![in_time, long_late, a_route, background, left]);

    queue.answer_duplicate_reads(&macs(&[JBL, SONY]), &Ok(vec![Some(0.4), None]));

    let expected = Held::Other("Ok([Some(0.4), None])".to_string());
    assert_eq!(held(&mut in_time_answer), expected);
    assert_eq!(held(&mut long_late_answer), expected);
    assert_eq!(held(&mut background_answer), expected);
    assert_eq!(held(&mut route_answer), Held::Nothing);
    assert_eq!(queue.len(), 1, "only the route is left");
    let left_over = queue.take_next(base, 0);
    assert_eq!(
        left_over.as_ref().map(describe),
        Some(format!("Route [{JBL}@0]"))
    );
}

// Criterion: a read that failed answers its duplicates with the same
// failure, message included — the graph did not answer any of them.
#[test]
fn test_answer_duplicate_reads_hands_on_a_failed_read_as_the_same_error() {
    let (first, mut first_answer) = read(&[JBL], None);
    let (second, mut second_answer) = read(&[JBL], None);
    let mut queue = queue_of(vec![first, second]);

    queue.answer_duplicate_reads(
        &macs(&[JBL]),
        &Err(RouterError::Audio(AudioError::PipeWire(
            "sinks unreadable".to_string(),
        ))),
    );

    for answer in [&mut first_answer, &mut second_answer] {
        let got = answer.try_recv();
        assert!(
            matches!(
                &got,
                Ok(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m == "sinks unreadable"
            ),
            "got {got:?}"
        );
    }
    assert!(queue.is_empty());
}

// Criterion (guard, a read is answered only by a read for the same
// speakers): after `SinkVolumes [JBL]` ran, a queued `SinkVolumes
// [JBL, SONY]` is not handed its one-element answer — deduplicating on
// the variant alone would — and after `SinkVolumes [JBL, SONY]` ran, a
// queued `SinkVolumes [JBL]` is not handed two levels. The same two
// speakers in the other order are another read too: the answer is
// positional. Each stays queued, unanswered, to run on its own.
#[test]
fn test_answer_duplicate_reads_leaves_a_read_for_other_speakers_in_the_queue() {
    let base = Instant::now();
    let (wider, mut wider_answer) = read(&[JBL, SONY], None);
    let mut queue = queue_of(vec![wider]);
    queue.answer_duplicate_reads(&macs(&[JBL]), &Ok(vec![Some(0.4)]));
    assert_eq!(held(&mut wider_answer), Held::Nothing);
    assert_eq!(queue.len(), 1);

    let (narrower, mut narrower_answer) = read(&[JBL], None);
    let (swapped, mut swapped_answer) = read(&[SONY, JBL], None);
    let mut queue = queue_of(vec![narrower, swapped]);
    queue.answer_duplicate_reads(&macs(&[JBL, SONY]), &Ok(vec![Some(0.4), Some(0.7)]));
    assert_eq!(held(&mut narrower_answer), Held::Nothing);
    assert_eq!(held(&mut swapped_answer), Held::Nothing);
    assert_eq!(
        described(&take_all(&mut queue, base, 0)),
        vec![
            format!("SinkVolumes [{JBL}]"),
            format!("SinkVolumes [{SONY},{JBL}]"),
        ]
    );
}

// Criterion (guard, the empty value): an empty read coalesces with
// nothing but itself. After `SinkVolumes [JBL]` ran, a queued
// `SinkVolumes []` is left alone — a prefix-style comparison would match
// it, the empty list opening every list. After `SinkVolumes []` ran, the
// queued `SinkVolumes [JBL]` is left alone and the queued `SinkVolumes
// []` receives the empty answer.
#[test]
fn test_answer_duplicate_reads_of_an_empty_read_answers_only_another_empty_read() {
    let (empty, mut empty_answer) = read(&[], None);
    let mut queue = queue_of(vec![empty]);
    queue.answer_duplicate_reads(&macs(&[JBL]), &Ok(vec![Some(0.4)]));
    assert_eq!(held(&mut empty_answer), Held::Nothing);
    assert_eq!(queue.len(), 1);

    let (jbl, mut jbl_answer) = read(&[JBL], None);
    let (empty, mut empty_answer) = read(&[], None);
    let mut queue = queue_of(vec![jbl, empty]);
    queue.answer_duplicate_reads(&[], &Ok(Vec::new()));
    assert_eq!(held(&mut jbl_answer), Held::Nothing);
    assert_eq!(held(&mut empty_answer), Held::Other("Ok([])".to_string()));
    assert_eq!(queue.len(), 1);
}

// Criterion: routing messages — and volume sets — are never answered by
// another message's result. One of each kind that is not a read, every
// one naming the JBL the read was for, is left in the queue unanswered,
// in order.
#[test]
fn test_answer_duplicate_reads_never_answers_a_message_that_is_not_a_read() {
    let base = Instant::now();
    let jbl = [target(JBL, 0)];
    let (a_route, mut route_answer) = route(&jbl, None);
    let (a_set, mut set_answer) = set(&[JBL], 0.5, None);
    let (a_retune, mut retune_answer) = retune(JBL, 120, None);
    let (a_spotify, mut spotify_answer) = spotify(&jbl, None);
    let (an_apply, mut apply_answer) = apply(&jbl, 0);
    let (a_repair, mut repair_answer) = repair(&jbl, None);
    let mut queue = queue_of(vec![
        a_route, a_set, a_retune, a_spotify, an_apply, a_repair,
    ]);

    queue.answer_duplicate_reads(&macs(&[JBL]), &Ok(vec![Some(0.4)]));

    assert_eq!(held(&mut route_answer), Held::Nothing);
    assert_eq!(held(&mut set_answer), Held::Nothing);
    assert_eq!(held(&mut retune_answer), Held::Nothing);
    assert_eq!(held(&mut spotify_answer), Held::Nothing);
    assert_eq!(held(&mut apply_answer), Held::Nothing);
    assert_eq!(held(&mut repair_answer), Held::Nothing);
    assert_eq!(
        described(&take_all(&mut queue, base, 0)),
        vec![
            format!("Route [{JBL}@0]"),
            format!("SetSinkVolumes [{JBL}] 0.5"),
            format!("Retune {COMBINED} bluez_output.2C_FD_B4_D3_AC_21 120"),
            format!("RouteForSpotify [{JBL}@0]"),
            format!("ApplySelection [{JBL}@0] 0"),
            format!("Repair [{JBL}@0]"),
        ]
    );
}

// ─── The actor: router + queue, over `FakeGraph` ────────────────────────

/// An actor over `fake` whose router reads a clock the test moves with
/// [`advance`], the [`Shared`] it publishes into, and that clock.
fn actor_on(fake: &FakeGraph) -> (Actor, Shared, Arc<Mutex<Instant>>) {
    let shared = Shared::new();
    let now = Arc::new(Mutex::new(Instant::now()));
    // Cloned: the actor owns one handle onto what it shares, the test
    // keeps the other.
    let actor = actor_sharing(fake, &shared, &now);
    (actor, shared, now)
}

/// An actor over `fake`, with a router of its own, publishing into
/// `shared` and reading `clock`.
fn actor_sharing(fake: &FakeGraph, shared: &Shared, clock: &Arc<Mutex<Instant>>) -> Actor {
    let clock = Arc::clone(clock);
    let router = AudioRouter::with_clock(
        // A clone of the fake is a handle onto the same state.
        Box::new(fake.clone()),
        Box::new(move || *clock.lock().unwrap()),
    );
    // Cloned: every actor started for a handle shares the same state.
    Actor::new(router, shared.clone())
}

/// Move a test router's clock forward by `by`.
fn advance(clock: &Arc<Mutex<Instant>>, by: Duration) {
    *clock.lock().unwrap() += by;
}

/// Queue `envelope` alone and run it at `now`; whether a message ran.
fn run_alone(actor: &mut Actor, envelope: Envelope, now: Instant) -> bool {
    let queue = RefCell::new(queue_of(vec![envelope]));
    let ran = actor.run_next(&queue, now);
    assert!(queue.borrow().is_empty(), "the message left the queue");
    ran
}

/// The answer a message's reply holds, once it ran.
fn answered<T>(answer: &mut Answer<T>) -> Option<Result<T, RouterError>> {
    answer.try_recv().ok()
}

fn clear_stale() -> GraphCall {
    GraphCall::ClearStaleDefaultSink {
        sink_name: COMBINED.to_string(),
    }
}

fn teardown() -> GraphCall {
    GraphCall::Teardown {
        sink_name: COMBINED.to_string(),
    }
}

fn create() -> GraphCall {
    GraphCall::CreateCombinedSink {
        sink_name: COMBINED.to_string(),
    }
}

fn load(real_sink: &str, latency_ms: u32) -> GraphCall {
    GraphCall::LoadBranch {
        sink_name: COMBINED.to_string(),
        real_sink: real_sink.to_string(),
        latency_ms,
    }
}

fn set_volume(sink: &str, level: f32) -> GraphCall {
    GraphCall::SetSinkVolume {
        sink: sink.to_string(),
        level,
    }
}

fn volume_of(sink: &str) -> GraphCall {
    GraphCall::SinkVolume {
        sink: sink.to_string(),
    }
}

/// How many routing passes read the combined sink's branches.
fn passes(fake: &FakeGraph) -> usize {
    fake.all_calls()
        .iter()
        .filter(|call| matches!(call, GraphCall::Branches { .. }))
        .count()
}

/// A graph with both speakers' sinks, the combined sink up and one live
/// branch into each speaker: a route to both changes nothing. Returns the
/// ids of the JBL's and the Sony's branch.
fn steady_graph() -> (FakeGraph, u32, u32) {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED]);
    let jbl = fake.seed_branch(COMBINED, JBL_SINK, 0, Some(true));
    let sony = fake.seed_branch(COMBINED, SONY_SINK, 0, Some(true));
    (fake, jbl, sony)
}

// Criterion: a route message is `route_for_targets` on its speakers —
// here a build from nothing, each speaker's branch at its own offset,
// given distinct values so a swap shows — and answers its result.
#[test]
fn test_a_route_message_routes_the_graph_to_its_speakers() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = route(&[target(JBL, 40), target(SONY, 70)], None);

    let ran = run_alone(&mut actor, envelope, Instant::now());

    assert!(ran);
    assert!(matches!(answered(&mut answer), Some(Ok(()))));
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(),
            teardown(),
            create(),
            load(JBL_SINK, 40),
            load(SONY_SINK, 70),
        ]
    );
}

// Criterion: a route message answers what the router answered — for no
// speaker at all, its refusal, with the graph not even read.
#[test]
fn test_a_route_message_for_no_speaker_answers_the_router_s_refusal_without_a_graph_call() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = route(&[], None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(
        matches!(
            &got,
            Some(Err(RouterError::Audio(AudioError::NoSpeakerConnected)))
        ),
        "got {got:?}"
    );
    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
}

// Criterion: a read message is `sink_volumes` — each speaker's level, in
// the order asked, over one read of the sink list. The two levels differ
// and the speakers are asked Sony first, so a swap shows.
#[test]
fn test_a_sink_volumes_message_reads_each_speaker_over_one_read_of_the_sink_list() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    fake.set_volume(SONY_SINK, 0.7);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = read(&[SONY, JBL], None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    assert_eq!(
        answered(&mut answer).and_then(Result::ok),
        Some(vec![Some(0.7), Some(0.4)])
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Sinks, volume_of(SONY_SINK), volume_of(JBL_SINK)]
    );
}

// Criterion: a set message is `set_sink_volume` per speaker, in order,
// at the message's level, stopping at the first failure. The near miss
// of "stopping" is the Sony, listed and settable behind the JBL whose
// set fails: a loop that carries on writes its level.
#[test]
fn test_a_set_sink_volumes_message_sets_each_speaker_and_stops_at_the_first_failure() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = set(&[SONY, JBL], 0.45, None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    assert!(matches!(answered(&mut answer), Some(Ok(()))));
    assert_eq!(
        fake.calls(),
        vec![set_volume(SONY_SINK, 0.45), set_volume(JBL_SINK, 0.45)]
    );

    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.fail_for(GraphOp::SetSinkVolume, JBL_SINK);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = set(&[JBL, SONY], 0.6, None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(
        matches!(
            &got,
            Some(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m.contains("SetSinkVolume told to fail")
        ),
        "got {got:?}"
    );
    assert_eq!(
        fake.calls(),
        vec![set_volume(JBL_SINK, 0.6)],
        "the Sony is not set once the JBL's set failed"
    );
}

// Criterion: an apply-selection message tears the combined sink down on
// an empty selection — the near miss is routing it, which the router
// refuses and which tears nothing down — and is `route_for_targets`
// otherwise.
#[test]
fn test_an_apply_selection_message_tears_down_on_an_empty_selection_and_routes_otherwise() {
    let (fake, _, _) = steady_graph();
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = apply(&[], 0);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(matches!(&got, Some(Ok(()))), "got {got:?}");
    assert_eq!(fake.calls(), vec![teardown()]);

    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = apply(&[target(JBL, 40)], 0);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    assert!(matches!(answered(&mut answer), Some(Ok(()))));
    assert_eq!(
        fake.routing_calls(),
        vec![clear_stale(), teardown(), create(), load(JBL_SINK, 40)]
    );
}

/// Run one repair of the JBL on `actor` and return its outcome.
fn repair_the_jbl(actor: &mut Actor) -> Option<RepairOutcome> {
    let (envelope, mut answer) = repair(&[target(JBL, 0)], None);
    assert!(run_alone(actor, envelope, Instant::now()));
    answered(&mut answer).and_then(Result::ok)
}

// Criterion: a repair message is `route_for_targets`, plus whether the
// graph changed and whether the last re-target failed — all three in its
// one answer. A build changes the graph; the same repair again, on the
// graph it left, does not: the near miss is an answer read off the
// router's running count, which is no longer zero.
#[test]
fn test_a_repair_message_answers_whether_it_routed_and_whether_it_changed_the_graph() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);

    let built = repair_the_jbl(&mut actor);
    assert!(
        matches!(
            &built,
            Some(RepairOutcome {
                routed: Ok(()),
                changed: true,
                retarget_failed: false
            })
        ),
        "got {built:?}"
    );
    assert_eq!(
        fake.routing_calls(),
        vec![clear_stale(), teardown(), create(), load(JBL_SINK, 0)]
    );

    fake.clear_calls();
    let steady = repair_the_jbl(&mut actor);
    assert!(
        matches!(
            &steady,
            Some(RepairOutcome {
                routed: Ok(()),
                changed: false,
                retarget_failed: false
            })
        ),
        "got {steady:?}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    assert_eq!(passes(&fake), 1, "the second repair still reads the graph");
}

// Criterion: a repair message reports a failed re-target (#139) — the
// build went through, the streams were not moved back — and a route that
// failed, as two different answers. Neither is told from a healthy build
// by `routed` alone, nor from the other by `changed` alone.
#[test]
fn test_a_repair_message_answers_a_failed_retarget_apart_from_a_failed_route() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail(GraphOp::RetargetStreams);
    let (mut actor, _, _) = actor_on(&fake);

    let not_retargeted = repair_the_jbl(&mut actor);
    assert!(
        matches!(
            &not_retargeted,
            Some(RepairOutcome {
                routed: Ok(()),
                changed: true,
                retarget_failed: true
            })
        ),
        "got {not_retargeted:?}"
    );

    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail(GraphOp::CreateCombinedSink);
    let (mut actor, _, _) = actor_on(&fake);

    let not_routed = repair_the_jbl(&mut actor);
    assert!(
        matches!(
            &not_routed,
            Some(RepairOutcome {
                routed: Err(AudioError::PipeWire(m)),
                changed: false,
                retarget_failed: false
            }) if m.contains("CreateCombinedSink told to fail")
        ),
        "got {not_routed:?}"
    );
}

// Criterion: a route-for-Spotify message is `route_for_targets` then
// `resolve_target_sink`, and answers the resolved node name. The fake
// resolves the combined sink to its own name, so the resolution is read
// off the log: the sink list is read once more after the last branch
// load. An answer made up from the logical target ends on that load.
#[test]
fn test_a_route_for_spotify_message_routes_then_resolves_the_node_name() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = spotify(&[target(JBL, 40)], None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    assert_eq!(
        answered(&mut answer).and_then(Result::ok),
        Some(COMBINED.to_string())
    );
    assert_eq!(
        fake.routing_calls(),
        vec![clear_stale(), teardown(), create(), load(JBL_SINK, 40)]
    );
    let all = fake.all_calls();
    assert_eq!(
        all.last(),
        Some(&GraphCall::Sinks),
        "the target is resolved after the route: {all:?}"
    );
}

// Criterion (non-nominal): a route-for-Spotify whose routing failed
// answers that failure and resolves nothing — the log ends on the failed
// call; one whose resolution failed answers the resolution's error, not
// a name: `librespot` is never pointed at a sink that was not found.
#[test]
fn test_a_route_for_spotify_message_answers_a_failed_route_or_a_failed_resolution() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail(GraphOp::CreateCombinedSink);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = spotify(&[target(JBL, 0)], None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(
        matches!(
            &got,
            Some(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m.contains("CreateCombinedSink told to fail")
        ),
        "got {got:?}"
    );
    assert_eq!(fake.all_calls().last(), Some(&create()));

    // The route reads the sink list twice — is the combined sink up, and
    // which node is the JBL's — and the resolution is the third read.
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail_after(GraphOp::Sinks, 2);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = spotify(&[target(JBL, 0)], None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(
        matches!(
            &got,
            Some(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m.contains("no PipeWire sink for target blue2th_combined")
        ),
        "got {got:?}"
    );
    assert_eq!(fake.calls().last(), Some(&load(JBL_SINK, 0)));
}

// Criterion (guard, unreadable is not absent): a retune reads the sink
// list first, and a list that cannot be read answers that error — never
// `Ok(())` as if there were no combined sink — with no other graph call.
// The near miss is the next test: a list that reads fine and does not
// hold the combined sink.
#[test]
fn test_a_retune_message_over_an_unreadable_sink_list_answers_the_error_and_asks_nothing_more() {
    let (fake, _, _) = steady_graph();
    fake.fail(GraphOp::Sinks);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = retune(SONY, 120, None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(
        matches!(
            &got,
            Some(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m.contains("Sinks told to fail")
        ),
        "got {got:?}"
    );
    assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
}

// Criterion (guard, unreadable is not absent — the other side): a sink
// list that reads fine and holds no combined sink is "nothing to
// retune": `Ok(())`, the list read once and no graph write. A namesake
// sharing the combined sink's opening characters is not the combined
// sink.
#[test]
fn test_a_retune_message_without_the_combined_sink_answers_ok_and_writes_nothing() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, "blue2th_combined_old"]);
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = retune(SONY, 120, None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    let got = answered(&mut answer);
    assert!(matches!(&got, Some(Ok(()))), "got {got:?}");
    assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
}

// Criterion: with the combined sink present a retune message is
// `retune_branch` — the delay set in place on that speaker's branch, at
// the message's latency, and nothing loaded or unloaded.
#[test]
fn test_a_retune_message_with_the_combined_sink_retunes_that_branch_in_place() {
    let (fake, jbl, sony) = steady_graph();
    let (mut actor, _, _) = actor_on(&fake);
    let (envelope, mut answer) = retune(SONY, 120, None);

    assert!(run_alone(&mut actor, envelope, Instant::now()));

    assert!(matches!(answered(&mut answer), Some(Ok(()))));
    assert_eq!(
        fake.calls(),
        vec![GraphCall::SetBranchDelay {
            id: sony,
            delay_ms: 120
        }]
    );
    let delays: Vec<(u32, u32)> = fake
        .loaded(COMBINED)
        .iter()
        .map(|l| (l.id, l.branch.latency_ms))
        .collect();
    assert_eq!(delays, vec![(jbl, 0), (sony, 120)]);
}

// Criterion (guard, exactly one deadline per message): the graph is
// handed one deadline per message, 1600 ms after the message started,
// however many graph calls the message makes. The near miss is the first
// message: a route that builds the combined sink from nothing for two
// speakers, at least five graph calls — a deadline set per call passes
// on a one-call message. The second message, started two seconds later,
// gets its own: a deadline set once for the actor's life fails there. A
// message that is not run hands the graph none.
#[test]
fn test_the_graph_is_handed_one_deadline_per_message_1600_ms_after_the_message_started() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    let (mut actor, _, _) = actor_on(&fake);
    assert_eq!(fake.deadlines(), Vec::<Instant>::new());
    let started = Instant::now() + Duration::from_secs(7);

    let (build, mut build_answer) = route(&[target(JBL, 0), target(SONY, 70)], None);
    assert!(run_alone(&mut actor, build, started));

    assert!(matches!(answered(&mut build_answer), Some(Ok(()))));
    assert!(
        fake.all_calls().len() >= 5,
        "the build is a many-call message: {:?}",
        fake.all_calls()
    );
    assert_eq!(
        fake.deadlines(),
        vec![started + Duration::from_millis(1600)]
    );

    let (a_read, _read_answer) = read(&[JBL, SONY], None);
    assert!(run_alone(
        &mut actor,
        a_read,
        started + Duration::from_secs(2)
    ));
    assert_eq!(
        fake.deadlines(),
        vec![
            started + Duration::from_millis(1600),
            started + Duration::from_millis(3600),
        ]
    );

    let (left, left_answer) = route(&[target(JBL, 0)], None);
    drop(left_answer);
    assert!(!run_alone(
        &mut actor,
        left,
        started + Duration::from_secs(4)
    ));
    assert_eq!(
        fake.deadlines().len(),
        2,
        "a message not run has no deadline"
    );
}

// Criterion: a freshly started actor publishes `None` — over whatever
// was published before it. The near miss is the due time left in place
// by an earlier actor, which a constructor that publishes nothing keeps.
#[test]
fn test_a_freshly_started_actor_publishes_no_confirmation_due_time() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let shared = Shared::new();
    let published = shared.confirmation_due();
    assert_eq!(*published.borrow(), None, "nothing is due before any actor");
    let clock = Arc::new(Mutex::new(Instant::now()));
    shared.publish_confirmation_due(Some(Instant::now() + Duration::from_secs(5)));
    assert!(published.borrow().is_some());

    let _actor = actor_sharing(&fake, &shared, &clock);

    assert_eq!(*published.borrow(), None);
}

// Criterion: after every message the actor publishes the router's
// earliest confirmation due time. Followed over four messages on the
// router's own clock: a build arms the JBL (due five seconds on); a
// route adding the Sony a second later arms it too, and the earliest
// stays the JBL's; a repair at that instant takes the JBL's reload and
// leaves the Sony's, due one second on; a repair then takes it, and
// nothing is due. A publication made only when a load arms something
// misses the last two.
#[test]
fn test_the_actor_publishes_the_earliest_confirmation_due_time_after_every_message() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    let (mut actor, shared, clock) = actor_on(&fake);
    let published = shared.confirmation_due();
    let t0 = *clock.lock().unwrap();
    let both = [target(JBL, 0), target(SONY, 0)];
    // The instant a message starts is not the router's clock: the due
    // time follows the clock.
    let started = Instant::now() + Duration::from_secs(3600);

    let (build, _build_answer) = route(&[target(JBL, 0)], None);
    assert!(run_alone(&mut actor, build, started));
    assert_eq!(*published.borrow(), Some(t0 + Duration::from_secs(5)));

    advance(&clock, Duration::from_secs(1));
    let (add, _add_answer) = route(&both, None);
    assert!(run_alone(&mut actor, add, started));
    assert_eq!(
        *published.borrow(),
        Some(t0 + Duration::from_secs(5)),
        "the earliest of the two"
    );

    let (a_read, _read_answer) = read(&[JBL], None);
    assert!(run_alone(&mut actor, a_read, started));
    assert_eq!(*published.borrow(), Some(t0 + Duration::from_secs(5)));

    advance(&clock, Duration::from_secs(4));
    let (confirm_jbl, _confirm_jbl_answer) = repair(&both, None);
    assert!(run_alone(&mut actor, confirm_jbl, started));
    assert_eq!(
        *published.borrow(),
        Some(t0 + Duration::from_secs(6)),
        "the JBL's reload was taken; the Sony's is left"
    );

    advance(&clock, Duration::from_secs(1));
    let (confirm_sony, _confirm_sony_answer) = repair(&both, None);
    assert!(run_alone(&mut actor, confirm_sony, started));
    assert_eq!(*published.borrow(), None);
    assert_eq!(CONFIRM_GAP, Duration::from_secs(5));
}

// Criterion: a new actor starts from a new router — a confirmation armed
// in the previous one is not carried over. The first actor builds, which
// arms the JBL's reload; a second actor is started over the same graph
// and the same shared state, as a replacement thread is. Nothing is due
// any more, and a repair at the instant the reload was due reloads
// nothing.
#[test]
fn test_a_new_actor_over_the_same_graph_carries_no_confirmation_over() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut first, shared, clock) = actor_on(&fake);
    let published = shared.confirmation_due();
    let (build, _build_answer) = route(&[target(JBL, 0)], None);
    assert!(run_alone(&mut first, build, Instant::now()));
    assert!(published.borrow().is_some(), "the build armed a reload");
    drop(first);

    let mut second = actor_sharing(&fake, &shared, &clock);
    assert_eq!(*published.borrow(), None);

    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let outcome = repair_the_jbl(&mut second);
    assert!(
        matches!(
            &outcome,
            Some(RepairOutcome {
                routed: Ok(()),
                changed: false,
                retarget_failed: false
            })
        ),
        "got {outcome:?}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    assert_eq!(*published.borrow(), None);
}

// Criterion (guard, a closed reply is never run): a message whose caller
// left is skipped without reaching the graph — the call log is read,
// since "no answer was received" is just as true of a message that ran.
// The near miss is the same message with its reply open, which runs.
#[test]
fn test_a_message_whose_reply_is_closed_never_reaches_the_graph() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (left, left_answer) = set(&[JBL], 0.8, None);
    drop(left_answer);

    assert!(!run_alone(&mut actor, left, Instant::now()));

    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    assert_eq!(fake.deadlines(), Vec::<Instant>::new());

    let (waiting, mut waiting_answer) = set(&[JBL], 0.8, None);
    assert!(run_alone(&mut actor, waiting, Instant::now()));
    assert!(matches!(answered(&mut waiting_answer), Some(Ok(()))));
    assert_eq!(fake.calls(), vec![set_volume(JBL_SINK, 0.8)]);
}

// Criterion: a request taken out past its `start_by` never reaches the
// graph and answers `Expired`; the actor goes on, in the same step, to
// the message behind it. An expired read at the head is not rescued by
// the identical read behind it: the checks run on the head, and that
// read runs on its own.
#[test]
fn test_an_expired_message_never_reaches_the_graph_and_the_actor_runs_the_one_behind_it() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    let (mut actor, _, _) = actor_on(&fake);
    let base = Instant::now();
    let (late_set, mut late_set_answer) = set(&[JBL], 0.8, Some(base));
    let (late_read, mut late_read_answer) = read(&[JBL], Some(base));
    let (in_time, mut in_time_answer) = read(&[JBL], Some(base + Duration::from_secs(1)));
    let queue = RefCell::new(queue_of(vec![late_set, late_read, in_time]));

    let ran = actor.run_next(&queue, base + Duration::from_millis(500));

    assert!(ran);
    assert_eq!(held(&mut late_set_answer), Held::Expired);
    assert_eq!(held(&mut late_read_answer), Held::Expired);
    assert_eq!(
        answered(&mut in_time_answer).and_then(Result::ok),
        Some(vec![Some(0.4)])
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Sinks, volume_of(JBL_SINK)],
        "the set was never written, and the list was read once"
    );
    assert!(!actor.run_next(&queue, base + Duration::from_millis(500)));
}

// Criterion: several identical volume reads queued — the read runs once
// and every waiting caller gets that answer, the one whose own start
// deadline had passed included. The read for both speakers is another
// read: it runs on its own, in the next step.
#[test]
fn test_queued_duplicates_of_a_read_are_answered_by_the_one_read_that_ran() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    fake.set_volume(SONY_SINK, 0.7);
    let (mut actor, _, _) = actor_on(&fake);
    let base = Instant::now();
    let now = base + Duration::from_millis(500);
    let (head, mut head_answer) = read(&[JBL], Some(base + Duration::from_secs(1)));
    let (overdue, mut overdue_answer) = read(&[JBL], Some(base));
    let (wider, mut wider_answer) = read(&[JBL, SONY], Some(base + Duration::from_secs(1)));
    let (background, mut background_answer) = read(&[JBL], None);
    let queue = RefCell::new(queue_of(vec![head, overdue, wider, background]));

    assert!(actor.run_next(&queue, now));

    for answer in [
        &mut head_answer,
        &mut overdue_answer,
        &mut background_answer,
    ] {
        assert_eq!(answered(answer).and_then(Result::ok), Some(vec![Some(0.4)]));
    }
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Sinks, volume_of(JBL_SINK)],
        "one read of the graph for three callers"
    );
    assert_eq!(held(&mut wider_answer), Held::Nothing);
    assert_eq!(queue.borrow().len(), 1);

    fake.clear_calls();
    assert!(actor.run_next(&queue, now));
    assert_eq!(
        answered(&mut wider_answer).and_then(Result::ok),
        Some(vec![Some(0.4), Some(0.7)])
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Sinks, volume_of(JBL_SINK), volume_of(SONY_SINK)]
    );
    assert!(!actor.run_next(&queue, now));
}

// Criterion: a superseded set is not applied — the graph receives the
// winner's level only — and answers `Superseded`, not success.
#[test]
fn test_a_superseded_set_never_reaches_the_graph_and_does_not_answer_success() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (older, mut older_answer) = set(&[JBL], 0.3, None);
    let (newer, mut newer_answer) = set(&[JBL], 0.5, None);
    let queue = RefCell::new(queue_of(vec![older, newer]));

    assert!(actor.run_next(&queue, Instant::now()));

    assert_eq!(held(&mut older_answer), Held::Superseded);
    assert!(matches!(answered(&mut newer_answer), Some(Ok(()))));
    assert_eq!(fake.calls(), vec![set_volume(JBL_SINK, 0.5)]);
    assert!(!actor.run_next(&queue, Instant::now()));
}

// Criterion (guard, the later set covers only part): `Set [JBL, SONY]
// 0.3` then `Set [JBL] 0.5` — the first runs whole, then the second, so
// the Sony is not left at its old level.
#[test]
fn test_a_set_a_later_one_covers_in_part_reaches_the_graph_whole_then_the_later_one() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (both, mut both_answer) = set(&[JBL, SONY], 0.3, None);
    let (one, mut one_answer) = set(&[JBL], 0.5, None);
    let queue = RefCell::new(queue_of(vec![both, one]));

    assert!(actor.run_next(&queue, Instant::now()));
    assert!(actor.run_next(&queue, Instant::now()));

    assert!(matches!(answered(&mut both_answer), Some(Ok(()))));
    assert!(matches!(answered(&mut one_answer), Some(Ok(()))));
    assert_eq!(
        fake.calls(),
        vec![
            set_volume(JBL_SINK, 0.3),
            set_volume(SONY_SINK, 0.3),
            set_volume(JBL_SINK, 0.5),
        ]
    );
}

// Criterion (guard, routing messages are never coalesced): two `Route`
// messages with the same speakers, queued together. Both reach the
// graph — two passes over the branches — and each gets its own answer.
#[test]
fn test_two_identical_route_messages_both_reach_the_graph() {
    let (fake, _, _) = steady_graph();
    let (mut actor, _, _) = actor_on(&fake);
    let speakers = [target(JBL, 0), target(SONY, 0)];
    let (first, mut first_answer) = route(&speakers, None);
    let (second, mut second_answer) = route(&speakers, None);
    let queue = RefCell::new(queue_of(vec![first, second]));

    assert!(actor.run_next(&queue, Instant::now()));
    assert_eq!(passes(&fake), 1, "one message per step");
    assert!(matches!(answered(&mut first_answer), Some(Ok(()))));
    assert_eq!(held(&mut second_answer), Held::Nothing);

    assert!(actor.run_next(&queue, Instant::now()));
    assert_eq!(passes(&fake), 2);
    assert!(matches!(answered(&mut second_answer), Some(Ok(()))));
}

// Criterion (guard, the latest selection, once): an apply-selection
// stamped before the routing generation moved never reaches the graph —
// here the Sony's branch is never loaded — and answers that it is
// outdated. The one stamped with the current generation, sent next,
// runs.
#[test]
fn test_an_outdated_apply_selection_never_reaches_the_graph() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED]);
    fake.seed_branch(COMBINED, JBL_SINK, 0, Some(true));
    let (mut actor, shared, _) = actor_on(&fake);
    let (stale, mut stale_answer) = apply(&[target(JBL, 0), target(SONY, 0)], shared.generation());
    assert_eq!(shared.advance_generation(), 1);

    assert!(!run_alone(&mut actor, stale, Instant::now()));

    assert_eq!(held(&mut stale_answer), Held::Outdated);
    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    assert_eq!(fake.deadlines(), Vec::<Instant>::new());

    let (current, mut current_answer) = apply(&[target(JBL, 0)], shared.generation());
    assert!(run_alone(&mut actor, current, Instant::now()));
    assert!(matches!(answered(&mut current_answer), Some(Ok(()))));
    assert_eq!(passes(&fake), 1);
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion (guard, the empty value): an empty speaker list reaches the
// router as it does today — `sink_volumes(&[])` is `Ok(vec![])` and a
// set of no speaker is `Ok(())`, neither with a graph call — and the
// empty set is not superseded by the JBL's set queued behind it, which
// runs too.
#[test]
fn test_empty_speaker_lists_reach_the_router_without_a_graph_call() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let (empty_read, mut empty_read_answer) = read(&[], None);
    let (empty_set, mut empty_set_answer) = set(&[], 0.3, None);
    let queue = RefCell::new(queue_of(vec![empty_read, empty_set]));

    assert!(actor.run_next(&queue, Instant::now()));
    assert!(actor.run_next(&queue, Instant::now()));

    assert_eq!(
        answered(&mut empty_read_answer).and_then(Result::ok),
        Some(Vec::new())
    );
    assert!(matches!(answered(&mut empty_set_answer), Some(Ok(()))));
    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

    let (empty_set, mut empty_set_answer) = set(&[], 0.3, None);
    let (jbl_set, mut jbl_set_answer) = set(&[JBL], 0.5, None);
    let queue = RefCell::new(queue_of(vec![empty_set, jbl_set]));
    assert!(actor.run_next(&queue, Instant::now()));
    assert!(actor.run_next(&queue, Instant::now()));
    assert!(matches!(answered(&mut empty_set_answer), Some(Ok(()))));
    assert!(matches!(answered(&mut jbl_set_answer), Some(Ok(()))));
    assert_eq!(fake.calls(), vec![set_volume(JBL_SINK, 0.5)]);
}

thread_local! {
/// The queue of the test below, reachable from the hook the fake
/// graph runs in the middle of a message — as the loop thread's
/// inbox is from its channel callback.
static INBOX: RefCell<Queue> = RefCell::new(Queue::new());
}

// Constraint (#146, kept): a message enters the queue while the loop
// iterates, and a running message iterates it — so the queue is not
// borrowed while a message runs, and a message queued meanwhile is taken
// out by the next step. The near miss is an actor that keeps the queue
// borrowed across the run: the push below, made from inside the running
// message's first graph call, trips that borrow.
#[test]
fn test_a_message_queued_while_another_runs_is_taken_out_by_the_next_step() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    let (mut actor, _, _) = actor_on(&fake);
    let now = Instant::now();
    let (running, mut running_answer) = read(&[JBL], None);
    let (arriving, mut arriving_answer) = set(&[JBL], 0.5, None);
    INBOX.with(|inbox| inbox.borrow_mut().push(running));
    fake.run_on_next_sinks_read(move || {
        // As the channel callback does while a message iterates the loop.
        INBOX.with(|inbox| inbox.borrow_mut().push(arriving));
    });

    let ran = INBOX.with(|inbox| actor.run_next(inbox, now));

    assert!(ran);
    assert_eq!(
        answered(&mut running_answer).and_then(Result::ok),
        Some(vec![Some(0.4)])
    );
    assert_eq!(held(&mut arriving_answer), Held::Nothing);
    assert_eq!(INBOX.with(|inbox| inbox.borrow().len()), 1);

    let ran = INBOX.with(|inbox| actor.run_next(inbox, now));

    assert!(ran, "the message that arrived meanwhile runs next");
    assert!(matches!(answered(&mut arriving_answer), Some(Ok(()))));
    assert_eq!(fake.calls(), vec![set_volume(JBL_SINK, 0.5)]);
    assert!(!INBOX.with(|inbox| actor.run_next(inbox, now)));
}

// Edge case: with nothing queued the actor runs nothing, and the graph
// is handed no deadline.
#[test]
fn test_run_next_on_an_empty_queue_runs_nothing() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, _, _) = actor_on(&fake);
    let queue = RefCell::new(Queue::new());

    assert!(!actor.run_next(&queue, Instant::now()));

    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    assert_eq!(fake.deadlines(), Vec::<Instant>::new());
}

// ─── #152: a routing message lost to a stall owes one re-apply ──────────

/// What the routing applier sees of a [`Shared`]: the routing generation
/// and its wake-up receiver, taken before anything ran.
struct Applier {
    shared: Shared,
    generation: u64,
    wakes: watch::Receiver<()>,
}

/// What the applier saw over one step: how far the routing generation
/// moved, and whether its receiver woke — `None` for a receiver whose
/// sender is gone, which no request can ever wake.
type Seen = (u64, Option<bool>);

/// Nothing published: the generation stands, the applier sleeps on.
const NOTHING: Seen = (0, Some(false));
/// One routing request published: the generation moved by one, and the
/// applier woke.
const ONE_REQUEST: Seen = (1, Some(true));

impl Applier {
    fn on(shared: &Shared) -> Self {
        Self {
            // Cloned: the test reads what the actor publishes into.
            shared: shared.clone(),
            generation: shared.generation(),
            wakes: shared.routing_requests(),
        }
    }

    /// What was published since the last look; marks it seen.
    fn look(&mut self) -> Seen {
        let generation = self.shared.generation();
        let moved = generation - self.generation;
        self.generation = generation;
        let woke = self.wakes.has_changed().ok();
        self.wakes.borrow_and_update();
        (moved, woke)
    }
}

/// What one message did to the debt.
#[derive(Debug)]
struct Lost {
    /// Whether the message ran at all.
    ran: bool,
    /// What its reply held.
    answer: Held,
    /// What running it published, before the graph answered again.
    by_the_run: Seen,
    /// What the first "the graph answers again" published.
    first_thaw: Seen,
    /// What a second one, right behind it, published.
    second_thaw: Seen,
}

/// Run the message `make` builds — handed the current routing
/// generation, for an apply-selection to be stamped with — alone at `at`,
/// then tell `actor` twice that the graph answers again.
fn lose_then_thaw<T: std::fmt::Debug>(
    actor: &mut Actor,
    shared: &Shared,
    at: Instant,
    make: impl FnOnce(u64) -> (Envelope, Answer<T>),
) -> Lost {
    let mut applier = Applier::on(shared);
    let (envelope, mut answer) = make(shared.generation());
    let ran = run_alone(actor, envelope, at);
    let answer = held(&mut answer);
    let by_the_run = applier.look();
    actor.graph_answers_again();
    let first_thaw = applier.look();
    actor.graph_answers_again();
    let second_thaw = applier.look();
    Lost {
        ran,
        answer,
        by_the_run,
        first_thaw,
        second_thaw,
    }
}

/// Whether a reply held an `Unanswered` — a request's own error, or a
/// repair's `routed`.
fn unanswered(answer: &Held) -> bool {
    matches!(answer, Held::Other(text) if text.contains("Unanswered"))
}

// Criterion (#152): every routing message — `Route`, `RouteForSpotify`,
// `ApplySelection` (a selection, and the last deselect that tears the
// combined sink down), `Retune`, `Repair` (its `routed` being
// `Err(Unanswered)`) — that ends `Unanswered` leaves a re-apply owed.
// Running it publishes nothing (no busy loop: nothing is sent until the
// graph has answered); the first "the graph answers again" publishes
// exactly one routing request; a second one right behind it publishes
// nothing, the debt being cleared. One entry per variant, so a variant
// left out of the routing set fails on its own line. The selection lost
// in the nominal scenario — the reconcile's first sink-list read
// stalling while the Sony is deselected — is among them.
#[test]
fn test_each_routing_message_that_went_unanswered_owes_one_re_apply_paid_when_the_graph_answers_again(
) {
    type Case = (
        &'static str,
        FakeGraph,
        Box<dyn FnOnce(&mut Actor, &Shared) -> Lost>,
    );

    let stalled_reads = || {
        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.fail_unanswered(GraphOp::Sinks);
        fake
    };
    let now = Instant::now();
    let cases: Vec<Case> = vec![
        (
            "Route",
            stalled_reads(),
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |_| route(&[target(JBL, 40)], None))
            }),
        ),
        (
            "RouteForSpotify",
            stalled_reads(),
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |_| spotify(&[target(JBL, 40)], None))
            }),
        ),
        (
            "ApplySelection",
            stalled_reads(),
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |generation| {
                    apply(&[target(JBL, 40)], generation)
                })
            }),
        ),
        (
            "ApplySelection, the reconcile's first read stalled",
            {
                let (fake, _, _) = steady_graph();
                fake.fail_unanswered_after(GraphOp::Sinks, 1);
                fake
            },
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |generation| {
                    apply(&[target(JBL, 0)], generation)
                })
            }),
        ),
        (
            "ApplySelection, empty",
            {
                let (fake, _, _) = steady_graph();
                fake.fail_unanswered(GraphOp::Teardown);
                fake
            },
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |generation| apply(&[], generation))
            }),
        ),
        (
            "Retune",
            {
                let (fake, _, _) = steady_graph();
                fake.fail_unanswered(GraphOp::Sinks);
                fake
            },
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |_| retune(SONY, 120, None))
            }),
        ),
        (
            "Repair",
            stalled_reads(),
            Box::new(move |actor: &mut Actor, shared: &Shared| {
                lose_then_thaw(actor, shared, now, |_| repair(&[target(JBL, 0)], None))
            }),
        ),
    ];

    for (name, fake, lose) in cases {
        let (mut actor, shared, _) = actor_on(&fake);

        let lost = lose(&mut actor, &shared);

        assert!(lost.ran, "{name}: the message ran: {lost:?}");
        assert!(
            unanswered(&lost.answer),
            "{name}: the message ended Unanswered: {lost:?}"
        );
        assert_eq!(lost.by_the_run, NOTHING, "{name}: running it sends nothing");
        assert_eq!(
            lost.first_thaw, ONE_REQUEST,
            "{name}: the graph answering again pays one re-apply"
        );
        assert_eq!(lost.second_thaw, NOTHING, "{name}: the debt is cleared");
    }
}

// Criterion (#152, guard, only routing messages owe): `SinkVolumes` and
// `SetSinkVolumes` ending `Unanswered` leave nothing owed — a volume
// slider that already reported its failure must not change the level
// later, and a read has nothing to re-apply. The near miss is the
// `Unanswered` itself, the same error that makes a routing message owe:
// "every `Unanswered` owes" publishes on the first thaw here. The
// control, on the same actor and the same stalled graph: a route lost
// the same way does owe.
#[test]
fn test_a_volume_message_that_went_unanswered_owes_nothing() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.set_volume(JBL_SINK, 0.4);
    fake.fail_unanswered(GraphOp::Sinks);
    fake.fail_unanswered(GraphOp::SetSinkVolume);
    let (mut actor, shared, _) = actor_on(&fake);
    let now = Instant::now();

    let read_lost = lose_then_thaw(&mut actor, &shared, now, |_| read(&[JBL], None));
    let set_lost = lose_then_thaw(&mut actor, &shared, now, |_| set(&[JBL], 0.45, None));

    assert!(unanswered(&read_lost.answer), "{read_lost:?}");
    assert_eq!(read_lost.first_thaw, NOTHING, "a read owes nothing");
    assert_eq!(read_lost.second_thaw, NOTHING);
    assert!(unanswered(&set_lost.answer), "{set_lost:?}");
    assert_eq!(set_lost.first_thaw, NOTHING, "a volume set owes nothing");
    assert_eq!(set_lost.second_thaw, NOTHING);

    let route_lost = lose_then_thaw(&mut actor, &shared, now, |_| route(&[target(JBL, 0)], None));
    assert!(unanswered(&route_lost.answer), "{route_lost:?}");
    assert_eq!(
        route_lost.first_thaw, ONE_REQUEST,
        "control: a routing message lost the same way owes"
    );
}

// Criterion (#152, guard, only `Unanswered` owes): a routing message
// ending with any other error leaves nothing owed — a `PipeWire(..)`
// refusal (the daemon answered: re-sending gets the same answer), on an
// apply-selection, a route-for-Spotify, a retune and a repair;
// `NoSpeakerConnected`; `Expired` (taken out of the queue past its
// `start_by`, already handed to the applier); and a repair whose
// `routed` is `Ok`. The near miss is the failure: "every failed routing
// message owes" publishes on the first thaw. The control, on the same
// actor: once the reads stall instead of being refused, a route owes.
#[test]
fn test_a_routing_message_that_failed_with_an_answer_owes_nothing() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    fake.fail(GraphOp::Sinks);
    let (mut actor, shared, _) = actor_on(&fake);
    let now = Instant::now();

    let mut outcomes = vec![
        (
            "ApplySelection refused",
            lose_then_thaw(&mut actor, &shared, now, |generation| {
                apply(&[target(JBL, 0)], generation)
            }),
        ),
        (
            "RouteForSpotify refused",
            lose_then_thaw(&mut actor, &shared, now, |_| {
                spotify(&[target(JBL, 0)], None)
            }),
        ),
        (
            "Retune refused",
            lose_then_thaw(&mut actor, &shared, now, |_| retune(SONY, 120, None)),
        ),
        (
            "Repair refused",
            lose_then_thaw(&mut actor, &shared, now, |_| {
                repair(&[target(JBL, 0)], None)
            }),
        ),
        (
            "Route to no speaker",
            lose_then_thaw(&mut actor, &shared, now, |_| route(&[], None)),
        ),
        (
            "Route expired",
            lose_then_thaw(&mut actor, &shared, now + Duration::from_millis(1), |_| {
                route(&[target(JBL, 0)], Some(now))
            }),
        ),
    ];
    fake.clear_failures();
    outcomes.push((
        "Repair routed",
        lose_then_thaw(&mut actor, &shared, now, |_| {
            repair(&[target(JBL, 0)], None)
        }),
    ));

    for (name, lost) in &outcomes {
        assert!(!unanswered(&lost.answer), "{name}: answered: {lost:?}");
        assert!(
            !matches!(lost.answer, Held::Nothing | Held::Dropped),
            "{name}: an answer came back: {lost:?}"
        );
        assert_eq!(lost.first_thaw, NOTHING, "{name}: owes nothing");
        assert_eq!(lost.second_thaw, NOTHING, "{name}");
    }
    let expired = outcomes.iter().find(|(name, _)| *name == "Route expired");
    assert!(
        expired.is_some_and(|(_, lost)| !lost.ran && lost.answer == Held::Expired),
        "the expired route was refused, not run: {expired:?}"
    );

    fake.fail_unanswered(GraphOp::Sinks);
    let route_lost = lose_then_thaw(&mut actor, &shared, now, |_| route(&[target(JBL, 0)], None));
    assert!(unanswered(&route_lost.answer), "{route_lost:?}");
    assert_eq!(
        route_lost.first_thaw, ONE_REQUEST,
        "control: the same route, stalled rather than refused, owes"
    );
}

// Criterion (#152, guard, exactly one re-apply per thaw): a selection,
// then an offset, then a repair pass, all lost to the same freeze, give
// one routing request when the graph answers again — not three — and a
// second "answers again" gives none. The re-apply reads the current
// state, so one covers them all.
#[test]
fn test_several_unanswered_routing_messages_owe_one_re_apply_in_total() {
    let (fake, _, _) = steady_graph();
    fake.fail_unanswered(GraphOp::Sinks);
    let (mut actor, shared, _) = actor_on(&fake);
    let mut applier = Applier::on(&shared);
    let now = Instant::now();

    let (selection, mut selection_answer) = apply(&[target(JBL, 0)], shared.generation());
    let (offset, mut offset_answer) = retune(JBL, 120, None);
    let (pass, mut pass_answer) = repair(&[target(JBL, 120)], None);
    assert!(run_alone(&mut actor, selection, now));
    assert!(run_alone(&mut actor, offset, now));
    assert!(run_alone(&mut actor, pass, now));
    for answer in [
        held(&mut selection_answer),
        held(&mut offset_answer),
        held(&mut pass_answer),
    ] {
        assert!(unanswered(&answer), "each was lost: {answer:?}");
    }
    assert_eq!(applier.look(), NOTHING, "nothing sent during the freeze");

    actor.graph_answers_again();
    assert_eq!(applier.look(), ONE_REQUEST, "one re-apply for the three");

    actor.graph_answers_again();
    assert_eq!(applier.look(), NOTHING, "and only one");
}

// Criterion (#152): a re-apply that goes `Unanswered` itself — the daemon
// froze again before it ran — leaves a re-apply owed again, paid at the
// next thaw. The re-apply is the apply-selection the applier sends,
// stamped with the generation the payment moved to.
#[test]
fn test_a_re_apply_that_went_unanswered_owes_again() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail_unanswered(GraphOp::Sinks);
    let (mut actor, shared, _) = actor_on(&fake);
    let now = Instant::now();

    let first = lose_then_thaw(&mut actor, &shared, now, |_| route(&[target(JBL, 0)], None));
    assert_eq!(first.first_thaw, ONE_REQUEST, "{first:?}");

    let reapply = lose_then_thaw(&mut actor, &shared, now, |generation| {
        apply(&[target(JBL, 0)], generation)
    });
    assert!(reapply.ran, "the re-apply is not outdated: {reapply:?}");
    assert!(unanswered(&reapply.answer), "{reapply:?}");
    assert_eq!(reapply.by_the_run, NOTHING);
    assert_eq!(
        reapply.first_thaw, ONE_REQUEST,
        "the lost re-apply is owed again"
    );
    assert_eq!(reapply.second_thaw, NOTHING);
}

// Criterion (#152): the graph answering again with nothing owed publishes
// nothing — on a fresh actor, and after a route the graph answered. The
// control: once a route is lost, the same call publishes one request.
#[test]
fn test_the_graph_answering_again_with_nothing_owed_publishes_nothing() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    let (mut actor, shared, _) = actor_on(&fake);
    let mut applier = Applier::on(&shared);

    actor.graph_answers_again();
    assert_eq!(applier.look(), NOTHING, "a fresh actor owes nothing");

    let routed = lose_then_thaw(&mut actor, &shared, Instant::now(), |_| {
        route(&[target(JBL, 0)], None)
    });
    assert_eq!(routed.answer, Held::Other("Ok(())".to_string()));
    assert_eq!(routed.first_thaw, NOTHING, "an answered route owes nothing");

    fake.fail_unanswered(GraphOp::Sinks);
    let lost = lose_then_thaw(&mut actor, &shared, Instant::now(), |_| {
        route(&[target(JBL, 0)], None)
    });
    assert_eq!(lost.first_thaw, ONE_REQUEST, "control: a lost route owes");
}

// Criterion (#152): the owed re-apply survives the loss of what ran the
// message: it is held in the `Shared` every actor of a handle shares,
// not by the connection nor by one actor. A route is lost on one actor;
// that actor goes away, as with a loop thread that died; the next actor
// over the same `Shared` — told the graph answers again, by a thaw or a
// connection regained — pays it, whether or not anything plays.
#[test]
fn test_an_owed_re_apply_survives_the_actor_and_is_paid_by_the_next_one_sharing_its_state() {
    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail_unanswered(GraphOp::Sinks);
    let (mut first, shared, clock) = actor_on(&fake);
    let mut applier = Applier::on(&shared);
    let (envelope, mut answer) = route(&[target(JBL, 0)], None);
    assert!(run_alone(&mut first, envelope, Instant::now()));
    assert!(unanswered(&held(&mut answer)));
    drop(first);
    fake.clear_failures();

    let mut next = actor_sharing(&fake, &shared, &clock);
    assert_eq!(applier.look(), NOTHING, "a new actor publishes no request");
    next.graph_answers_again();

    assert_eq!(applier.look(), ONE_REQUEST);
}
