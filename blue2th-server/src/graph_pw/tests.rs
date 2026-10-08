// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::router_actor::Message;
use crate::router_handle::RouterError;
use crate::targets::MAX_OFFSET_MS;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
        let max = word(&section(&delay, "config"), "max-delay").and_then(|w| w.parse::<f32>().ok());
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
const ROUTE_FIXTURE: &str = include_str!("../../tests/fixtures/pipewire_route_params.txt");

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

    let written = PodDeserializer::deserialize_any_from(&route_pod(&parsed, vec![0.5]).unwrap())
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

// ─── #147: the graph as the router handle's transport, over a fake loop ──

impl LoopSender for mpsc::Sender<Envelope> {
    fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
        mpsc::Sender::send(self, envelope).map_err(|e| e.0)
    }
}

/// A loop thread that is already dead: its receiver is gone, so a send
/// gives the envelope back.
fn dead_loop() -> Box<dyn LoopSender> {
    let (tx, rx) = mpsc::channel::<Envelope>();
    drop(rx);
    Box::new(tx)
}

/// A loop thread that answers every route as a healthy actor would,
/// recording each message it received by its variant's name. A message
/// of another kind is dropped unanswered: these tests send routes only.
fn answering_loop(received: Arc<Mutex<Vec<String>>>) -> Box<dyn LoopSender> {
    let (tx, rx) = mpsc::channel::<Envelope>();
    std::thread::spawn(move || {
        for envelope in rx {
            received
                .lock()
                .unwrap()
                .push(envelope.message.name().to_string());
            if let Message::Route { reply, .. } = envelope.message {
                let _ = reply.send(Ok(()));
            }
        }
    });
    Box::new(tx)
}

/// Send one route through `graph`, to be started by `start_by`, and wait
/// for its answer as a handle does. `None` for a reply dropped unanswered.
fn send_route(
    graph: &mut PipeWireGraph,
    start_by: Option<Instant>,
) -> Result<Option<Result<(), RouterError>>, AudioError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    Transport::send(
        graph,
        Envelope {
            start_by,
            message: Message::Route {
                speakers: Vec::new(),
                reply,
            },
        },
    )?;
    Ok(answer.blocking_recv().ok())
}

/// Whether `sent` is a route the loop thread took and answered `Ok(())`.
fn routed(sent: &Result<Option<Result<(), RouterError>>, AudioError>) -> bool {
    matches!(sent, Ok(Some(Ok(()))))
}

// Criterion: a thread that has died (its receiver dropped) is replaced by the
// send that finds it dead, and that very message is answered by the new
// thread; the next send reuses the new thread.
#[test]
fn test_a_dead_loop_thread_is_replaced_by_one_answering_the_same_message() {
    let spawned = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&spawned);
    let received = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&received);
    let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            dead_loop()
        } else {
            answering_loop(Arc::clone(&log))
        }
    }));

    let first = send_route(&mut graph, None);
    assert!(routed(&first), "got {first:?}");
    assert_eq!(
        spawned.load(Ordering::SeqCst),
        2,
        "the dead thread and its replacement"
    );

    let second = send_route(&mut graph, None);
    assert!(routed(&second), "got {second:?}");
    assert_eq!(
        spawned.load(Ordering::SeqCst),
        2,
        "a live thread is not replaced"
    );
    assert_eq!(*received.lock().unwrap(), vec!["Route", "Route"]);
}

// Criterion: `PipeWireGraph::spawn()` starts no loop thread until the first
// command, so constructing it in a test touches no daemon.
#[test]
fn test_spawn_starts_no_loop_thread_before_the_first_command() {
    let graph = PipeWireGraph::spawn();

    assert!(graph.sender.is_none(), "no loop thread was started");
}

// Criterion (#147, non-nominal): with no graph thread at all — a detached
// graph — a message is refused at once with "not running", as every
// command is, and the envelope is dropped with its reply: a caller is
// never left waiting on a thread that is not there.
#[test]
fn test_detached_graph_refuses_a_message_at_once_and_drops_its_reply() {
    use crate::router_actor::{Envelope, Message, Transport};

    let mut graph = PipeWireGraph::detached();
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    let started = Instant::now();

    let sent = Transport::send(
        &mut graph,
        Envelope {
            start_by: None,
            message: Message::Route {
                speakers: Vec::new(),
                reply,
            },
        },
    );

    assert!(
        matches!(&sent, Err(AudioError::PipeWire(m)) if m.contains("not running")),
        "got {sent:?}"
    );
    assert!(
        matches!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ),
        "the reply went with the envelope"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no timeout is waited out, waited {:?}",
        started.elapsed()
    );
}

// ─── #146: the start deadline's constants, and the stamp across a resend ─
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
// `router_handle`'s
// `test_every_request_call_gives_up_2_s_after_it_was_made_and_closes_its_reply`.
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

/// A sender that records the `start_by` of every envelope it is handed,
/// then passes the envelope on to `inner`.
struct StampRecorder {
    stamps: Arc<Mutex<Vec<Option<Instant>>>>,
    inner: Box<dyn LoopSender>,
}

impl LoopSender for StampRecorder {
    fn send(&self, envelope: Envelope) -> Result<(), Envelope> {
        self.stamps.lock().unwrap().push(envelope.start_by);
        self.inner.send(envelope)
    }
}

/// A healthy loop thread behind a [`StampRecorder`] writing into `stamps`.
fn stamp_recording_loop(stamps: Arc<Mutex<Vec<Option<Instant>>>>) -> Box<dyn LoopSender> {
    Box::new(StampRecorder {
        stamps,
        inner: answering_loop(Arc::new(Mutex::new(Vec::new()))),
    })
}

// Criterion (#146, guard): a message resent to a replacement thread
// carries the `start_by` it was stamped with — the handle stamps it once,
// and the graph hands the envelope on as it came. The near miss is a
// replacement thread that takes 50 ms to start: a stamp made again once
// it is there lands 50 ms past the one sent, where the very instant is
// wanted. A background message keeps its absence of one just the same.
#[test]
fn test_a_message_resent_to_a_replacement_thread_keeps_the_start_by_it_was_stamped_with() {
    let stamps = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&stamps);
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let mut graph = PipeWireGraph::with_loop(Box::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            return dead_loop();
        }
        std::thread::sleep(Duration::from_millis(50));
        stamp_recording_loop(Arc::clone(&log))
    }));
    let start_by = Instant::now() + Duration::from_millis(300);

    let answer = send_route(&mut graph, Some(start_by));

    assert!(routed(&answer), "got {answer:?}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        2,
        "the dead thread and its replacement"
    );
    assert_eq!(
        *stamps.lock().unwrap(),
        vec![Some(start_by)],
        "the replacement received the message once, with the stamp it was sent with"
    );

    let background = send_route(&mut graph, None);
    assert!(routed(&background), "got {background:?}");
    assert_eq!(*stamps.lock().unwrap(), vec![Some(start_by), None]);
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
            answering_loop(Arc::new(Mutex::new(Vec::new())))
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

    // A message reuses the thread `watch` started.
    assert!(routed(&send_route(&mut graph, None)));
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
                return dead_loop();
            }
            if let Some(events) = events {
                let _ = events.send(GraphEvent::Reconnected);
            }
            answering_loop(Arc::new(Mutex::new(Vec::new())))
        },
    ));
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    graph.watch(events);
    let answer = send_route(&mut graph, None);

    assert!(routed(&answer), "got {answer:?}");
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

    assert!(routed(&send_route(&mut graph, None)));

    assert_eq!(*handed.lock().unwrap(), vec![false]);
}

// Criterion: `detached()` emits nothing, even watched, and reaches no
// daemon: its messages still err at once. The control that the channel
// works at all is the watch test above.
#[test]
fn test_detached_graph_watched_emits_nothing() {
    let mut graph = PipeWireGraph::detached();
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    graph.watch(events);
    let sent = send_route(&mut graph, None);

    assert!(
        matches!(&sent, Err(AudioError::PipeWire(m)) if m.contains("not running")),
        "got {sent:?}"
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

// Criterion (#152): a connection regained after a loss counts as "the
// graph answers again": `connected` answers `true` once per loss, the
// same once the `Reconnected` event is owed for. The thread's first
// connection is not regained, and a loop reporting "connected" on every
// wake-up while it holds the connection answers `false` after the
// first: a `true` on each would publish a re-apply per wake-up.
#[test]
fn test_reconnect_watch_connected_answers_true_once_per_loss() {
    let now = Instant::now();
    let mut watch = ReconnectWatch::new(now);

    assert!(
        !watch.connected(None),
        "the first connection is not regained"
    );
    assert!(!watch.connected(None));

    watch.lost(now);
    watch.failed(now);
    assert!(watch.connected(None), "a connection back after a loss");
    assert!(!watch.connected(None), "and only once");

    watch.lost(now);
    assert!(watch.connected(None), "a second loss, a second one");
}

// ─── #152: the late `done` of a stalled sync ────────────────────────────

// Criterion (#152): with no sync stalled, a `done` of any seq is not a
// thaw — the daemon answering every round trip is the steady state. The
// control: the same watch, once a sync stalled, takes a `done` for it.
#[test]
fn test_stall_watch_without_a_stall_takes_no_done_for_a_thaw() {
    let mut watch = StallWatch::default();

    for seq in [0, 1, 2, 3, 40, i32::MAX] {
        assert!(!watch.done(seq), "done {seq} with nothing stalled");
    }

    watch.sync_unanswered(41);
    assert!(watch.done(41), "control: a stall makes a thaw possible");
}

// Criterion (#152, guard, a thaw needs `done >= stalled seq`): after a
// stall at seq N, a `done` of N-1 — another message's round trip,
// answered before the freeze — is not a thaw, and the debt must not be
// paid on it while the daemon may still be frozen. The `done` of N is.
#[test]
fn test_stall_watch_takes_the_done_of_the_stalled_seq_and_not_the_one_before_it() {
    let mut watch = StallWatch::default();
    watch.sync_unanswered(3);

    assert!(!watch.done(2), "done N-1 is not a thaw");
    assert!(watch.done(3), "done N is the thaw");
}

// Criterion (#152): a `done` past the stalled seq is a thaw too — a later
// sync's `done` can arrive first only if the daemon answered, and
// answering N+3 means N was answered on the way.
#[test]
fn test_stall_watch_takes_a_done_past_the_stalled_seq_for_the_thaw() {
    let mut watch = StallWatch::default();
    watch.sync_unanswered(3);

    assert!(watch.done(6), "done N+3 is a thaw");
}

// Criterion (#152, exactly one thaw per stall): after a thaw a later
// `done` is not a second thaw, until the next stall; that next stall is
// thawed by its own `done`, not by a seq the first thaw already passed.
#[test]
fn test_stall_watch_takes_one_thaw_per_stall() {
    let mut watch = StallWatch::default();
    watch.sync_unanswered(3);
    assert!(watch.done(3));

    assert!(!watch.done(4), "a later done is not a second thaw");
    assert!(!watch.done(9));

    watch.sync_unanswered(12);
    assert!(!watch.done(11), "the next stall waits for its own done");
    assert!(watch.done(12), "and is thawed by it");
    assert!(!watch.done(13));
}

// Criterion (#152): the loop side takes a thaw once — what makes the
// loop pay one re-apply per thaw, whether it looks while idle or after a
// message. Before the stall's `done` there is nothing to take; the
// `done` before the stalled seq brings nothing either.
#[test]
fn test_stall_watch_holds_the_thaw_until_it_is_taken_once() {
    let mut watch = StallWatch::default();
    watch.sync_unanswered(3);
    assert!(!watch.take_thaw(), "stalled, not thawed");
    watch.done(2);
    assert!(!watch.take_thaw(), "done N-1 brings no thaw");

    watch.done(3);

    assert!(watch.take_thaw(), "the thaw is held for the loop");
    assert!(!watch.take_thaw(), "and taken once");
}

// Criterion (#152): a thaw the loop has not taken yet is dropped when a
// later sync goes unanswered — the daemon froze again, and a re-apply
// sent now would be lost to the same freeze. The debt stays owed, and is
// paid on the `done` of the new stall.
#[test]
fn test_stall_watch_drops_a_thaw_not_taken_when_the_daemon_stalls_again() {
    let mut watch = StallWatch::default();
    watch.sync_unanswered(3);
    watch.done(3);

    watch.sync_unanswered(5);

    assert!(!watch.take_thaw(), "a new stall drops the thaw");
    watch.done(4);
    assert!(!watch.take_thaw(), "done 4 is not the new stall's");
    watch.done(5);
    assert!(watch.take_thaw(), "the new stall's done thaws it");
}

// Captured (spike `pw-probe stall <pipewire_pid> 5`, PipeWire 1.4.11,
// 2026-10-04, two runs):
//   baseline sync seq=2: answered=true in 344.621µs
//   stalled sync A seq=3: answered=false at +1.602341473s after STOP
//   stalled sync B seq=4: answered=false at +3.203672025s after STOP
//   seq=3  +8.866µs after CONT
//   seq=4  +9.748µs after CONT
//   all done seqs in arrival order: [2, 3, 4] (monotonic: true)
// Criterion (#152): two syncs stalled during one freeze — seqs 3 and 4 —
// then both `done`s arriving together after `SIGCONT`: the first `done`
// at least the earliest stalled seq, 3, is the thaw, and 4 right behind
// it is not a second one. The baseline's `done` of 2, before the freeze,
// is none. Pinned reading: the earliest outstanding stall sets the
// threshold, so the thaw is seen on the first `done` after the resume.
#[test]
fn test_stall_watch_over_the_captured_freeze_takes_the_done_of_seq_3_for_the_one_thaw() {
    let mut watch = StallWatch::default();

    assert!(!watch.done(2), "the baseline, answered before the freeze");
    watch.sync_unanswered(3);
    watch.sync_unanswered(4);

    assert!(watch.done(3), "the first done after SIGCONT is the thaw");
    assert!(!watch.done(4), "its neighbour is not a second thaw");
}
