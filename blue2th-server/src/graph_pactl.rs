// SPDX-License-Identifier: MIT OR Apache-2.0
//! [`PactlGraph`]: the [`Graph`] that drives PipeWire by spawning `pactl` and
//! reading the text it prints (#79). The subprocess seam and every parser of
//! that text live here, so nothing outside this module knows what a listing
//! looks like.

use std::process::Command;

use crate::audio::{clamp_volume, AudioError, CombineBranch};
use crate::graph::{Graph, LoadedBranch};

/// The audio graph as `pactl` sees it. Stateless: every call spawns the
/// subprocess and reads the graph afresh.
#[derive(Debug, Default)]
pub struct PactlGraph;

impl PactlGraph {
    /// A graph over the `pactl` found on the `PATH`. Spawns nothing until a
    /// method is called.
    pub fn new() -> Self {
        Self
    }
}

impl Graph for PactlGraph {
    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        let listing = sink_listing()
            .ok_or_else(|| AudioError::PipeWire("pactl list sinks failed".to_string()))?;
        Ok(listing
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string())
            .collect())
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        let listing = module_listing()?;
        // A failed `pactl` yields an empty listing, which `sink_input_liveness` reads
        // as "cannot tell" rather than as a graph where every branch is dead.
        let live = sink_input_liveness(&sink_input_listing().unwrap_or_default());
        Ok(loaded_branches_with_liveness(
            &listing,
            sink_name,
            live.as_deref(),
        ))
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        load_module(&[
            "module-null-sink".to_string(),
            format!("sink_name={sink_name}"),
            format!("sink_properties=node.description={sink_name}"),
        ])
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        load_branch_loopback(sink_name, real_sink, latency_ms)
    }

    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        unload_module_id(id);
        Ok(())
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        // Every module line contains the empty pattern: an empty name would
        // unload the whole of PipeWire's module list, not a combined sink.
        // `module_line_matches` refuses it too; this is what makes the refusal
        // an error the caller sees rather than a silent no-op.
        if sink_name.is_empty() {
            return Err(AudioError::PipeWire(
                "cannot tear down a combined sink with no name".to_string(),
            ));
        }
        unload_modules_matching(&[sink_name])
    }

    fn set_default_sink(&mut self, sink: &str) -> Result<(), AudioError> {
        set_default_sink(sink)
    }

    fn sink_volume(&mut self, sink: &str) -> Option<f32> {
        let output = pactl(&["get-sink-volume", sink]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        // e.g. "Volume: front-left: 38666 /  59% / -13.75 dB,   front-right: ..."
        parse_first_percent(&String::from_utf8_lossy(&output.stdout))
    }

    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
        let pct = (clamp_volume(level) * 100.0).round() as u32;
        let level_arg = format!("{pct}%");
        let status = pactl(&["set-sink-volume", sink, &level_arg])
            .status()
            .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(AudioError::PipeWire(format!(
                "pactl set-sink-volume failed: {status}"
            )))
        }
    }
}

/// The loopback branches currently loaded for the combined sink `sink_name`,
/// read out of the text `pactl list short modules` prints: one entry per
/// `module-loopback` fed by `<sink_name>.monitor`, carrying the **resolved** sink
/// node it feeds and its `latency_msec`. Pure — performs no I/O.
///
/// The input comes from a subprocess, so anything that does not parse is skipped
/// rather than reported: a truncated or unexpected listing yields fewer branches,
/// never a failure.
pub fn loaded_branches(
    listing: &str,
    sink_name: &str,
    live: Option<&[SinkInputStream]>,
) -> Vec<CombineBranch> {
    branch_modules(listing, sink_name)
        .into_iter()
        .filter(|(module_id, _)| match live {
            // Nothing could be read about liveness: keep every branch, or a
            // transient `pactl` failure would read as "everything is dead" and
            // rebuild the whole graph under the audio it protects.
            None => true,
            Some(streams) => module_is_live(streams, *module_id),
        })
        .map(|(_, branch)| branch)
        .collect()
}

/// What [`Graph::branches`] hands the router, read out of the two listings: every
/// loopback loaded for `sink_name` with its module id and a three-valued
/// liveness — live, dead, or `None` for every branch when the sink-input listing
/// could not be read. Pure — performs no I/O.
fn loaded_branches_with_liveness(
    listing: &str,
    sink_name: &str,
    live: Option<&[SinkInputStream]>,
) -> Vec<LoadedBranch> {
    let dead = dead_branch_modules(listing, sink_name, live);
    branch_modules(listing, sink_name)
        .into_iter()
        .map(|(id, branch)| LoadedBranch {
            id,
            branch,
            live: live.map(|_| !dead.contains(&id)),
        })
        .collect()
}

/// Every loopback branch loaded for `sink_name`, paired with the id of the module
/// carrying it — the id `pactl unload-module` takes, and the one a sink-input
/// reports in `Owner Module:`. Pure — performs no I/O.
fn branch_modules(listing: &str, sink_name: &str) -> Vec<(u32, CombineBranch)> {
    let source = format!("source={sink_name}.monitor");
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let module_id: u32 = columns.next()?.parse().ok()?;
            if columns.next()? != "module-loopback" {
                return None;
            }
            let args: Vec<&str> = columns.next()?.split_whitespace().collect();
            if !args.contains(&source.as_str()) {
                return None;
            }
            let sink = args.iter().find_map(|a| a.strip_prefix("sink="))?;
            // `pactl` accepts a `sink=` carrying no value and prints it back
            // verbatim. It names no node, and an empty name is a wildcard to
            // every prefix or substring predicate downstream — it once built an
            // unload pattern, `sink=`, that every loopback line of the combined
            // sink contained. Rejected at the parser, so it never travels.
            if sink.is_empty() {
                return None;
            }
            let latency_ms = args
                .iter()
                .find_map(|a| a.strip_prefix("latency_msec="))?
                .parse()
                .ok()?;
            Some((
                module_id,
                CombineBranch {
                    sink: sink.to_string(),
                    latency_ms,
                },
            ))
        })
        .collect()
}

/// The module ids of the loopbacks loaded for `sink_name` that are loaded but
/// dead: listed, yet feeding nothing. Empty when liveness could not be read.
///
/// These are the branches [`loaded_branches_with_liveness`] reports as
/// `live: Some(false)`, which the router unloads by id before loading the
/// replacement: two loopbacks onto the same speaker would double the audio (#75).
fn dead_branch_modules(
    listing: &str,
    sink_name: &str,
    live: Option<&[SinkInputStream]>,
) -> Vec<u32> {
    let Some(streams) = live else {
        return Vec::new();
    };
    branch_modules(listing, sink_name)
        .into_iter()
        .filter(|(module_id, _)| !module_is_live(streams, *module_id))
        .map(|(module_id, _)| module_id)
        .collect()
}

/// One playback stream as `pactl list sink-inputs` reports it: the id of the
/// module that owns it, and the index of the sink it feeds.
///
/// A `module-loopback`'s playback stream reports its own module id in
/// `Owner Module:`, which is what ties a loaded branch to the audio it is — or is
/// not — carrying. A plain client reports `n/a` there and owns no module of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkInputStream {
    /// The `Owner Module:` field.
    pub owner_module: u32,
    /// The `Sink:` field: PulseAudio's index of the sink this stream feeds.
    pub sink: u32,
}

/// Read the text `pactl list sink-inputs` prints into one entry per stream that
/// belongs to a module. Pure — performs no I/O.
///
/// The input comes from a subprocess, so anything that does not parse is skipped
/// rather than reported.
pub fn parse_sink_inputs(listing: &str) -> Vec<SinkInputStream> {
    let mut streams = Vec::new();
    let mut owner_module: Option<u32> = None;
    let mut sink: Option<u32> = None;
    let mut flush = |owner_module: &mut Option<u32>, sink: &mut Option<u32>| {
        if let (Some(owner_module), Some(sink)) = (owner_module.take(), sink.take()) {
            streams.push(SinkInputStream { owner_module, sink });
        }
    };
    for line in listing.lines() {
        let field = line.trim();
        if field.starts_with("Sink Input #") {
            // A new block starts: whatever the previous one gathered is complete.
            flush(&mut owner_module, &mut sink);
        } else if let Some(value) = field.strip_prefix("Owner Module:") {
            // `n/a` (a plain client) fails to parse, which is exactly the skip
            // wanted: its `Sink:` vouches for no module of ours.
            owner_module = value.trim().parse().ok();
        } else if let Some(value) = field.strip_prefix("Sink:") {
            sink = value.trim().parse().ok();
        }
    }
    flush(&mut owner_module, &mut sink);
    streams
}

/// What a sink-input listing lets us conclude about liveness: `Some(streams)`
/// when it could be read, `None` when it could not.
///
/// The distinction matters because the repair pass only runs while audio is
/// flowing: with something playing, a listing carrying no stream at all is a
/// failed `pactl`, not a graph where every branch is dead. Concluding the latter
/// would rebuild the whole graph and interrupt the audio the pass exists to
/// protect.
pub fn sink_input_liveness(listing: &str) -> Option<Vec<SinkInputStream>> {
    let streams = parse_sink_inputs(listing);
    if streams.is_empty() {
        return None;
    }
    Some(streams)
}

/// Whether a loaded `module-loopback` is actually feeding a speaker: it has a
/// playback stream, and that stream sits on a real sink.
///
/// A module outlives its sink's node — a module is not a node — so a loopback can
/// stay loaded while carrying nothing (#75).
pub fn module_is_live(streams: &[SinkInputStream], module_id: u32) -> bool {
    streams
        .iter()
        .any(|s| s.owner_module == module_id && s.sink != INVALID_SINK_INDEX)
}

/// PulseAudio's invalid sink index: what a loopback's playback stream reports
/// once the node it was pinned to is gone.
const INVALID_SINK_INDEX: u32 = u32::MAX;

/// A `pactl` invocation described as data — program, arguments, environment —
/// rather than as a built [`Command`], which cannot be inspected once created.
/// Same seam as `build_librespot_args`: it is what lets a test pin what the
/// subprocess is actually asked to do without spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PactlCommand {
    /// The program to run.
    pub program: String,
    /// The arguments, in order.
    pub args: Vec<String>,
    /// Environment variables set on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

/// Describe a `pactl` invocation carrying `args`, forcing `LC_ALL=C`: `pactl`'s
/// long listings are translated, so under a non-English locale every label the
/// parsers look for is absent and a dead branch reads as "cannot tell" forever
/// (#75). The locale is added; the program and the arguments travel untouched.
pub fn build_pactl_command(args: &[&str]) -> PactlCommand {
    PactlCommand {
        program: "pactl".to_string(),
        // Owned copies: the description outlives the borrowed argument slice.
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: vec![("LC_ALL".to_string(), "C".to_string())],
    }
}

/// The one place a [`PactlCommand`] becomes a runnable [`Command`]. Every call
/// site goes through it, so the locale cannot be forgotten at a single seam —
/// a built `Command` is opaque, so this is the only reviewable guarantee.
fn pactl(args: &[&str]) -> Command {
    let described = build_pactl_command(args);
    let mut command = Command::new(&described.program);
    command.args(&described.args);
    for (key, value) in &described.env {
        command.env(key, value);
    }
    command
}

/// The text `pactl list short modules` prints, for [`loaded_branches`] and
/// [`unload_modules_matching`] to read.
fn module_listing() -> Result<String, AudioError> {
    let output = pactl(&["list", "short", "modules"])
        .output()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if !output.status.success() {
        return Err(AudioError::PipeWire(
            "pactl list modules failed".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The text `pactl list sink-inputs` prints, for [`sink_input_liveness`] to read.
fn sink_input_listing() -> Result<String, AudioError> {
    let output = pactl(&["list", "sink-inputs"])
        .output()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if !output.status.success() {
        return Err(AudioError::PipeWire(
            "pactl list sink-inputs failed".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Unload every loaded module whose `pactl list short modules` line
/// [`module_line_matches`] `patterns`. Best-effort — a missing module is not an
/// error.
fn unload_modules_matching(patterns: &[&str]) -> Result<(), AudioError> {
    for line in module_listing()?.lines() {
        if module_line_matches(line, patterns) {
            if let Some(id) = line.split('\t').next() {
                // Best-effort: ignore failures so one stale module cannot block teardown.
                let _ = pactl(&["unload-module", id]).status();
            }
        }
    }
    Ok(())
}

/// Whether a module line carries every one of `patterns`, and there is at least
/// one. Pure — performs no I/O.
///
/// An empty pattern is a substring of every line, so it matches none: what
/// matches here is unloaded, and "everything" is never what a caller means.
/// The guard sits in the predicate itself, where the wildcard lives, so no
/// caller has to remember it.
fn module_line_matches(line: &str, patterns: &[&str]) -> bool {
    !patterns.is_empty()
        && patterns
            .iter()
            .all(|pattern| !pattern.is_empty() && line.contains(pattern))
}

/// Unload one module by the id `pactl` printed for it. Best-effort, like
/// [`unload_modules_matching`]: a module that is already gone is not an error,
/// and one failure must not stop the rest of a repair.
fn unload_module_id(module_id: u32) {
    let _ = pactl(&["unload-module", &module_id.to_string()]).status();
}

/// Load a PipeWire module via `pactl load-module <args...>`, mapping a failure to
/// an [`AudioError::PipeWire`].
fn load_module(args: &[String]) -> Result<(), AudioError> {
    let mut argv: Vec<&str> = vec!["load-module"];
    argv.extend(args.iter().map(String::as_str));
    let status = pactl(&argv)
        .status()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioError::PipeWire(format!(
            "pactl load-module failed: {status}"
        )))
    }
}

/// The text `pactl list short sinks` prints, or `None` when it could not be read.
/// The distinction matters: an unreadable listing is "cannot tell", and reading
/// it as "no sink exists" would unload every branch and cut the sound.
fn sink_listing() -> Option<String> {
    let output = pactl(&["list", "short", "sinks"]).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Make `sink` the default PipeWire sink (by node name) via `pactl`.
fn set_default_sink(sink: &str) -> Result<(), AudioError> {
    let status = pactl(&["set-default-sink", sink])
        .status()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioError::PipeWire(format!(
            "pactl set-default-sink failed: {status}"
        )))
    }
}

/// Load one delayed loopback from the combined sink's monitor into a resolved
/// speaker sink. `*_dont_move=true` pins both ends, so a default-sink change
/// cannot drag the branch off the speaker it was built for.
fn load_branch_loopback(
    sink_name: &str,
    real_sink: &str,
    latency_ms: u32,
) -> Result<(), AudioError> {
    load_module(&[
        "module-loopback".to_string(),
        format!("source={sink_name}.monitor"),
        format!("sink={real_sink}"),
        format!("latency_msec={latency_ms}"),
        "source_dont_move=true".to_string(),
        "sink_dont_move=true".to_string(),
    ])
}

/// Extract the first `<n>%` from `pactl get-sink-volume` output as a fraction
/// (`59%` -> `0.59`). Above `1.0` for an over-amplified sink, which prints `153%`.
fn parse_first_percent(text: &str) -> Option<f32> {
    let pct_end = text.find('%')?;
    // Collect the digit run immediately before '%' (char-wise, no byte slicing).
    let digits: String = text[..pct_end]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let pct: u32 = digits.parse().ok()?;
    Some(pct as f32 / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::*;
    use blue2th_proto::SpeakerTarget;

    // The live sink volume read from `pactl get-sink-volume` is parsed into
    // `0.0..=1.0`.
    #[test]
    fn test_parse_first_percent_reads_volume_fraction() {
        let line = "Volume: front-left: 38666 /  59% / -13.75 dB,   front-right: 38666 /  59%";
        assert_eq!(parse_first_percent(line), Some(0.59));
        assert_eq!(
            parse_first_percent("Volume: front-left: 0 / 0% / -inf dB"),
            Some(0.0)
        );
        assert_eq!(parse_first_percent("no percentage here"), None);
    }

    // Criterion: that function is the single place the base is applied, so what
    // `loaded_branches` parses out of a loaded module and what the plan asks for
    // are the same quantity. Applying the base only at load time would have every
    // reconciliation compare a loaded 120 against a planned 70, see a mismatch and
    // reload every branch on every tick — an audio interruption every five seconds.
    #[test]
    fn test_branch_latency_round_trips_from_the_plan_through_the_module_listing() {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 70,
            },
        ]);
        // What `pactl list short modules` prints back for a graph loaded from this
        // very plan: one loopback per branch, on the resolved node, carrying the
        // latency the plan asked for.
        let listing: String = spec
            .branches
            .iter()
            .enumerate()
            .map(|(index, branch)| {
                format!(
                    "{}\tmodule-loopback\tsource={}.monitor sink={}.1 latency_msec={} source_dont_move=true sink_dont_move=true\n",
                    27 + index,
                    spec.sink_name,
                    branch.sink,
                    branch.latency_ms,
                )
            })
            .collect();

        let loaded = loaded_branches(&listing, &spec.sink_name, None);

        assert_eq!(
            loaded.iter().map(|b| b.latency_ms).collect::<Vec<_>>(),
            vec![50, 120],
            "the plan already carries the base, so the loaded modules do too"
        );
        assert_eq!(
            reconcile_branches(&loaded, &spec),
            BranchReconciliation::default(),
            "a graph loaded from the plan reconciles against it as a no-op"
        );
    }

    // Criterion: `start()` propagates a *resolution failure*, so the resolver
    // must report one rather than hand the target back unresolved — a silent
    // fallback to the default sink is the defect this change fixes. Needs no
    // hardware and holds either way: with no `pactl` the lookup fails outright,
    // and with a live one no sink can carry this name.
    #[test]
    fn test_resolve_target_sink_for_an_absent_node_is_an_error() {
        assert!(
            AudioRouter::new(Box::new(PactlGraph::new()))
                .resolve_target_sink("blue2th_no_such_sink_ever")
                .is_err(),
            "an unresolvable target must be an error, never the target handed back"
        );
    }

    // The empty target `spotify_target_sink(&[])` yields must not resolve to the
    // first sink `pactl` happens to list.
    #[test]
    fn test_resolve_target_sink_for_an_empty_target_is_an_error() {
        assert!(
            AudioRouter::new(Box::new(PactlGraph::new()))
                .resolve_target_sink("")
                .is_err(),
            "an empty target names no node and must not resolve to an arbitrary sink"
        );
    }

    /// A realistic `pactl list short modules` block: index, module name and the
    /// argument string, tab-separated. It carries the combined sink's own null
    /// sink, its two delayed loopbacks, a loopback belonging to a *different*
    /// combined sink, a loopback feeding *into* the combined sink (whose
    /// `sink=` mentions it but which is not one of its branches) and ordinary
    /// unrelated modules.
    ///
    /// The last three lines are the near misses, each shaped like a branch on one
    /// axis only: a module that is not a loopback, a loopback whose `source=`
    /// merely *contains* ours inside a longer token, and a loopback whose `sink=`
    /// carries no value. The module ids are the wide ones PipeWire hands out.
    ///
    /// The two branch latencies are what a plan at offsets 0 and 250 loads, base
    /// included, so a listing captured from a healthy graph reconciles clean
    /// against [`two_speaker_spec`].
    const PACTL_MODULES: &str = concat!(
        "10\tmodule-device-restore\t\n",
        "26\tmodule-null-sink\tsink_name=blue2th_combined sink_properties=node.description=blue2th_combined\n",
        "27\tmodule-loopback\tsource=blue2th_combined.monitor sink=bluez_output.80_99_E7_63_50_29.1 latency_msec=50 source_dont_move=true sink_dont_move=true\n",
        "28\tmodule-loopback\tsource=blue2th_combined.monitor sink=bluez_output.11_22_33_44_55_66.1 latency_msec=300 source_dont_move=true sink_dont_move=true\n",
        "29\tmodule-loopback\tsource=other_combined.monitor sink=bluez_output.AA_BB_CC_DD_EE_FF.1 latency_msec=120 source_dont_move=true sink_dont_move=true\n",
        "30\tmodule-loopback\tsource=alsa_input.pci-0000_00_1f.3.analog-stereo sink=blue2th_combined latency_msec=40\n",
        "31\tmodule-switch-on-connect\t\n",
        "536870915\tmodule-remap-sink\tsink_name=remap source=blue2th_combined.monitor sink=bluez_output.99_88_77_66_55_44.1 latency_msec=0\n",
        "536870916\tmodule-loopback\tsource=alsa_input.pci-0000_00_1f.3.analog-stereo sink=bluez_output.99_88_77_66_55_44.1 latency_msec=0 sink_properties=media.name=source=blue2th_combined.monitor\n",
        "536870917\tmodule-loopback\tsource=blue2th_combined.monitor sink= latency_msec=20\t\n",
    );

    // A module is a branch because of the *name* in its second column, not because
    // its arguments look like one: `unload_modules_matching` would otherwise unload
    // a module blue2th never created. Nothing else in the listing distinguishes the
    // `module-remap-sink` line, so dropping the name check leaves no other trace.
    #[test]
    fn test_loaded_branches_ignores_a_non_loopback_module_shaped_like_a_branch() {
        assert!(
            !loaded_branches(PACTL_MODULES, "blue2th_combined", None)
                .iter()
                .any(|b| b.sink.contains("99_88_77_66_55_44")),
            "only module-loopback lines are branches"
        );
    }

    // `source=` identifies a branch as a whole argument token: a loopback carrying
    // `source=blue2th_combined.monitor` *inside* a longer token (here a
    // `sink_properties=media.name=…`) belongs to another source entirely, and
    // matching it as a substring would hand back a branch pointing at the wrong
    // speaker.
    #[test]
    fn test_loaded_branches_ignores_a_loopback_whose_source_only_contains_ours() {
        let line = "536870916\tmodule-loopback\tsource=alsa_input.pci-0000_00_1f.3.analog-stereo sink=bluez_output.99_88_77_66_55_44.1 latency_msec=0 sink_properties=media.name=source=blue2th_combined.monitor\n";
        assert!(
            line.contains("source=blue2th_combined.monitor"),
            "the line does contain the marker, so only a token-wise match rejects it"
        );

        assert!(
            loaded_branches(line, "blue2th_combined", None).is_empty(),
            "the marker sits inside another token, so this is not one of our branches"
        );
    }

    // `pactl` accepts a `sink=` carrying no value and prints it back verbatim
    // (checked against PipeWire). Such a module names no node, and
    // `reconcile_combined` builds its unload pattern from that name — `sink=`,
    // which every loopback line of the combined sink contains. Admitting it as a
    // branch would put it in `to_unload` and take every other branch with it.
    #[test]
    fn test_loaded_branches_skips_a_loopback_whose_sink_names_no_node() {
        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined", None);

        assert!(
            branches.iter().all(|b| !b.sink.is_empty()),
            "a branch must name a node, got {branches:?}"
        );
        assert!(
            PACTL_MODULES
                .lines()
                .filter(|l| l.contains("source=blue2th_combined.monitor"))
                .all(|l| l.contains("sink=")),
            "the unload pattern an empty sink builds matches every branch line"
        );
    }

    // Criterion: a pure function reads `pactl list short modules` and returns the
    // loopback branches loaded for a given combined sink, each with the real sink
    // it feeds and its `latency_msec`.
    #[test]
    fn test_loaded_branches_reads_each_loopback_sink_and_latency() {
        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined", None);

        assert_eq!(
            branches,
            vec![
                CombineBranch {
                    sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
                    latency_ms: 50,
                },
                CombineBranch {
                    sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                    latency_ms: 300,
                },
            ],
            "the two loopbacks fed by blue2th_combined.monitor, with their latencies"
        );
    }

    // Criterion: the parser ignores modules belonging to another sink name, and
    // any module that is not a `module-loopback` — in particular the null sink,
    // which a selection change must never unload.
    #[test]
    fn test_loaded_branches_ignores_other_sinks_and_non_loopback_modules() {
        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined", None);

        assert!(
            !branches
                .iter()
                .any(|b| b.sink.contains("AA_BB_CC_DD_EE_FF")),
            "a loopback of another combined sink is not one of our branches: {branches:?}"
        );
        assert!(
            !branches.iter().any(|b| b.sink.contains("blue2th_combined")),
            "neither the null sink nor a loopback feeding into it is a branch: {branches:?}"
        );
    }

    // Criterion: the same parser reads another sink's branches without picking
    // ours up, i.e. the match is on `source=<sink_name>.monitor`.
    #[test]
    fn test_loaded_branches_reads_only_the_named_sinks_branches() {
        let branches = loaded_branches(PACTL_MODULES, "other_combined", None);

        assert_eq!(
            branches,
            vec![CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_FF.1".to_string(),
                latency_ms: 120,
            }],
            "only the loopback fed by other_combined.monitor"
        );
    }

    // Criterion: a listing with nothing matching yields an empty set rather than
    // a panic — the input comes from a subprocess and may be anything.
    #[test]
    fn test_loaded_branches_without_a_matching_module_is_empty() {
        assert!(loaded_branches("", "blue2th_combined", None).is_empty());
        assert!(loaded_branches(
            "10\tmodule-device-restore\t\n31\tmodule-switch-on-connect\t\n",
            "blue2th_combined",
            None
        )
        .is_empty());
        assert!(
            loaded_branches("27\tmodule-loopback", "blue2th_combined", None).is_empty(),
            "a truncated line names no sink and no latency, so it is no branch"
        );
    }

    /// The spec the reconciliation tests compare a loaded graph against: two
    /// speakers, offsets 0 and 250 ms, branch sinks held as `bluez_output.*`
    /// **prefixes** the way [`combine_sink_plan`] builds them.
    fn two_speaker_spec() -> CombineSinkSpec {
        combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 250,
            },
        ])
    }

    // Criterion: a speaker present in both the loaded set and the spec with the
    // same latency appears in neither list — an unchanged selection touches
    // nothing, which is what keeps the stream alive.
    #[test]
    fn test_reconcile_branches_unchanged_selection_changes_nothing() {
        let spec = two_speaker_spec();
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, None);

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(plan, BranchReconciliation::default());
    }

    // Criterion: a speaker dropped from the selection yields exactly one unload,
    // naming the resolved node its loopback feeds, and never the null sink.
    #[test]
    fn test_reconcile_branches_dropped_speaker_unloads_only_that_branch() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, None);

        let plan = reconcile_branches(&loaded, &spec);

        assert!(
            plan.to_load.is_empty(),
            "the remaining speaker's loopback is already loaded, got {:?}",
            plan.to_load
        );
        assert_eq!(
            plan.to_unload,
            vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: branch_latency_ms(250),
            }],
            "only the deselected speaker's loopback is unloaded"
        );
        assert!(
            !plan
                .to_unload
                .iter()
                .any(|b| b.sink.contains(&spec.sink_name)),
            "the null sink is never unloaded by a selection change: {:?}",
            plan.to_unload
        );
    }

    // Criterion: a speaker present in both but with a different latency is
    // reloaded — the new branch is loaded and the stale loopback must not
    // survive.
    #[test]
    fn test_reconcile_branches_latency_change_replaces_the_stale_loopback() {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 400,
            },
        ]);
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, None);

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load, spec.branches,
            "the retuned speaker carries its new latency, and every branch is reloaded with it"
        );
        assert_eq!(
            plan.to_unload, loaded,
            "every loaded loopback is unloaded, the stale one included"
        );
        assert!(
            plan.to_load
                .iter()
                .any(|b| b.latency_ms == branch_latency_ms(400)),
            "the new offset reaches the plan, got {:?}",
            plan.to_load
        );
    }

    /// A realistic `pactl list sink-inputs` block, in the shape captured from a
    /// live system with a probe loopback loaded: a plain client reports
    /// `Owner Module: n/a`, while a loopback's playback stream reports the id of
    /// the module that owns it.
    ///
    /// Module 27 is the first branch of [`PACTL_MODULES`] and feeds a real sink;
    /// module 28 is its second branch, and its stream sits on `4294967295` —
    /// PulseAudio's invalid index, the signature captured while a returned
    /// speaker stayed silent (#75).
    const PACTL_SINK_INPUTS: &str = concat!(
        "Sink Input #135\n",
        "\tDriver: PipeWire\n",
        "\tOwner Module: n/a\n",
        "\tClient: 134\n",
        "\tSink: 5691\n",
        "\tProperties:\n",
        "\t\tapplication.name = \"speech-dispatcher-dummy\"\n",
        "\t\tnode.name = \"speech-dispatcher-dummy\"\n",
        "\n",
        "Sink Input #28098\n",
        "\tDriver: PipeWire\n",
        "\tOwner Module: 27\n",
        "\tClient: n/a\n",
        "\tSink: 28091\n",
        "\tProperties:\n",
        "\t\tnode.name = \"output.loopback-6815-13\"\n",
        "\t\tmedia.name = \"loopback-6815-13 output\"\n",
        "\n",
        "Sink Input #28099\n",
        "\tDriver: PipeWire\n",
        "\tOwner Module: 28\n",
        "\tClient: n/a\n",
        "\tSink: 4294967295\n",
        "\tProperties:\n",
        "\t\tnode.name = \"output.loopback-6815-14\"\n",
        "\t\tmedia.name = \"loopback-6815-14 output\"\n",
    );

    // Criterion: a pure function parses `pactl list sink-inputs` into, for each
    // stream, its owning module id and the sink it feeds.
    #[test]
    fn test_parse_sink_inputs_reads_each_streams_module_and_sink() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert_eq!(
            streams,
            vec![
                SinkInputStream {
                    owner_module: 27,
                    sink: 28091,
                },
                SinkInputStream {
                    owner_module: 28,
                    sink: INVALID_SINK_INDEX,
                },
            ],
            "one entry per stream owning a module, with the sink it feeds"
        );
    }

    // Criterion: a stream with no owning module (`Owner Module: n/a`, what a plain
    // client reports) is ignored — it belongs to no module of ours, and taking its
    // `Sink:` would make a foreign stream vouch for one of our branches.
    #[test]
    fn test_parse_sink_inputs_skips_a_stream_owning_no_module() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert!(
            !streams.iter().any(|s| s.sink == 5691),
            "the sink of the `n/a` client must not appear: {streams:?}"
        );
        assert_eq!(streams.len(), 2, "only the two module-owned streams");
    }

    // Criterion: a loopback module with a stream on a real sink is live.
    #[test]
    fn test_module_is_live_with_a_stream_on_a_real_sink_is_live() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert!(
            module_is_live(&streams, 27),
            "module 27's stream feeds sink 28091, so the branch carries audio"
        );
    }

    // Criterion: a module whose stream sits on `4294967295` is not live. This is
    // the captured signature of the defect: the loopback survived its sink's node,
    // `sink_dont_move=true` kept it from re-attaching, and it now feeds nothing.
    #[test]
    fn test_module_is_live_with_a_stream_on_the_invalid_sink_is_not_live() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert!(
            !module_is_live(&streams, 28),
            "a stream on {INVALID_SINK_INDEX} feeds nothing, so the branch is dead"
        );
    }

    // Criterion: a module with no sink-input at all is not live.
    #[test]
    fn test_module_is_live_without_any_stream_is_not_live() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert!(
            !streams.iter().any(|s| s.owner_module == 29),
            "module 29 owns no stream in the fixture, which is the case under test"
        );
        assert!(
            !module_is_live(&streams, 29),
            "no stream at all is as dead as a stream on the invalid sink"
        );
    }

    // Criterion: `loaded_branches` counts only live branches, so a stale loopback
    // reads as absent — which is what makes the reconciliation rebuild it.
    #[test]
    fn test_loaded_branches_drops_a_branch_whose_module_is_not_live() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined", Some(&streams));

        assert_eq!(
            branches,
            vec![CombineBranch {
                sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
                latency_ms: 50,
            }],
            "module 28's loopback feeds nothing, so its branch is absent"
        );
    }

    // Criterion: `loaded_branches` keeps a branch whose module is live — the
    // healthy graph must stay a no-op, or the pass would churn the audio it
    // protects.
    #[test]
    fn test_loaded_branches_keeps_a_branch_whose_module_is_live() {
        let streams = vec![
            SinkInputStream {
                owner_module: 27,
                sink: 28091,
            },
            SinkInputStream {
                owner_module: 28,
                sink: 28092,
            },
        ];

        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined", Some(&streams));
        let plan = reconcile_branches(&branches, &two_speaker_spec());

        assert_eq!(branches.len(), 2, "both loopbacks feed a real sink");
        assert_eq!(
            plan,
            BranchReconciliation::default(),
            "every planned branch is live, so nothing is loaded and nothing unloaded"
        );
    }

    // Criterion: the stale module is unloaded before the replacement is loaded.
    // Only the pure half is pinnable here — once the dead branch reads as absent,
    // the reconciliation asks for that speaker to be loaded again. Performing the
    // unload first is `reconcile_combined`'s `pactl` seam, which CI does not run.
    #[test]
    fn test_reconcile_branches_asks_to_reload_a_branch_ruled_dead() {
        let spec = two_speaker_spec();
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, Some(&streams));
        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load, spec.branches,
            "the dead speaker's branch is rebuilt, and the live ones with it"
        );
        assert_eq!(
            plan.to_unload, loaded,
            "the branch that survived is torn down too, so both start in one pass"
        );
    }

    // Criterion: an empty or truncated listing yields no streams.
    #[test]
    fn test_parse_sink_inputs_of_an_unreadable_listing_yields_no_streams() {
        assert!(parse_sink_inputs("").is_empty());
        assert!(
            parse_sink_inputs("Sink Input #28098\n\tDriver: PipeWire\n").is_empty(),
            "a stream naming neither module nor sink is no stream"
        );
    }

    // Criterion: an unreadable listing reads as "cannot tell", not "everything is
    // dead" — the pass only runs while audio flows, so a listing with no stream at
    // all is a failed `pactl`, and treating it as death would rebuild the whole
    // graph and cut the sound.
    #[test]
    fn test_sink_input_liveness_of_an_unreadable_listing_is_unknown() {
        assert!(
            sink_input_liveness("").is_none(),
            "an empty listing tells us nothing about any branch"
        );
        assert!(
            sink_input_liveness(PACTL_SINK_INPUTS).is_some(),
            "a listing that parses does tell us"
        );
    }

    // Criterion: an unreadable listing must not empty the branch set — the caller
    // keeps every loaded branch, so the reconciliation stays a no-op.
    #[test]
    fn test_loaded_branches_with_unknown_liveness_keeps_every_branch() {
        let spec = two_speaker_spec();
        let unknown = sink_input_liveness("");

        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, unknown.as_deref());
        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(loaded.len(), 2, "cannot tell is not everything is dead");
        assert_eq!(
            plan,
            BranchReconciliation::default(),
            "a transient pactl failure must not rebuild the graph"
        );
    }

    /// A `pactl list sink-inputs` block as it really prints under the operator's
    /// `fr_FR.UTF-8`: every label is translated, the block header included —
    /// captured from the running backend, where `sink_input_listing ok=true
    /// len=7172` came with `live=None` on every tick (#75).
    const PACTL_SINK_INPUTS_FR: &str = concat!(
        "Entrée de la destination #135\n",
        "\tPilote : PipeWire\n",
        "\tModule du propriétaire : n/d\n",
        "\tClient : 134\n",
        "\tDestination : 29979\n",
        "\tSpécification de l’échantillon : s16le 1ch 44100Hz\n",
        "Entrée de la destination #31519\n",
        "\tPilote : PipeWire\n",
        "\tModule du propriétaire : 536870917\n",
        "\tClient : n/d\n",
        "\tDestination : 31506\n",
        "\tSpécification de l’échantillon : float32le 2ch 48000Hz\n",
    );

    // Criterion: every `pactl` invocation carries `LC_ALL=C`. The rule is pinned on
    // the pure description of the command, because a `std::process::Command` cannot
    // be inspected once built — the same seam `build_librespot_args` uses for argv.
    #[test]
    fn test_build_pactl_command_forces_the_c_locale() {
        let described = build_pactl_command(&["list", "sink-inputs"]);

        assert!(
            described
                .env
                .iter()
                .any(|(key, value)| key == "LC_ALL" && value == "C"),
            "the invocation must force LC_ALL=C: {described:?}"
        );
    }

    // Criterion: the locale is *added*, nothing else is rewritten — the program is
    // still `pactl` and the arguments arrive unchanged, in order.
    #[test]
    fn test_build_pactl_command_keeps_program_and_arguments_unchanged() {
        let described = build_pactl_command(&["list", "short", "modules"]);

        assert_eq!(described.program, "pactl");
        assert_eq!(
            described.args,
            vec![
                "list".to_string(),
                "short".to_string(),
                "modules".to_string()
            ],
            "the arguments must travel through untouched: {described:?}"
        );
    }

    // Criterion: the locale rule holds for *every* call site, not only the two long
    // listings — the parse-breaking translation is the reason, but a description
    // that only sometimes carries the locale would leave the rule to be
    // rediscovered one seam at a time.
    #[test]
    fn test_build_pactl_command_forces_the_locale_for_every_call_site() {
        for args in [
            vec!["list", "short", "modules"],
            vec!["list", "sink-inputs"],
            vec!["list", "short", "sinks"],
            vec!["load-module", "module-null-sink"],
            vec!["unload-module", "536870917"],
            vec!["set-default-sink", "blue2th_combined"],
            vec!["set-sink-volume", "blue2th_combined", "50%"],
            vec!["get-sink-volume", "blue2th_combined"],
        ] {
            let described = build_pactl_command(&args);
            assert!(
                described
                    .env
                    .iter()
                    .any(|(key, value)| key == "LC_ALL" && value == "C"),
                "every pactl call site must force the locale, {args:?} does not"
            );
            assert_eq!(
                described.args,
                args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                "adding the locale must not rewrite the arguments of {args:?}"
            );
        }
    }

    // Criterion: a French listing yields no streams. This is a regression guard
    // that documents *why* the `LC_ALL=C` exists, and it must not be answered by
    // teaching the parser French: `Owner Module:` and `Sink:` are absent from every
    // translated locale, so chasing labels language by language would be endless.
    // Keep the locale forced, and this test stays the reason it is not noise.
    #[test]
    fn test_parse_sink_inputs_of_a_french_listing_yields_no_streams() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS_FR);

        assert!(
            streams.is_empty(),
            "a translated listing names no field the parser knows: {streams:?}"
        );
    }

    // Criterion: and because it yields no streams, a French listing reads as
    // "cannot tell" — which is exactly what was observed live (`live=None` on every
    // tick, with 7 KB of listing read fine): the liveness filter is skipped and a
    // dead branch is never detected. Forcing the locale is what closes it.
    #[test]
    fn test_sink_input_liveness_of_a_french_listing_is_unknown() {
        assert!(
            sink_input_liveness(PACTL_SINK_INPUTS_FR).is_none(),
            "an untranslated parser learns nothing from a translated listing"
        );
    }

    // Criterion: the invalid index the liveness rule tests against is the one
    // `pactl` really prints — the fixture carries the literal 4294967295, and the
    // production constant must be that same number. Without this the fixture and
    // `module_is_live` could drift apart while both tests stayed green.
    #[test]
    fn test_invalid_sink_index_is_the_number_pactl_prints() {
        assert_eq!(INVALID_SINK_INDEX, 4_294_967_295);
        assert!(
            PACTL_SINK_INPUTS.contains("Sink: 4294967295"),
            "the fixture must carry the literal the parser has to read"
        );
    }

    // Criterion: the stale module is unloaded before the replacement is loaded,
    // and `reconcile_branches` cannot ask for it — a dead branch is deliberately
    // absent from `loaded_branches`, so it is named by module id or not at all.
    // Module 28's loopback sits on the invalid sink and must be the one named.
    #[test]
    fn test_dead_branch_modules_names_the_module_of_a_branch_feeding_nothing() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        assert_eq!(
            dead_branch_modules(PACTL_MODULES, "blue2th_combined", Some(&streams)),
            vec![28],
            "only the loopback whose stream feeds nothing is unloaded by id"
        );
    }

    // Criterion: the module ids to unload and the branches that survive partition
    // the loaded loopbacks — a live branch is never unloaded out from under the
    // audio it is carrying, which is the whole risk of unloading by id.
    #[test]
    fn test_dead_branch_modules_never_names_a_live_branchs_module() {
        let streams = vec![
            SinkInputStream {
                owner_module: 27,
                sink: 28091,
            },
            SinkInputStream {
                owner_module: 28,
                sink: 28092,
            },
        ];

        assert!(
            dead_branch_modules(PACTL_MODULES, "blue2th_combined", Some(&streams)).is_empty(),
            "both loopbacks feed a real sink, so neither is stale"
        );
        assert_eq!(
            loaded_branches(PACTL_MODULES, "blue2th_combined", Some(&streams)).len(),
            2,
            "and both are still counted as branches"
        );
    }

    // Criterion: an unreadable listing yields "cannot tell", not "everything is
    // dead". Unloading by id bypasses `reconcile_branches` entirely, so this guard
    // is the only thing standing between a transient `pactl` failure and every
    // loopback of a playing selection being torn down.
    #[test]
    fn test_dead_branch_modules_with_unknown_liveness_names_nothing() {
        assert!(
            dead_branch_modules(PACTL_MODULES, "blue2th_combined", None).is_empty(),
            "with liveness unknown no module may be unloaded"
        );
    }

    // Criterion: the ids are the ones `pactl unload-module` takes, i.e. the first
    // column of the module listing — not the position of the branch in the plan.
    // PipeWire hands out wide ids, and a 0-based index would happily unload
    // `module-device-restore`.
    #[test]
    fn test_dead_branch_modules_names_the_id_pactl_printed() {
        let listing = concat!(
            "10\tmodule-device-restore\t\n",
            "536870917\tmodule-loopback\tsource=blue2th_combined.monitor sink=bluez_output.80_99_E7_63_50_29.1 latency_msec=50 sink_dont_move=true\n",
        );
        let streams = vec![SinkInputStream {
            owner_module: 536_870_917,
            sink: INVALID_SINK_INDEX,
        }];

        assert_eq!(
            dead_branch_modules(listing, "blue2th_combined", Some(&streams)),
            vec![536_870_917],
            "the module id comes from the listing's first column"
        );
    }

    // Criterion: a reconciliation either leaves every branch alone or asks for the
    // whole plan — there is no third answer that loads a subset. `reconcile_combined`
    // relies on it when it drops the first load report on a confirming pass: the
    // rebuild that follows re-attempts everything that pass attempted.
    #[test]
    fn test_reconcile_branches_loads_all_of_the_plan_or_none_of_it() {
        let spec = two_speaker_spec();
        let one_loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: branch_latency_ms(0),
        }];
        let both_loaded = loaded_branches(PACTL_MODULES, &spec.sink_name, None);

        for loaded in [Vec::new(), one_loaded, both_loaded] {
            let plan = reconcile_branches(&loaded, &spec);
            assert!(
                plan.to_load.is_empty() || plan.to_load == spec.branches,
                "a partial load would leave a speaker to start on its own, got {:?}",
                plan.to_load
            );
        }
    }

    // Criterion: what `PactlGraph::branches` hands the router is every loaded
    // loopback with its id and a three-valued liveness. Module 27's stream feeds
    // a real sink and module 28's sits on the invalid index, so the first is
    // live and the second dead; with no sink-input listing both are unknown.
    // Pinned on the pure composition, since the trait method itself spawns.
    #[test]
    fn test_loaded_branches_with_liveness_reports_each_branch_as_live_dead_or_unknown() {
        let streams = parse_sink_inputs(PACTL_SINK_INPUTS);

        let read = loaded_branches_with_liveness(PACTL_MODULES, "blue2th_combined", Some(&streams));

        assert_eq!(
            read,
            vec![
                LoadedBranch {
                    id: 27,
                    branch: CombineBranch {
                        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
                        latency_ms: 50,
                    },
                    live: Some(true),
                },
                LoadedBranch {
                    id: 28,
                    branch: CombineBranch {
                        sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                        latency_ms: 300,
                    },
                    live: Some(false),
                },
            ],
            "one entry per loopback of the combined sink, with its module id and liveness"
        );

        let unknown = loaded_branches_with_liveness(PACTL_MODULES, "blue2th_combined", None);

        assert_eq!(
            unknown.iter().map(|b| b.live).collect::<Vec<_>>(),
            vec![None, None],
            "with no sink-input listing no branch is ruled dead, and none is ruled live"
        );
    }

    // Criterion: the teardown predicate never matches on an empty pattern —
    // every line contains the empty string, and what matches is unloaded. The
    // rule is pinned on the pure predicate because exercising `teardown("")`
    // against a real `pactl` would, on a regression, unload the developer's
    // entire module list.
    #[test]
    fn test_module_line_matches_never_matches_an_empty_pattern() {
        let lines: Vec<&str> = PACTL_MODULES.lines().collect();

        assert!(
            lines.iter().all(|line| !module_line_matches(line, &[""])),
            "an empty pattern matches no line"
        );
        assert!(
            lines.iter().all(|line| !module_line_matches(line, &[])),
            "no pattern at all matches no line"
        );
        assert!(
            lines
                .iter()
                .all(|line| !module_line_matches(line, &["blue2th_combined", ""])),
            "an empty pattern among real ones still matches nothing"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| module_line_matches(line, &["blue2th_combined"]))
                .count(),
            7,
            "a real pattern matches the lines that carry it: {lines:?}"
        );
    }
}
