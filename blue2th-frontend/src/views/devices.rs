// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use super::app::{
    protocol_message, use_backend_health, use_locale, BackendGone, BackendOnline, Route,
};
use super::spotify::{SpotifyLoginDialog, SpotifySource};
use super::status::{BackendStatus, SettingsButton};
use super::transport::TransportBar;
use crate::{backend, settings, timer, BLUETOOTH_ICON};
use dioxus::prelude::*;

/// Total number of bars drawn in the signal-strength icon.
const SIGNAL_BARS: u8 = 4;

/// Map an RSSI (dBm) to `(filled bars out of SIGNAL_BARS, colour)`. Stronger
/// signals fill more bars and trend green; weaker ones fewer bars and red.
fn signal_bars(rssi: i16) -> (u8, &'static str) {
    const GREEN: &str = "#22c55e";
    const ORANGE: &str = "#f59e0b";
    const RED: &str = "#ef4444";
    match rssi {
        r if r >= -55 => (4, GREEN),
        r if r >= -67 => (3, GREEN),
        r if r >= -78 => (2, ORANGE),
        _ => (1, RED),
    }
}

/// Signal-strength icon (like a Wi-Fi/network gauge): `SIGNAL_BARS` bars of
/// increasing height, the strongest `filled` of them coloured by intensity
/// (green → orange → red), the rest dimmed. `None` RSSI renders all bars dimmed.
#[component]
fn SignalBars(rssi: Option<i16>, #[props(default = false)] struck: bool) -> Element {
    let (filled, color) = match rssi {
        Some(r) => signal_bars(r),
        None => (0, ""),
    };
    let badge_class = if struck {
        "signal-badge struck"
    } else {
        "signal-badge"
    };
    rsx! {
        span { class: "{badge_class}",
            span { class: "signal-bars",
                for i in 1..=SIGNAL_BARS {
                    span {
                        class: "signal-bar",
                        style: if i <= filled { format!("background:{color};") } else { String::new() },
                    }
                }
            }
        }
    }
}

/// Merge the backend's device list into `found`: refresh the rows already shown
/// (keeping their order) and append the ones missing. Never clears, so a
/// transient backend hiccup cannot empty the list under the user.
fn merge_devices(
    found: &mut Signal<Vec<blue2th_proto::DeviceInfo>>,
    fetched: Vec<blue2th_proto::DeviceInfo>,
) {
    let mut list = found.write();
    for device in fetched {
        match list.iter().position(|d| d.address == device.address) {
            Some(i) => list[i] = device,
            None => list.push(device),
        }
    }
}

/// Order the scanned list: favourites first, then by signal strength.
///
/// A favourite is a device the backend is already bonded with (`paired`) — the
/// user's own speaker, as opposed to every stranger the radio picks up. Burying
/// it under a nearer unknown device is what makes the list unusable in a
/// crowded place, so pairing outranks signal; RSSI only breaks ties inside each
/// group, with an unknown RSSI last (`Reverse(None)` sorts after
/// `Reverse(Some(_))`). Stable, so two equal devices keep the order the scan
/// found them in.
fn sort_scanned(devices: &mut [blue2th_proto::DeviceInfo]) {
    devices.sort_by_key(|d| (std::cmp::Reverse(d.paired), std::cmp::Reverse(d.rssi)));
}

/// Whether a failed connect justifies marking the speaker **unavailable** (the
/// greyed row with struck signal bars). Pure, so the rule is testable outside a
/// Dioxus component.
///
/// A failed connect is the only reliable signal that a *paired* speaker is out
/// of reach. A refused Bluetooth pairing is not that: the speaker was simply not
/// in pairing mode, and the row must stay clickable so the user can retry (#52).
fn marks_unavailable(err: &backend::BackendError) -> bool {
    !err.is_pairing_failed()
}

/// Replace the device with `info`'s address in `found` with its updated state.
fn replace_device(
    found: &mut Signal<Vec<blue2th_proto::DeviceInfo>>,
    info: blue2th_proto::DeviceInfo,
) {
    let idx = found.read().iter().position(|d| d.address == info.address);
    if let Some(i) = idx {
        found.write()[i] = info;
    }
}

/// One row of the backend scan list: tap a disconnected device to connect it
/// (pair/trust/connect on the PC), or use the button to disconnect. `busy` holds
/// the address currently being acted on, so only one action runs at a time and
/// the active row shows a spinner.
#[component]
fn BackendDeviceItem(
    device: blue2th_proto::DeviceInfo,
    found: Signal<Vec<blue2th_proto::DeviceInfo>>,
    busy: Signal<Option<String>>,
    error: Signal<Option<String>>,
    unavailable: Signal<HashSet<String>>,
    targets: Signal<blue2th_proto::TargetsState>,
) -> Element {
    // An incompatible backend must refuse every device action, not fail on it
    // once the request is out (#33); an unpaired one likewise, since every
    // route but `/health` 401s until it is paired (#37).
    let actionable = settings::backend_actionable(use_backend_health());
    let addr = device.address.clone();
    let connected = device.connected;
    let rssi = device.rssi;
    // Bonded with the backend: one of the user's own devices, which the scanned
    // list both marks and floats to the top (see `sort_scanned`). Only there —
    // the pinned section holds exactly the connected devices, which are all
    // bonded, so a star on every row would mark nothing.
    let favourite = device.paired && !connected;
    let label = device
        .name
        .clone()
        .unwrap_or_else(|| device.address.clone());

    // Phase 4: is this speaker a selected playback target, and at what offset?
    let selected = targets.read().speakers.iter().any(|s| s.address == addr);
    let offset_ms = targets
        .read()
        .speakers
        .iter()
        .find(|s| s.address == addr)
        .map(|s| s.offset_ms)
        .unwrap_or(0);
    // Local slider value for smooth dragging; the backend is called on release
    // (`onchange`). Kept in step with the backend offset, except while dragging.
    let mut offset_draft = use_signal(|| offset_ms);
    let mut offset_dragging = use_signal(|| false);
    {
        let addr_sync = addr.clone();
        use_effect(move || {
            let backend_offset = targets
                .read()
                .speakers
                .iter()
                .find(|s| s.address == addr_sync)
                .map(|s| s.offset_ms)
                .unwrap_or(0);
            if !*offset_dragging.peek() {
                offset_draft.set(backend_offset);
            }
        });
    }

    let in_flight = busy().as_deref() == Some(addr.as_str());
    // A connected device is reachable by definition, so it is never "unavailable".
    let is_unavailable = !connected && unavailable.read().contains(addr.as_str());
    let (icon, icon_class) = if in_flight {
        ("⟳", "device-icon connecting")
    } else if connected {
        ("●", "device-icon connected")
    } else {
        ("○", "device-icon disconnected")
    };
    let row_class = if in_flight {
        "device-row active"
    } else if is_unavailable {
        "device-row unavailable"
    } else if !actionable {
        // Same dimmed, non-interactive look as an unreachable speaker: the row
        // cannot be acted on, and the banner above says why.
        "device-row blocked"
    } else {
        "device-row"
    };
    let disconnect_label = rust_i18n::t!("device.disconnect");

    rsx! {
        li {
            class: "{row_class}",
            onclick: {
                let addr = addr.clone();
                move |_| {
                    // Only connect an idle, available, disconnected device on a
                    // backend this app can still talk to.
                    if connected || is_unavailable || busy().is_some() || !actionable {
                        return;
                    }
                    let addr = addr.clone();
                    let mut busy = busy;
                    let mut found = found;
                    let mut error = error;
                    let mut unavailable = unavailable;
                    *busy.write() = Some(addr.clone());
                    spawn(async move {
                        match backend::connect_device(&addr).await {
                            Ok(info) => {
                                unavailable.write().remove(&addr);
                                replace_device(&mut found, info);
                            }
                            Err(e) => {
                                // A failed connect is the only reliable signal that a
                                // paired device is unreachable (powered off) — but a
                                // refused bond is not that, so the row stays clickable.
                                if marks_unavailable(&e) {
                                    unavailable.write().insert(addr.clone());
                                }
                                *error.write() = Some(if e.is_pairing_failed() {
                                    rust_i18n::t!("device.pairing_failed").to_string()
                                } else {
                                    e.to_string()
                                });
                            }
                        }
                        *busy.write() = None;
                    });
                }
            },
            // Corner mark, out of the flow: the row lays out identically
            // whether or not it carries the star.
            if favourite {
                span {
                    class: "device-favourite",
                    title: "{rust_i18n::t!(\"device.favourite\")}",
                    "★"
                }
            }
            span { class: "{icon_class}", "{icon}" }
            span { class: "device-name", "{label}" }
            SignalBars { rssi, struck: is_unavailable }
            if connected {
                div { class: "device-actions",
                    button {
                        class: if selected { "btn-target selected" } else { "btn-target" },
                        title: if selected { "{rust_i18n::t!(\"device.deselect_target\")}" } else { "{rust_i18n::t!(\"device.select_target\")}" },
                        aria_label: if selected { "{rust_i18n::t!(\"device.deselect_target\")}" } else { "{rust_i18n::t!(\"device.select_target\")}" },
                        onclick: {
                            let addr = addr.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                if !actionable {
                                    return;
                                }
                                let addr = addr.clone();
                                let mut targets = targets;
                                let mut error = error;
                                spawn(async move {
                                    let res = if selected {
                                        backend::deselect_target(&addr).await
                                    } else {
                                        backend::select_target(&addr).await
                                    };
                                    match res {
                                        Ok(state) => *targets.write() = state,
                                        Err(e) => *error.write() = Some(e.to_string()),
                                    }
                                });
                            }
                        },
                        span { class: "btn-target-icon", if selected { "✓" } else { "+" } }
                    }
                    span { class: "row-sep" }
                    button {
                        class: "btn-disconnect",
                        title: "{disconnect_label}",
                        aria_label: "{disconnect_label}",
                        onclick: {
                            let addr = addr.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                if busy().is_some() || !actionable {
                                    return;
                                }
                                let addr = addr.clone();
                                let mut busy = busy;
                                let mut found = found;
                                let mut error = error;
                                *busy.write() = Some(addr.clone());
                                spawn(async move {
                                    match backend::disconnect_device(&addr).await {
                                        Ok(info) => replace_device(&mut found, info),
                                        Err(e) => *error.write() = Some(e.to_string()),
                                    }
                                    *busy.write() = None;
                                });
                            }
                        },
                        // Feather "log-out" icon: a door with an arrow exiting it.
                        svg {
                            class: "btn-disconnect-icon",
                            view_box: "0 0 24 24",
                            width: "18",
                            height: "18",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            path { d: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" }
                            polyline { points: "16 17 21 12 16 7" }
                            line { x1: "21", y1: "12", x2: "9", y2: "12" }
                        }
                    }
                }
                // Per-speaker latency offset, shown only once the speaker is a
                // selected playback target (phase 4 sync tuning, 0..=750 ms).
                if selected {
                    div { class: "target-offset",
                        span { class: "target-offset-label",
                            "{rust_i18n::t!(\"device.offset\")}: {offset_draft} ms"
                        }
                        input {
                            class: "target-offset-slider",
                            r#type: "range",
                            min: "0",
                            max: "750",
                            step: "10",
                            value: "{offset_draft}",
                            // Not just the release handler: without this the
                            // thumb still slides under the finger and only the
                            // backend call is dropped, which reads as the app
                            // losing the value rather than refusing it (#33).
                            disabled: !actionable,
                            onpointerdown: move |e| e.stop_propagation(),
                            onclick: move |e| e.stop_propagation(),
                            oninput: move |e| {
                                *offset_dragging.write() = true;
                                if let Ok(v) = e.value().parse::<u32>() {
                                    *offset_draft.write() = v;
                                }
                            },
                            onchange: {
                                let addr = addr.clone();
                                move |e| {
                                    *offset_dragging.write() = false;
                                    if !actionable {
                                        return;
                                    }
                                    if let Ok(v) = e.value().parse::<u32>() {
                                        let addr = addr.clone();
                                        let mut targets = targets;
                                        let mut error = error;
                                        spawn(async move {
                                            match backend::set_offset(&addr, v).await {
                                                Ok(state) => *targets.write() = state,
                                                Err(e) => *error.write() = Some(e.to_string()),
                                            }
                                        });
                                    }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

/// Phase 1: trigger a scan on the PC backend and list the devices it discovers,
/// reusing the app's scan button + device-list styling. Self-contained so it does
/// not disturb the legacy Android path.
#[component]
pub(crate) fn BackendScan() -> Element {
    use_locale();

    let mut scanning = use_signal(|| false);
    let mut found: Signal<Vec<blue2th_proto::DeviceInfo>> = use_signal(Vec::new);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    // Address of the device currently connecting/disconnecting (one at a time).
    let busy: Signal<Option<String>> = use_signal(|| None);
    // Addresses found unreachable (a connect failed); cleared on a new scan.
    let mut unavailable: Signal<HashSet<String>> = use_signal(HashSet::new);

    // Transport (phase 3): current playback state. Fetched once on mount;
    // refreshed from each action's reply. The expand/collapse state lives inside
    // TransportBar so it resets to expanded each time the bar (re)appears.
    let mut playback: Signal<Option<blue2th_proto::PlaybackState>> = use_signal(|| None);
    use_hook(|| {
        spawn(async move {
            if let Ok(state) = backend::playback_state().await {
                *playback.write() = Some(state);
            }
        });
    });

    // Phase 4: the backend's playback-target selection (which speakers are picked
    // for fan-out, plus each one's latency offset). Fetched once on mount and kept
    // in step by each select/deselect/offset reply and the periodic refresh below.
    let mut targets: Signal<blue2th_proto::TargetsState> =
        use_signal(|| blue2th_proto::TargetsState {
            speakers: Vec::new(),
            routing: blue2th_proto::RoutingMode::Idle,
        });
    use_hook(|| {
        spawn(async move {
            if let Ok(state) = backend::fetch_targets().await {
                *targets.write() = state;
            }
        });
    });

    let backend_online = use_context::<BackendOnline>().0;
    // One classification for the banners, the scan button and every control
    // below, so they cannot disagree about the same backend (#33, #37).
    let health = use_backend_health();
    let actionable = settings::backend_actionable(health);
    // Load the backend's known devices on mount, and again each time it comes
    // back online. Nothing is cleared *here*: leaving the app (Spotify login in
    // the browser, or a restart) briefly flips the health probe to offline, and
    // wiping the list on that first miss is what used to force a manual "load
    // devices". Only the confirmed loss below clears, and this same effect is what
    // repopulates afterwards, with no user action.
    use_effect(move || {
        if !backend_online() {
            return;
        }
        let mut found = found;
        spawn(async move {
            if let Ok(devices) = backend::fetch_devices().await {
                merge_devices(&mut found, devices);
            }
        });
    });

    // The backend is gone: drop what it owned. The list, the playback state and
    // the target selection all describe *that* backend, and a list nobody can act
    // on reads as a working app. Deliberately not `merge_devices`, which never
    // removes anything so a refetch cannot empty the list under the user — this is
    // the separate, explicit act of a confirmed loss. Emptying the targets also
    // takes the transport bar away, since it only renders with a target.
    //
    // The unavailable marks go with the list, exactly as they do on a manual scan:
    // a connect attempted while the backend was dying fails, and a failed connect
    // is read as "this *speaker* is unreachable", which disables its row. Kept,
    // those marks would outlive the list and come back over the refetched devices
    // as rows only a manual scan could revive — the tap this change exists to
    // spare the user.
    let backend_gone = use_context::<BackendGone>().0;
    use_effect(move || {
        if !backend_gone() {
            return;
        }
        let mut found = found;
        let mut playback = playback;
        let mut targets = targets;
        let mut unavailable = unavailable;
        if !found.peek().is_empty() {
            found.write().clear();
        }
        if !unavailable.peek().is_empty() {
            unavailable.write().clear();
        }
        if playback.peek().is_some() {
            *playback.write() = None;
        }
        let idle = blue2th_proto::TargetsState {
            speakers: Vec::new(),
            routing: blue2th_proto::RoutingMode::Idle,
        };
        if *targets.peek() != idle {
            *targets.write() = idle;
        }
    });

    // Poll the playback state while online so the UI reflects changes made
    // outside the app: the tone ending on its own, and the volume being changed
    // on the speaker itself (AVRCP).
    use_hook(|| {
        let mut playback = playback;
        spawn(async move {
            loop {
                timer::sleep(std::time::Duration::from_secs(1)).await;
                if !*backend_online.peek() {
                    continue;
                }
                if let Ok(state) = backend::playback_state().await {
                    if playback.peek().as_ref() != Some(&state) {
                        *playback.write() = Some(state);
                    }
                }
            }
        });
    });

    // Poll the target selection while online so a speaker dropped server-side
    // (e.g. it disconnected externally, recomputing the routing mode) is reflected.
    use_hook(|| {
        let mut targets = targets;
        spawn(async move {
            loop {
                timer::sleep(std::time::Duration::from_secs(2)).await;
                if !*backend_online.peek() {
                    continue;
                }
                if let Ok(state) = backend::fetch_targets().await {
                    if *targets.peek() != state {
                        *targets.write() = state;
                    }
                }
            }
        });
    });

    // Auto-dismiss errors as a transient toast (legacy toast UX). This runs in the
    // stable BackendScan scope, so the timer is never cancelled by a row unmount.
    use_effect(move || {
        let mut error = error;
        if error.read().is_some() {
            spawn(async move {
                timer::sleep(std::time::Duration::from_secs(5)).await;
                *error.write() = None;
            });
        }
    });

    // Periodically refresh connected/rssi from the backend so a device that was
    // connected and powers off flips to disconnected on its own (no re-scan).
    use_hook(|| {
        let mut found = found;
        let mut unavailable = unavailable;
        spawn(async move {
            loop {
                timer::sleep(std::time::Duration::from_secs(3)).await;
                if found.peek().is_empty() {
                    continue;
                }
                let fetched = match backend::fetch_devices().await {
                    Ok(devices) => devices,
                    Err(_) => continue,
                };
                // Only write when something actually changed, to avoid needless renders.
                let changed = {
                    let current = found.peek();
                    fetched.iter().any(|d| {
                        current
                            .iter()
                            .find(|x| x.address == d.address)
                            .is_some_and(|x| x.connected != d.connected || x.rssi != d.rssi)
                    })
                };
                if changed {
                    let mut list = found.write();
                    for d in &fetched {
                        if let Some(entry) = list.iter_mut().find(|x| x.address == d.address) {
                            entry.connected = d.connected;
                            entry.rssi = d.rssi;
                        }
                    }
                }
                // A device that is now connected is reachable: drop any stale
                // unavailable mark so its row leaves the disabled state.
                let reconnected: Vec<&str> = fetched
                    .iter()
                    .filter(|d| d.connected)
                    .map(|d| d.address.as_str())
                    .collect();
                if !reconnected.is_empty()
                    && reconnected.iter().any(|a| unavailable.peek().contains(*a))
                {
                    let mut marks = unavailable.write();
                    for a in reconnected {
                        marks.remove(a);
                    }
                }
            }
        });
    });

    let btn_class = if scanning() {
        "btn-scan scanning"
    } else {
        "btn-scan"
    };
    let scan_icon = if scanning() { "⟳" } else { "⊙" };
    let scan_label = if scanning() {
        rust_i18n::t!("scan.scanning")
    } else {
        rust_i18n::t!("scan.button")
    };
    let empty_label = rust_i18n::t!("device.empty");
    // Permanent, unlike the dot's `title`: a phone has no hover, so the tooltip
    // alone left the user with a coloured dot and no explanation (#33, #37).
    let notice = settings::device_list_notice(health);
    let navigator = use_navigator();

    rsx! {
        div { class: "status-row",
            BackendStatus { error }
            SpotifySource { targets, error }
            SettingsButton {}
        }
        SpotifyLoginDialog { error }
        // Directly under the backend and Spotify cards, above the scan button:
        // it explains why everything below it is inert. Not inside the
        // empty-state card — an incompatible backend still serves `/devices`,
        // so the list is normally full and a message living there would never
        // be seen.
        match notice {
            // Nothing to tap: the fix is on the other machine, so this one
            // stays a plain `div` (#33).
            Some(settings::DeviceListNotice::Incompatible(mismatch)) => rsx! {
                div { class: "device-list-warning", "{protocol_message(mismatch)}" }
            },
            // Amber like the dot, and tappable: pairing is one screen away, so
            // the banner is the shortcut there (#37).
            Some(settings::DeviceListNotice::Unpaired) => rsx! {
                button {
                    class: "device-list-warning unpaired",
                    onclick: move |_| {
                        navigator.push(Route::AppSettingsPage {});
                    },
                    "{rust_i18n::t!(\"server.not_paired\")}"
                }
            },
            None => rsx! {},
        }
        button {
            class: "{btn_class}",
            // Incompatible as well as offline: a backend announcing a contract
            // this app does not speak may answer `/scan` with the right shape
            // and the wrong meaning (#33). Unpaired too: `/scan` would 401
            // (#37).
            disabled: scanning() || !actionable,
            onclick: move |_| async move {
                *error.write() = None;
                found.write().clear();
                unavailable.write().clear();
                *scanning.write() = true;
                match backend::scan_devices().await {
                    Ok(devices) => *found.write() = devices,
                    Err(e) => *error.write() = Some(e.to_string()),
                }
                *scanning.write() = false;
            },
            span { class: "scan-icon", "{scan_icon}" }
            "{scan_label}"
        }
        if let Some(e) = error() {
            div { class: "toast-error", "{e}" }
        }
        {
            let devices = found();
            let is_empty = devices.is_empty();
            // Connected devices are pinned in an always-visible section at the top.
            let (connected, mut others): (Vec<_>, Vec<_>) =
                devices.into_iter().partition(|d| d.connected);
            // Favourites first, then strongest signal (see `sort_scanned`).
            sort_scanned(&mut others);
            // A connected speaker is required for playback; the transport bar is
            // disabled otherwise.
            let has_speaker = !connected.is_empty();
            // At least one speaker must be *selected* as a target before `/play`
            // has somewhere to route audio (phase 4 explicit selection).
            let has_target = !targets().speakers.is_empty();
            rsx! {
                // Pinned speakers — always visible, never covered by the player.
                if has_speaker {
                    ul { class: "pinned-devices pinned-card",
                        for device in connected {
                            BackendDeviceItem {
                                key: "{device.address}",
                                device: device.clone(),
                                found,
                                busy,
                                error,
                                unavailable,
                                targets,
                            }
                        }
                    }
                }
                // Player stage: the scrollable device list with the transport bar
                // anchored to its bottom. Expanding the bar covers the list only.
                div { class: "player-stage",
                    div {
                        class: if has_target { "device-scroll has-player" } else { "device-scroll" },
                        if is_empty {
                            div { class: "device-list-empty",
                                img {
                                    class: "device-list-empty-icon",
                                    src: BLUETOOTH_ICON,
                                    alt: "",
                                }
                                p { class: "device-list-empty-text", "{empty_label}" }
                            }
                        } else {
                            ul { class: "device-list",
                                for device in others {
                                    BackendDeviceItem {
                                        key: "{device.address}",
                                        device: device.clone(),
                                        found,
                                        busy,
                                        error,
                                        unavailable,
                                        targets,
                                    }
                                }
                            }
                        }
                    }
                    // The player only appears once a speaker is connected (so it
                    // never shows before the backend/scan, nor with no target).
                    if has_target {
                        TransportBar { playback, has_target, error }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
