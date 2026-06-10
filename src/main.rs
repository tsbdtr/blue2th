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

use bluetooth::{connect_device, disconnect_device, request_enable_bluetooth, scan_devices};
#[cfg(target_os = "android")]
use bluetooth::{enable_bluetooth, is_device_connected};

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_LOGO: Asset = asset!("/assets/bluetooth.svg");

const MAX_CONNECTIONS: usize = 2;

#[derive(Clone, PartialEq)]
enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
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

    // On Android, poll every 2 s to detect when a Connected device drops its A2DP link
    // externally (e.g. device powered off). When is_device_connected returns Ok(false),
    // revert the device status to Disconnected so the UI stays accurate.
    #[cfg(target_os = "android")]
    use_hook(|| {
        // Clone is required: the spawned async block needs its own Signal handle.
        let mut devices = devices;
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // Collect names of Connected devices (snapshot to avoid holding the lock
                // across an await point).
                let connected_names: Vec<String> = devices
                    .read()
                    .iter()
                    .filter(|(_, s)| *s == ConnectionStatus::Connected)
                    .map(|(n, _)| n.clone())
                    .collect();
                for name in connected_names {
                    // Clone is required: is_device_connected takes String by value, and
                    // `name` is still needed in the position() lookup after the await.
                    if let Ok(false) = is_device_connected(name.clone()).await {
                        // Device dropped — revert to Disconnected.
                        let idx = devices.read().iter().position(|(n, _)| n == &name);
                        if let Some(i) = idx {
                            devices.write()[i].1 = ConnectionStatus::Disconnected;
                        }
                    }
                }
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
                        let mut d = devices.write();
                        for name in found {
                            if !d.iter().any(|(n, _)| n == &name) {
                                d.push((name, ConnectionStatus::Disconnected));
                            }
                        }
                        *scanning.write() = false;
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
                                            DeviceItem { key: "{name}", name, status, devices }
                                        }
                                    }
                                }
                                ul { class: "device-list",
                                    for (name, status) in others {
                                        DeviceItem { key: "{name}", name, status, devices }
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
        }
    }
}

#[component]
fn DeviceItem(
    name: String,
    status: ConnectionStatus,
    devices: Signal<Vec<(String, ConnectionStatus)>>,
) -> Element {
    use_locale();

    let navigator = use_navigator();
    let (icon, icon_class) = status_icon(&status);
    let is_connected = status == ConnectionStatus::Connected;
    let disconnect_label = rust_i18n::t!("device.disconnect");

    // Ephemeral error notification: auto-clears after 3 s via a spawned task.
    let mut connect_error: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        li {
            class: "device-row",
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
                    // Show the spinner immediately before handing off to the background task.
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
                            }
                            Ok(false) => {
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Disconnected;
                                }
                            }
                            Err(e) => {
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Disconnected;
                                }
                                *connect_error.write() = Some(e.to_string());
                                spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    *connect_error.write() = None;
                                });
                            }
                        }
                    });
                }
            },
            span { class: "{icon_class}", "{icon}" }
            span { class: "device-name", "{name}" }
            if let Some(err) = connect_error() {
                span { class: "device-connect-error", "{err}" }
            }
            if is_connected {
                div { class: "device-actions",
                    button {
                        class: "btn-disconnect",
                        onclick: {
                            // Clone is required: same reason as the connect closure above.
                            let name = name.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                // Clone is required: `spawn` captures a `'static` async block.
                                let name = name.clone();
                                // Show the spinner immediately before handing off to the background task.
                                let idx = devices.read().iter().position(|(n, _)| n == &name);
                                if let Some(i) = idx {
                                    devices.write()[i].1 = ConnectionStatus::Connecting;
                                }
                                // BT operation runs on a background task — UI thread stays free.
                                spawn(async move {
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
                                        Err(e) => {
                                            let idx = devices
                                                .read()
                                                .iter()
                                                .position(|(n, _)| n == &name);
                                            if let Some(i) = idx {
                                                devices.write()[i].1 =
                                                    ConnectionStatus::Connected;
                                            }
                                            *connect_error.write() = Some(e.to_string());
                                            spawn(async move {
                                                tokio::time::sleep(
                                                    std::time::Duration::from_secs(3),
                                                )
                                                .await;
                                                *connect_error.write() = None;
                                            });
                                        }
                                    }
                                });
                            }
                        },
                        "{disconnect_label}"
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
