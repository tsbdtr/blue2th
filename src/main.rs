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

use dioxus::prelude::*;

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
                    span { class: "app-title-text", "Blue" }
                    span { class: "app-title-num", "2" }
                    span { class: "app-title-text", "th" }
                }
                img {
                    class: "app-logo",
                    src: BLUETOOTH_LOGO,
                    alt: "Bluetooth",
                }
            }
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
    use super::{merge_connection_status, reconcile_connection_status, ConnectionStatus};

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
