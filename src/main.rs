// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;

use dioxus::prelude::*;

mod backend;
mod bluetooth;

#[cfg(target_os = "android")]
use bluetooth::enable_bluetooth;
use bluetooth::{
    connect_device, connected_device_names, disconnect_device, request_enable_bluetooth,
    scan_devices,
};

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_LOGO: Asset = asset!("/assets/bluetooth.svg");

const MAX_CONNECTIONS: usize = 2;

/// Vertical travel (px) past which a drag on the transport handle is treated as
/// an expand/collapse gesture rather than a tap.
const TRANSPORT_DRAG_THRESHOLD_PX: f64 = 24.0;

/// Temporarily hide the legacy on-phone Bluetooth UI (scan button + device list)
/// while the app transitions to driving the PC backend. The code path is kept
/// intact for the future on-phone LE Audio feature (see docs/ROADMAP.md).
const SHOW_LEGACY_BT_UI: bool = false;

/// How often the app re-checks the PC backend's reachability.
const BACKEND_HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the PC backend is currently reachable, shared via context. A newtype
/// (not a bare `Signal<bool>`) so it does not collide with `bt_enabled`, which is
/// also a `Signal<bool>` in context.
#[derive(Clone, Copy)]
struct BackendOnline(Signal<bool>);

#[derive(Clone, Debug, PartialEq)]
enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
}

/// Pure helper: map found device names plus the set of currently-connected names
/// into `(name, ConnectionStatus)` pairs. A name present in `connected` becomes
/// `Connected`; otherwise `Disconnected`. Order follows `found`, with no duplicates.
#[cfg_attr(not(test), allow(dead_code))]
fn merge_connection_status(
    found: Vec<String>,
    connected: &[String],
) -> Vec<(String, ConnectionStatus)> {
    let mut result: Vec<(String, ConnectionStatus)> = Vec::new();
    for name in found {
        let status = if connected.contains(&name) {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Disconnected
        };
        if let Some(entry) = result.iter_mut().find(|(n, _)| *n == name) {
            // Refresh the status of an already-present device (no duplicate entry).
            entry.1 = status;
        } else {
            result.push((name, status));
        }
    }
    result
}

/// Pure helper: reconcile each listed device's status against the set of names
/// reported as currently connected, in place. A device whose name is in
/// `connected` becomes `Connected`; otherwise `Disconnected`. An in-flight
/// `Connecting` entry is always preserved so a background reconcile never clobbers
/// a connection attempt the user just started.
///
/// Shared by both Android reconcile sites (the post-scan background task and the
/// 2 s polling loop) so the two stay behaviorally identical.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn reconcile_connection_status(devices: &mut [(String, ConnectionStatus)], connected: &[String]) {
    for (name, status) in devices.iter_mut() {
        if *status == ConnectionStatus::Connecting {
            continue;
        }
        *status = if connected.iter().any(|c| c == name) {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Disconnected
        };
    }
}

fn status_icon(status: &ConnectionStatus) -> (&'static str, &'static str) {
    match status {
        ConnectionStatus::Disconnected => ("○", "device-icon disconnected"),
        ConnectionStatus::Connecting => ("⟳", "device-icon connecting"),
        ConnectionStatus::Connected => ("●", "device-icon connected"),
    }
}

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

// Reads the locale from context, sets the global rust-i18n locale, and subscribes
// the calling component to locale changes so it re-renders when the locale changes.
fn use_locale() {
    let locale = use_context::<Signal<String>>();
    rust_i18n::set_locale(&locale());
}

#[derive(Routable, Clone, PartialEq)]
enum Route {
    #[route("/")]
    Home {},
    #[route("/device/:name")]
    DeviceSettings { name: String },
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let devices: Signal<Vec<(String, ConnectionStatus)>> = use_signal(Vec::new);
    use_context_provider(|| devices);

    // bt_enabled is global so it survives navigation between Home and DeviceSettings.
    let bt_enabled: Signal<bool> = use_signal(|| false);
    use_context_provider(|| bt_enabled);

    // Periodically probe the PC backend so the whole app knows whether it is
    // reachable. Shared via context: Home shows a status dot, BackendScan gates
    // its scan button and clears its list when the backend goes down.
    let backend_online: Signal<bool> = use_signal(|| false);
    use_context_provider(|| BackendOnline(backend_online));
    use_hook(|| {
        // Signal<bool> is Copy; the spawned task captures its own handle.
        let mut backend_online = backend_online;
        spawn(async move {
            loop {
                let reachable = backend::ping_backend().await.is_ok();
                if *backend_online.peek() != reachable {
                    *backend_online.write() = reachable;
                }
                tokio::time::sleep(BACKEND_HEALTH_INTERVAL).await;
            }
        });
    });

    // On Android, keep bt_enabled in sync with the real adapter state.
    // The first iteration runs immediately (startup check, no initial sleep) so the correct
    // button is shown without waiting; subsequent iterations catch external enable/disable events.
    #[cfg(target_os = "android")]
    use_hook(|| {
        // Clone is required here because each spawned task needs its own captured copy of
        // the Signal handle; Signal<bool> is Copy so this is a bitwise copy, not allocation.
        let mut bt_enabled = bt_enabled;
        spawn(async move {
            loop {
                if let Ok(state) = enable_bluetooth().await {
                    if *bt_enabled.peek() != state {
                        *bt_enabled.write() = state;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    });

    // On Android, poll every 2 s to keep each device's status in sync with the
    // real connection state in BOTH directions: a device connected externally
    // (e.g. via Android settings) becomes Connected, and one that drops its link
    // becomes Disconnected — all without requiring a manual re-scan.
    #[cfg(target_os = "android")]
    use_hook(|| {
        // Clone is required: the spawned async block needs its own Signal handle.
        let mut devices = devices;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // Nothing to reconcile until devices have been loaded.
                if devices.read().is_empty() {
                    continue;
                }
                let connected = connected_device_names().await.unwrap_or_default();
                let mut d = devices.write();
                reconcile_connection_status(&mut d, &connected);
            }
        });
    });

    let locale: Signal<String> = use_signal(|| "fr".to_string());
    use_context_provider(|| locale);
    rust_i18n::set_locale(&locale());

    rsx! {
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Link { rel: "stylesheet", href: TAILWIND_CSS }
        Router::<Route> {}
    }
}

#[component]
fn Home() -> Element {
    use_locale();

    let mut devices = use_context::<Signal<Vec<(String, ConnectionStatus)>>>();
    // mut is only exercised on non-Android (where Ok(()) sets bt_enabled = true);
    // on Android the system dialog owns the state transition.
    #[cfg_attr(target_os = "android", allow(unused_mut))]
    let mut bt_enabled = use_context::<Signal<bool>>();
    let mut scanning = use_signal(|| false);
    let mut bt_error: Signal<Option<String>> = use_signal(|| None);
    let toast_error: Signal<Option<String>> = use_signal(|| None);

    // When BT is disabled, clear the device list.
    use_effect(move || {
        if !bt_enabled() {
            devices.write().clear();
        }
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

    rsx! {
        div {
            class: "home",
            div {
                class: "app-header",
                h1 { class: "app-title",
                    span { class: "app-title-text tl tl-1", "B" }
                    span { class: "app-title-text tl tl-2", "l" }
                    span { class: "app-title-text tl tl-3", "u" }
                    span { class: "app-title-text tl tl-4", "e" }
                    span { class: "app-title-num", "2" }
                    span { class: "app-title-text app-title-suffix", "th" }
                }
                img {
                    class: "app-logo",
                    src: BLUETOOTH_LOGO,
                    alt: "Bluetooth",
                }
            }
            BackendScan {}
            if SHOW_LEGACY_BT_UI {
            if let Some(err) = bt_error() {
                div {
                    class: "bt-error-banner",
                    span { "{err}" }
                    button {
                        class: "bt-error-dismiss",
                        onclick: move |_| *bt_error.write() = None,
                        "×"
                    }
                }
            }
            if bt_enabled() {
                button {
                    class: "{btn_class}",
                    disabled: scanning(),
                    onclick: move |_| async move {
                        *scanning.write() = true;
                        let found = scan_devices().await.unwrap_or_default();
                        // Populate the list immediately. The real A2DP connection
                        // status is reconciled in the background below, because
                        // obtaining the A2DP proxy can block for several seconds
                        // and must never gate the list from appearing.
                        {
                            let mut d = devices.write();
                            for name in found {
                                if !d.iter().any(|(n, _)| n == &name) {
                                    d.push((name, ConnectionStatus::Disconnected));
                                }
                            }
                        }
                        *scanning.write() = false;
                        // Reflect the real connection state without blocking the
                        // UI thread: querying it can take a moment on Android.
                        spawn(async move {
                            let connected =
                                connected_device_names().await.unwrap_or_default();
                            let mut d = devices.write();
                            reconcile_connection_status(&mut d, &connected);
                        });
                    },
                    span { class: "scan-icon", "{scan_icon}" }
                    "{scan_label}"
                }
            } else {
                button {
                    class: "btn-enable-bt",
                    onclick: move |_| async move {
                        match request_enable_bluetooth().await {
                            Ok(()) => {
                                #[cfg(not(target_os = "android"))]
                                { *bt_enabled.write() = true; }
                                // On Android the system dialog is fire-and-forget.
                                // Poll the adapter state every 500ms until BT is on (max 15s).
                                #[cfg(target_os = "android")]
                                spawn(async move {
                                    for _ in 0..30u8 {
                                        tokio::time::sleep(
                                            std::time::Duration::from_millis(500),
                                        )
                                        .await;
                                        if let Ok(true) = enable_bluetooth().await {
                                            *bt_enabled.write() = true;
                                            break;
                                        }
                                    }
                                });
                            }
                            Err(e) => *bt_error.write() = Some(e.to_string()),
                        }
                    },
                    span { "⚡" }
                    "{rust_i18n::t!(\"bt.enable\")}"
                }
            }
            {
                let (connected, others): (Vec<_>, Vec<_>) = devices()
                    .into_iter()
                    .partition(|(_, s)| *s == ConnectionStatus::Connected);
                let connected_count = connected.len();
                let is_empty = connected.is_empty() && others.is_empty();
                rsx! {
                    div { class: "device-list-container",
                        div { class: "device-list-wrapper",
                            if is_empty {
                                div { class: "device-list-empty",
                                    img {
                                        class: "device-list-empty-icon",
                                        src: BLUETOOTH_LOGO,
                                        alt: "",
                                    }
                                    p { class: "device-list-empty-text", "{empty_label}" }
                                }
                            } else {
                                if !connected.is_empty() {
                                    ul { class: "pinned-devices",
                                        for (name, status) in connected {
                                            DeviceItem { key: "{name}", name, status, devices, toast_error }
                                        }
                                    }
                                }
                                ul { class: "device-list",
                                    for (name, status) in others {
                                        DeviceItem { key: "{name}", name, status, devices, toast_error }
                                    }
                                }
                            }
                        }
                        // Connection counter: floating badge on the top-right border of the list.
                        if !is_empty {
                            div { class: "connected-counter",
                                "{rust_i18n::t!(\"device.connected_count\", count = connected_count.to_string().as_str())}"
                            }
                        }
                    }
                }
            }
            if let Some(err) = toast_error() {
                div { class: "toast-error", "{err}" }
            }
            }
        }
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
    let addr = device.address.clone();
    let connected = device.connected;
    let rssi = device.rssi;
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
                    // Only connect an idle, available, disconnected device.
                    if connected || is_unavailable || busy().is_some() {
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
                                // paired device is unreachable (powered off).
                                unavailable.write().insert(addr.clone());
                                *error.write() = Some(e.to_string());
                            }
                        }
                        *busy.write() = None;
                    });
                }
            },
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
                                if busy().is_some() {
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
fn BackendScan() -> Element {
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
    // Drop stale scan results as soon as the backend becomes unreachable.
    use_effect(move || {
        let mut found = found;
        if !backend_online() {
            found.write().clear();
        }
    });

    // Poll the playback state while online so the UI reflects changes made
    // outside the app: the tone ending on its own, and the volume being changed
    // on the speaker itself (AVRCP).
    use_hook(|| {
        let mut playback = playback;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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

    // Server-reachability encart (label + green/red dot), paired with the Spotify
    // toggle in a single status row above the scan button.
    let server_label = rust_i18n::t!("server.status");
    let server_tooltip = if backend_online() {
        rust_i18n::t!("server.online")
    } else {
        rust_i18n::t!("server.offline")
    };

    rsx! {
        div { class: "status-row",
            div { class: "backend-status",
                span { class: "backend-status-label", "{server_label}" }
                span { class: "backend-status-sep" }
                span {
                    class: "backend-status-dot",
                    title: "{server_tooltip}",
                    style: format!(
                        "display:inline-block;width:12px;height:12px;border-radius:50%;background:{};",
                        if backend_online() { "#22c55e" } else { "#ef4444" },
                    ),
                }
            }
            SpotifySource { targets, error }
        }
        SpotifyConnect { error }
        button {
            class: "{btn_class}",
            disabled: scanning() || !backend_online(),
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
            // Sort the rest by signal strength: strongest (greenest) first, with
            // unknown RSSI last (Reverse(None) sorts after Reverse(Some(_))).
            others.sort_by_key(|d| std::cmp::Reverse(d.rssi));
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
                                    src: BLUETOOTH_LOGO,
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

/// Bottom transport bar (phase 3): play/pause, stop, and a PipeWire-sink volume
/// slider. Collapsed it is a thin bar at the bottom of the player stage; expanded
/// it covers the device list (but never the pinned speakers above it). Controls
/// are disabled until a speaker is connected, since `/play` needs a target sink.
#[component]
fn TransportBar(
    playback: Signal<Option<blue2th_proto::PlaybackState>>,
    has_target: bool,
    error: Signal<Option<String>>,
) -> Element {
    use blue2th_proto::PlaybackStatus;
    use_locale();

    // Expand/collapse state is local so the bar (re)appears expanded each time it
    // is mounted; the user can still collapse it while it is shown.
    let mut expanded = use_signal(|| true);

    let status = playback()
        .map(|p| p.status)
        .unwrap_or(PlaybackStatus::Stopped);
    let volume = playback().map(|p| p.volume).unwrap_or(1.0);
    let is_playing = status == PlaybackStatus::Playing;

    // Local slider value for smooth dragging; the backend is called on release
    // (`onchange`) rather than on every tick (`oninput`).
    let mut vol_draft = use_signal(|| volume);
    // True while the user drags, so the polled backend volume does not fight the
    // thumb under the finger.
    let mut dragging = use_signal(|| false);
    // Keep the slider in step with the backend volume (e.g. changed on the
    // speaker itself), except while the user is actively dragging.
    use_effect(move || {
        if let Some(state) = playback() {
            if !*dragging.peek() {
                vol_draft.set(state.volume);
            }
        }
    });
    let vol_pct = (vol_draft() * 100.0).round() as i32;

    // Y where a drag on the handle began, to tell an expand/collapse swipe from a
    // tap on pointer release.
    let mut drag_start_y = use_signal(|| Option::<f64>::None);
    // Drives the press ripple via a signal (not CSS :active, which sticks on the
    // Android WebView), matching the device-row pattern.
    let mut handle_active = use_signal(|| false);
    // Y where a drag anywhere on the bar began (separate from the handle's, since
    // both can receive the bubbled pointer events).
    let mut bar_drag_y = use_signal(|| Option::<f64>::None);

    let bar_class = if expanded() {
        "transport-bar expanded"
    } else {
        "transport-bar"
    };
    let toggle_icon = if expanded() { "⌄" } else { "⌃" };
    let toggle_label = if expanded() {
        rust_i18n::t!("transport.collapse")
    } else {
        rust_i18n::t!("transport.expand")
    };
    let (play_icon, play_label) = if is_playing {
        ("⏸", rust_i18n::t!("transport.pause"))
    } else {
        ("▶", rust_i18n::t!("transport.play"))
    };
    let stop_label = rust_i18n::t!("transport.stop");
    let status_label = match status {
        PlaybackStatus::Playing => rust_i18n::t!("transport.status_playing"),
        PlaybackStatus::Paused => rust_i18n::t!("transport.status_paused"),
        PlaybackStatus::Stopped => rust_i18n::t!("transport.status_stopped"),
    };

    rsx! {
        div {
            class: "{bar_class}",
            // Drag up/down anywhere on the bar expands/collapses it (the handle
            // still toggles on tap). Children that need their own gesture (the
            // volume slider) stop propagation so they never trigger this.
            onpointerdown: move |e| {
                *bar_drag_y.write() = Some(e.client_coordinates().y);
            },
            onpointermove: move |e| {
                // Trigger as soon as the threshold is crossed (more reliable than
                // waiting for pointerup, which a cancelled touch may skip).
                let Some(start_y) = *bar_drag_y.peek() else {
                    return;
                };
                let delta = e.client_coordinates().y - start_y;
                if delta < -TRANSPORT_DRAG_THRESHOLD_PX {
                    *expanded.write() = true;
                    *bar_drag_y.write() = None;
                } else if delta > TRANSPORT_DRAG_THRESHOLD_PX {
                    *expanded.write() = false;
                    *bar_drag_y.write() = None;
                }
            },
            onpointerup: move |_| {
                *bar_drag_y.write() = None;
            },
            button {
                class: if handle_active() { "transport-handle active" } else { "transport-handle" },
                title: "{toggle_label}",
                aria_label: "{toggle_label}",
                onpointerdown: move |e| {
                    *drag_start_y.write() = Some(e.client_coordinates().y);
                    // Fire the ripple and clear it after the animation so a quick
                    // tap still plays it fully.
                    *handle_active.write() = true;
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
                        *handle_active.write() = false;
                    });
                },
                onpointerup: move |e| {
                    let Some(start_y) = drag_start_y.write().take() else {
                        return;
                    };
                    let delta = e.client_coordinates().y - start_y;
                    // Drags are owned by the bar-level handler; the handle only
                    // toggles on a short tap.
                    if delta.abs() <= TRANSPORT_DRAG_THRESHOLD_PX {
                        let now = expanded();
                        *expanded.write() = !now;
                    }
                },
                span { class: "transport-handle-icon", "{toggle_icon}" }
            }
            if expanded() {
                div { class: "transport-meta",
                    div { class: "transport-title", "{rust_i18n::t!(\"transport.title\")}" }
                    div { class: "transport-track", "🎵 {rust_i18n::t!(\"transport.track\")}" }
                    div { class: "transport-status", "{status_label}" }
                }
            }
            div { class: "transport-controls",
                button {
                    class: "transport-btn transport-play",
                    disabled: !has_target,
                    title: "{play_label}",
                    aria_label: "{play_label}",
                    onclick: move |_| {
                        if !has_target {
                            return;
                        }
                        let mut playback = playback;
                        let mut error = error;
                        spawn(async move {
                            let res = if is_playing {
                                backend::pause().await
                            } else {
                                backend::play().await
                            };
                            match res {
                                Ok(state) => *playback.write() = Some(state),
                                Err(e) => *error.write() = Some(e.to_string()),
                            }
                        });
                    },
                    span { "{play_icon}" }
                }
                button {
                    class: "transport-btn transport-stop",
                    disabled: !has_target,
                    title: "{stop_label}",
                    aria_label: "{stop_label}",
                    onclick: move |_| {
                        if !has_target {
                            return;
                        }
                        let mut playback = playback;
                        let mut error = error;
                        spawn(async move {
                            match backend::stop().await {
                                Ok(state) => *playback.write() = Some(state),
                                Err(e) => *error.write() = Some(e.to_string()),
                            }
                        });
                    },
                    span { "⏹" }
                }
                div { class: "transport-volume",
                    span { class: "transport-volume-icon", "🔊" }
                    input {
                        class: "transport-volume-slider",
                        r#type: "range",
                        min: "0",
                        max: "1",
                        step: "0.01",
                        value: "{vol_draft}",
                        disabled: !has_target,
                        // Keep slider drags from bubbling to the bar's expand/
                        // collapse gesture.
                        onpointerdown: move |e| e.stop_propagation(),
                        oninput: move |e| {
                            *dragging.write() = true;
                            if let Ok(v) = e.value().parse::<f32>() {
                                *vol_draft.write() = v;
                            }
                        },
                        onchange: move |e| {
                            *dragging.write() = false;
                            if !has_target {
                                return;
                            }
                            if let Ok(v) = e.value().parse::<f32>() {
                                let mut playback = playback;
                                let mut error = error;
                                spawn(async move {
                                    match backend::set_volume(v).await {
                                        Ok(state) => *playback.write() = Some(state),
                                        Err(e) => *error.write() = Some(e.to_string()),
                                    }
                                });
                            }
                        },
                    }
                    if expanded() {
                        span { class: "transport-volume-pct", "{vol_pct}%" }
                    }
                }
            }
            if !has_target {
                div { class: "transport-hint", "{rust_i18n::t!(\"transport.no_target\")}" }
            }
        }
    }
}

/// Spotify source control (phase 5.1): activate/deactivate the `librespot`
/// backend on the PC. Streaming and transport are driven by the official Spotify
/// app (pick `blue2th-PC` as the device); this only toggles the Connect backend
/// and shows its state. The start action is disabled with no target selected,
/// mirroring the server's 400 precondition, and errors surface via the shared
/// `error` toast signal.
#[component]
fn SpotifySource(
    targets: Signal<blue2th_proto::TargetsState>,
    error: Signal<Option<String>>,
) -> Element {
    use blue2th_proto::SpotifyStatus;
    use_locale();

    // Current backend state: fetched once on mount, refreshed from each toggle's
    // reply and the periodic poll below (the subprocess can die on its own
    // server-side, so we reconcile rather than trust the last action).
    let mut spotify: Signal<Option<blue2th_proto::SpotifyState>> = use_signal(|| None);
    use_hook(|| {
        spawn(async move {
            if let Ok(state) = backend::spotify_status().await {
                *spotify.write() = Some(state);
            }
        });
    });

    let backend_online = use_context::<BackendOnline>().0;
    use_hook(|| {
        let mut spotify = spotify;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if !*backend_online.peek() {
                    continue;
                }
                if let Ok(state) = backend::spotify_status().await {
                    if spotify.peek().as_ref() != Some(&state) {
                        *spotify.write() = Some(state);
                    }
                }
            }
        });
    });

    // In-flight guard so a double tap does not fire two start/stop calls.
    let busy = use_signal(|| false);

    let is_running = spotify()
        .map(|s| s.status == SpotifyStatus::Running)
        .unwrap_or(false);
    let has_target = !targets().speakers.is_empty();

    // Tooltip mirrors the action the click will perform (or the in-flight state).
    let toggle_tooltip = if busy() {
        rust_i18n::t!("spotify.working")
    } else if is_running {
        rust_i18n::t!("spotify.stop")
    } else {
        rust_i18n::t!("spotify.start")
    };
    // Dim (and block) the encart while offline or mid-flight; and when starting,
    // until a speaker is selected (the server rejects a start with no target — a 400).
    let disabled = busy() || !backend_online() || (!is_running && !has_target);
    let card_class = if disabled {
        "backend-status spotify-card disabled"
    } else {
        "backend-status spotify-card"
    };

    rsx! {
        // Compact status encart matching the server one: "Spotify | ●".
        // The whole card is the toggle — green dot ⇒ running, red ⇒ stopped.
        div {
            class: "{card_class}",
            title: "{toggle_tooltip}",
            onclick: move |_| {
                if busy() || !backend_online() {
                    return;
                }
                // Guard the start precondition client-side too, so the user gets
                // the message without a round-trip to a 400.
                if !is_running && !has_target {
                    *error.write() = Some(rust_i18n::t!("spotify.no_target").to_string());
                    return;
                }
                let mut spotify = spotify;
                let mut error = error;
                let mut busy = busy;
                *busy.write() = true;
                spawn(async move {
                    let res = if is_running {
                        backend::stop_spotify().await
                    } else {
                        backend::start_spotify().await
                    };
                    match res {
                        Ok(state) => *spotify.write() = Some(state),
                        Err(e) => *error.write() = Some(e.to_string()),
                    }
                    *busy.write() = false;
                });
            },
            span { class: "backend-status-label", "{rust_i18n::t!(\"spotify.title\")}" }
            span { class: "backend-status-sep" }
            span {
                class: "backend-status-dot",
                style: format!(
                    "display:inline-block;width:12px;height:12px;border-radius:50%;background:{};",
                    if is_running { "#22c55e" } else { "#ef4444" },
                ),
            }
        }
    }
}

/// Spotify Web API control (phase 5.2): OAuth (PKCE) login plus now-playing and
/// transport over the PC backend. When Disconnected it offers a "Connect Spotify"
/// action that fetches the authorize URL to open in the browser; when Connected it
/// shows the now-playing track (pushed over SSE) and play/pause/next/previous
/// controls. Errors surface via the shared `error` toast signal.
#[component]
fn SpotifyConnect(error: Signal<Option<String>>) -> Element {
    use blue2th_proto::{NowPlayingState, SpotifyAuthStatus};
    use_locale();

    let backend_online = use_context::<BackendOnline>().0;

    // Current auth state: fetched on mount and refreshed by the poll below.
    let mut auth: Signal<Option<blue2th_proto::SpotifyAuthState>> = use_signal(|| None);
    use_hook(|| {
        spawn(async move {
            if let Ok(state) = backend::spotify_auth_status().await {
                *auth.write() = Some(state);
            }
        });
    });
    use_hook(|| {
        let mut auth = auth;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if !*backend_online.peek() {
                    continue;
                }
                if let Ok(state) = backend::spotify_auth_status().await {
                    if auth.peek().as_ref() != Some(&state) {
                        *auth.write() = Some(state);
                    }
                }
            }
        });
    });

    // Now-playing snapshot pushed over the SSE feed; (re)subscribes if the stream
    // ends. The task lives on the root scope so it is not cancelled on re-render.
    let now_playing: Signal<Option<blue2th_proto::NowPlaying>> = use_signal(|| None);
    use_hook(|| {
        let mut now_playing = now_playing;
        spawn(async move {
            loop {
                let _ = backend::subscribe_now_playing(|np| {
                    *now_playing.write() = Some(np);
                })
                .await;
                // The stream closed (backend down or restarted); retry shortly.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    });

    // The authorize URL to open in the browser once "Connect Spotify" is tapped.
    let auth_url = use_signal(|| Option::<String>::None);
    let busy = use_signal(|| false);

    let connected = auth()
        .map(|s| s.status == SpotifyAuthStatus::Connected)
        .unwrap_or(false);

    if !backend_online() {
        return rsx! {};
    }

    if !connected {
        return rsx! {
            div { class: "spotify-connect",
                button {
                    class: "btn-spotify-connect",
                    disabled: busy(),
                    onclick: move |_| {
                        let mut busy = busy;
                        let mut error = error;
                        let mut auth_url = auth_url;
                        *busy.write() = true;
                        spawn(async move {
                            match backend::spotify_auth_url().await {
                                Ok(resp) => *auth_url.write() = Some(resp.url),
                                Err(e) => *error.write() = Some(e.to_string()),
                            }
                            *busy.write() = false;
                        });
                    },
                    span { "🎧 " }
                    "{rust_i18n::t!(\"spotify.connect\")}"
                }
                if let Some(url) = auth_url() {
                    a {
                        class: "spotify-auth-link",
                        href: "{url}",
                        "{rust_i18n::t!(\"spotify.open_login\")}"
                    }
                }
            }
        };
    }

    let (title, subtitle) = match now_playing() {
        Some(np) if np.state != NowPlayingState::Idle => (
            np.title
                .unwrap_or_else(|| rust_i18n::t!("spotify.unknown_track").to_string()),
            np.artist.unwrap_or_default(),
        ),
        _ => (
            rust_i18n::t!("spotify.nothing_playing").to_string(),
            String::new(),
        ),
    };

    rsx! {
        div { class: "spotify-connect connected",
            div { class: "spotify-now-playing",
                span { class: "spotify-np-title", "{title}" }
                span { class: "spotify-np-artist", "{subtitle}" }
            }
            div { class: "spotify-transport",
                button {
                    class: "spotify-transport-btn",
                    title: "{rust_i18n::t!(\"spotify.previous\")}",
                    aria_label: "{rust_i18n::t!(\"spotify.previous\")}",
                    onclick: move |_| {
                        let mut error = error;
                        spawn(async move {
                            if let Err(e) = backend::spotify_previous().await {
                                *error.write() = Some(e.to_string());
                            }
                        });
                    },
                    "⏮"
                }
                button {
                    class: "spotify-transport-btn",
                    title: "{rust_i18n::t!(\"spotify.play\")}",
                    aria_label: "{rust_i18n::t!(\"spotify.play\")}",
                    onclick: move |_| {
                        let mut error = error;
                        spawn(async move {
                            if let Err(e) = backend::spotify_play().await {
                                *error.write() = Some(e.to_string());
                            }
                        });
                    },
                    "▶"
                }
                button {
                    class: "spotify-transport-btn",
                    title: "{rust_i18n::t!(\"spotify.pause\")}",
                    aria_label: "{rust_i18n::t!(\"spotify.pause\")}",
                    onclick: move |_| {
                        let mut error = error;
                        spawn(async move {
                            if let Err(e) = backend::spotify_pause().await {
                                *error.write() = Some(e.to_string());
                            }
                        });
                    },
                    "⏸"
                }
                button {
                    class: "spotify-transport-btn",
                    title: "{rust_i18n::t!(\"spotify.next\")}",
                    aria_label: "{rust_i18n::t!(\"spotify.next\")}",
                    onclick: move |_| {
                        let mut error = error;
                        spawn(async move {
                            if let Err(e) = backend::spotify_next().await {
                                *error.write() = Some(e.to_string());
                            }
                        });
                    },
                    "⏭"
                }
            }
        }
    }
}

#[component]
fn DeviceItem(
    name: String,
    status: ConnectionStatus,
    devices: Signal<Vec<(String, ConnectionStatus)>>,
    toast_error: Signal<Option<String>>,
) -> Element {
    use_locale();

    let navigator = use_navigator();
    let (icon, icon_class) = status_icon(&status);
    let is_connected = status == ConnectionStatus::Connected;
    let disconnect_label = rust_i18n::t!("device.disconnect");

    // Controls the ripple highlight via a Rust signal instead of CSS :active, which gets
    // stuck on Android WebView when the DOM is mutated during the touch event.
    let mut row_active = use_signal(|| false);

    rsx! {
        li {
            class: if row_active() { "device-row active" } else { "device-row" },
            onclick: {
                // Clone is required: the closure must own `name` because it outlives the render frame.
                let name = name.clone();
                move |_| {
                    // Clone is required: `spawn` captures a `'static` async block.
                    let name = name.clone();
                    // Guards run synchronously — no await, no UI blocking.
                    let current = devices
                        .read()
                        .iter()
                        .find(|(n, _)| n == &name)
                        .map(|(_, s)| s.clone());
                    if !matches!(current, Some(ConnectionStatus::Disconnected)) {
                        return;
                    }
                    let connected_count = devices
                        .read()
                        .iter()
                        .filter(|(_, s)| *s == ConnectionStatus::Connected)
                        .count();
                    if connected_count >= MAX_CONNECTIONS {
                        return;
                    }
                    // Show the spinner and ripple immediately before handing off to the background task.
                    *row_active.write() = true;
                    let idx = devices.read().iter().position(|(n, _)| n == &name);
                    if let Some(i) = idx {
                        devices.write()[i].1 = ConnectionStatus::Connecting;
                    }
                    // BT operation runs on a background task — UI thread stays free.
                    spawn(async move {
                        match connect_device(name.clone()).await {
                            Ok(true) => {
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Connected;
                                }
                                *row_active.write() = false;
                            }
                            Ok(false) => {
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Disconnected;
                                }
                                *row_active.write() = false;
                            }
                            Err(_e) => {
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Disconnected;
                                }
                                *toast_error.write() = Some(
                                    rust_i18n::t!("device.connect_failed", name = name.as_str())
                                        .into_owned(),
                                );
                                // Clear both the toast and the ripple at the same moment.
                                spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    *toast_error.write() = None;
                                    *row_active.write() = false;
                                });
                            }
                        }
                    });
                }
            },
            span { class: "{icon_class}", "{icon}" }
            span { class: "device-name", "{name}" }
            if is_connected {
                div { class: "device-actions",
                    button {
                        class: "btn-disconnect",
                        title: "{disconnect_label}",
                        aria_label: "{disconnect_label}",
                        onclick: {
                            // Clone is required: same reason as the connect closure above.
                            let name = name.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                // Clone is required: the async block captures a `'static` copy.
                                let name = name.clone();
                                // Setting Connecting moves this device out of the "connected"
                                // (pinned) list into the main list, which UNMOUNTS this
                                // DeviceItem and recreates it under the other `<ul>`. A task
                                // spawned with `spawn` is tied to this scope and would be
                                // cancelled by that unmount before it ever ran. `spawn_forever`
                                // attaches the task to the root scope so it survives, and the
                                // task only touches `devices`/`toast_error` (owned by an
                                // ancestor) — never this component's local `row_active`, whose
                                // signal dies with the unmounted scope.
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Connecting;
                                }
                                dioxus::core::spawn_forever(async move {
                                    match disconnect_device(name.clone()).await {
                                        Ok(true) => {
                                            let idx = devices
                                                .read()
                                                .iter()
                                                .position(|(n, _)| n == &name);
                                            if let Some(i) = idx {
                                                devices.write()[i].1 =
                                                    ConnectionStatus::Disconnected;
                                            }
                                        }
                                        Ok(false) => {
                                            let idx = devices
                                                .read()
                                                .iter()
                                                .position(|(n, _)| n == &name);
                                            if let Some(i) = idx {
                                                devices.write()[i].1 =
                                                    ConnectionStatus::Connected;
                                            }
                                        }
                                        Err(_e) => {
                                            let idx = devices
                                                .read()
                                                .iter()
                                                .position(|(n, _)| n == &name);
                                            if let Some(i) = idx {
                                                devices.write()[i].1 =
                                                    ConnectionStatus::Connected;
                                            }
                                            *toast_error.write() = Some(
                                                rust_i18n::t!("device.disconnect_failed", name = name.as_str())
                                                    .into_owned(),
                                            );
                                            // Auto-dismiss the toast after 5 s.
                                            dioxus::core::spawn_forever(async move {
                                                tokio::time::sleep(
                                                    std::time::Duration::from_secs(5),
                                                )
                                                .await;
                                                *toast_error.write() = None;
                                            });
                                        }
                                    }
                                });
                            }
                        },
                        span { class: "btn-disconnect-icon", "⏻" }
                    }
                    button {
                        class: "btn-settings",
                        onclick: {
                            // Clone is required: closure must own `name` for the push call.
                            let name = name.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                navigator.push(Route::DeviceSettings { name: name.clone() });
                            }
                        },
                        "⚙"
                    }
                }
            }
        }
    }
}

#[component]
fn DeviceSettings(name: String) -> Element {
    use_locale();

    let navigator = use_navigator();
    let mut auto_reconnect = use_signal(|| true);
    let mut trusted = use_signal(|| false);
    let mut alias = use_signal(|| name.clone());
    let mut codec = use_signal(|| "SBC".to_string());
    let mut volume = use_signal(|| 75_i32);

    let settings_title = rust_i18n::t!("settings.title", name = name.as_str());
    let volume_label = rust_i18n::t!("settings.volume", volume = volume().to_string().as_str());

    rsx! {
        div {
            class: "settings-page",

            div {
                class: "settings-header",
                button {
                    class: "btn-back",
                    onclick: move |_| { navigator.go_back(); },
                    "{rust_i18n::t!(\"common.back\")}"
                }
                h2 { "{settings_title}" }
            }

            div {
                class: "settings-section",
                h3 { "{rust_i18n::t!(\"settings.section_general\")}" }

                div { class: "settings-row",
                    label { "{rust_i18n::t!(\"settings.alias\")}" }
                    input {
                        r#type: "text",
                        value: "{alias}",
                        oninput: move |e| *alias.write() = e.value(),
                    }
                }

                div { class: "settings-row",
                    label { "{rust_i18n::t!(\"settings.auto_reconnect\")}" }
                    input {
                        r#type: "checkbox",
                        checked: auto_reconnect(),
                        oninput: move |e| *auto_reconnect.write() = e.checked(),
                    }
                }

                div { class: "settings-row",
                    label { "{rust_i18n::t!(\"settings.trusted\")}" }
                    input {
                        r#type: "checkbox",
                        checked: trusted(),
                        oninput: move |e| *trusted.write() = e.checked(),
                    }
                }
            }

            div {
                class: "settings-section",
                h3 { "{rust_i18n::t!(\"settings.section_audio\")}" }

                div { class: "settings-row",
                    label { "{rust_i18n::t!(\"settings.codec\")}" }
                    select {
                        value: "{codec}",
                        onchange: move |e| *codec.write() = e.value(),
                        option { value: "SBC",     "SBC" }
                        option { value: "AAC",     "AAC" }
                        option { value: "aptX",    "aptX" }
                        option { value: "aptX HD", "aptX HD" }
                        option { value: "LDAC",    "LDAC" }
                    }
                }

                div { class: "settings-row",
                    label { "{volume_label}" }
                    input {
                        r#type: "range",
                        min: "0",
                        max: "100",
                        value: "{volume}",
                        oninput: move |e| {
                            if let Ok(v) = e.value().parse() {
                                *volume.write() = v;
                            }
                        },
                    }
                }
            }

            div {
                class: "settings-section danger-section",
                h3 { "{rust_i18n::t!(\"settings.section_danger\")}" }
                button {
                    class: "btn-forget",
                    onclick: move |_| { navigator.go_back(); },
                    "{rust_i18n::t!(\"settings.forget\")}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        merge_connection_status, reconcile_connection_status, signal_bars, ConnectionStatus,
    };

    // AC: RSSI maps to a bar count (1..=4) and a colour, stronger = more bars/greener.
    #[test]
    fn test_signal_bars_strong_signal_is_full_and_green() {
        assert_eq!(signal_bars(-40), (4, "#22c55e"));
    }

    #[test]
    fn test_signal_bars_medium_signal_is_orange() {
        let (bars, color) = signal_bars(-72);
        assert_eq!(bars, 2);
        assert_eq!(color, "#f59e0b");
    }

    #[test]
    fn test_signal_bars_weak_signal_is_one_bar_and_red() {
        assert_eq!(signal_bars(-95), (1, "#ef4444"));
    }

    #[test]
    fn test_signal_bars_is_monotonic_in_strength() {
        // Weaker signal never yields more bars than a stronger one.
        assert!(signal_bars(-90).0 <= signal_bars(-50).0);
    }

    // AC: A pure helper assigns `Connected` to names present in the connected set
    // and `Disconnected` otherwise.
    #[test]
    fn test_merge_connection_status_assigns_connected_and_disconnected() {
        let found = vec![
            "Blue Speaker".to_string(),
            "HeadPhones Pro".to_string(),
            "Old Earbuds".to_string(),
        ];
        let connected = vec!["Blue Speaker".to_string(), "Old Earbuds".to_string()];

        let result = merge_connection_status(found, &connected);

        assert_eq!(
            result,
            vec![
                ("Blue Speaker".to_string(), ConnectionStatus::Connected),
                ("HeadPhones Pro".to_string(), ConnectionStatus::Disconnected),
                ("Old Earbuds".to_string(), ConnectionStatus::Connected),
            ],
            "names in the connected set must be Connected, others Disconnected"
        );
    }

    // AC: empty connected set → every device is Disconnected.
    #[test]
    fn test_merge_connection_status_empty_connected_all_disconnected() {
        let found = vec!["Blue Speaker".to_string(), "HeadPhones Pro".to_string()];
        let connected: Vec<String> = Vec::new();

        let result = merge_connection_status(found, &connected);

        assert!(
            result
                .iter()
                .all(|(_, s)| *s == ConnectionStatus::Disconnected),
            "with an empty connected set, every device must be Disconnected, got: {result:?}"
        );
        assert_eq!(result.len(), 2, "all found devices must be present");
    }

    // AC: the merge helper does not duplicate a device already present in the list
    // and refreshes its status from the connected set.
    #[test]
    fn test_merge_connection_status_no_duplicate_and_refreshes_status() {
        // "Blue Speaker" appears twice in the found list (e.g. a re-scan).
        let found = vec![
            "Blue Speaker".to_string(),
            "HeadPhones Pro".to_string(),
            "Blue Speaker".to_string(),
        ];
        let connected = vec!["Blue Speaker".to_string()];

        let result = merge_connection_status(found, &connected);

        // No duplicate entry for "Blue Speaker".
        let blue_count = result.iter().filter(|(n, _)| n == "Blue Speaker").count();
        assert_eq!(
            blue_count, 1,
            "a device must not be duplicated, got {blue_count} entries for 'Blue Speaker'"
        );
        // Its status is refreshed from the connected set.
        assert!(
            result
                .iter()
                .any(|(n, s)| n == "Blue Speaker" && *s == ConnectionStatus::Connected),
            "'Blue Speaker' status must be refreshed to Connected from the connected set"
        );
    }

    // AC: pre-existing connections detected at scan time count toward the x/2 counter.
    // The number of Connected entries must equal the number of found names in the set.
    #[test]
    fn test_merge_connection_status_counter_reflects_connected() {
        let found = vec![
            "Blue Speaker".to_string(),
            "HeadPhones Pro".to_string(),
            "Soundbar".to_string(),
        ];
        let connected = vec!["Blue Speaker".to_string(), "Soundbar".to_string()];

        let result = merge_connection_status(found, &connected);

        let connected_count = result
            .iter()
            .filter(|(_, s)| *s == ConnectionStatus::Connected)
            .count();
        assert_eq!(
            connected_count, 2,
            "the connected counter must reflect the two pre-existing connections"
        );
    }

    // Reconcile updates status in both directions: a device in the connected set
    // becomes Connected, one absent from it becomes Disconnected.
    #[test]
    fn test_reconcile_connection_status_updates_both_directions() {
        let mut devices = vec![
            ("Blue Speaker".to_string(), ConnectionStatus::Disconnected),
            ("HeadPhones Pro".to_string(), ConnectionStatus::Connected),
        ];
        let connected = vec!["Blue Speaker".to_string()];

        reconcile_connection_status(&mut devices, &connected);

        assert_eq!(
            devices,
            vec![
                ("Blue Speaker".to_string(), ConnectionStatus::Connected),
                ("HeadPhones Pro".to_string(), ConnectionStatus::Disconnected),
            ],
            "reconcile must connect listed devices and disconnect the rest"
        );
    }

    // Reconcile must never clobber an in-flight Connecting entry, even when that
    // device is absent from the connected set.
    #[test]
    fn test_reconcile_connection_status_preserves_connecting() {
        let mut devices = vec![
            ("Blue Speaker".to_string(), ConnectionStatus::Connecting),
            ("HeadPhones Pro".to_string(), ConnectionStatus::Connected),
        ];
        // Connecting device not yet reported connected; the other dropped its link.
        let connected: Vec<String> = Vec::new();

        reconcile_connection_status(&mut devices, &connected);

        assert_eq!(
            devices[0].1,
            ConnectionStatus::Connecting,
            "an in-flight Connecting entry must be preserved"
        );
        assert_eq!(
            devices[1].1,
            ConnectionStatus::Disconnected,
            "a non-connecting device absent from the set must become Disconnected"
        );
    }
}
