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

use bluetooth::{connect_device, disconnect_device, enable_bluetooth, scan_devices};

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_LOGO: Asset = asset!("/assets/bluetooth.svg");

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
    let mut bt_enabled = use_context::<Signal<bool>>();
    let mut scanning = use_signal(|| false);
    let mut show_confirm = use_signal(|| false);
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
                    onclick: move |_| *show_confirm.write() = true,
                    span { "⚡" }
                    "{rust_i18n::t!(\"bt.enable\")}"
                }
            }
            {
                let (connected, others): (Vec<_>, Vec<_>) = devices()
                    .into_iter()
                    .partition(|(_, s)| *s == ConnectionStatus::Connected);
                let is_empty = connected.is_empty() && others.is_empty();
                rsx! {
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
                }
            }
        }
        if show_confirm() {
            ConfirmModal {
                on_confirm: move || async move {
                    match enable_bluetooth().await {
                        Ok(true) => *bt_enabled.write() = true,
                        Ok(false) => {}
                        Err(e) => *bt_error.write() = Some(e.to_string()),
                    }
                    *show_confirm.write() = false;
                },
                on_cancel: move || *show_confirm.write() = false,
            }
        }
    }
}

#[component]
fn ConfirmModal(on_confirm: EventHandler<()>, on_cancel: EventHandler<()>) -> Element {
    use_locale();

    rsx! {
        div {
            class: "modal-overlay",
            onclick: move |_| on_cancel.call(()),
            div {
                class: "modal-box",
                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                p { class: "modal-title", "{rust_i18n::t!(\"bt.confirm_title\")}" }
                p { class: "modal-subtitle", "{rust_i18n::t!(\"bt.confirm_subtitle\")}" }
                div {
                    class: "modal-actions",
                    button {
                        class: "modal-btn modal-btn-cancel",
                        onclick: move |_| on_cancel.call(()),
                        "{rust_i18n::t!(\"common.no\")}"
                    }
                    button {
                        class: "modal-btn modal-btn-confirm",
                        onclick: move |_| on_confirm.call(()),
                        "{rust_i18n::t!(\"bt.confirm_yes\")}"
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

    rsx! {
        li {
            class: "device-row",
            onclick: {
                let name = name.clone();
                move |_| {
                    let name = name.clone();
                    async move {
                        let current = devices
                            .read()
                            .iter()
                            .find(|(n, _)| n == &name)
                            .map(|(_, s)| s.clone());
                        if !matches!(current, Some(ConnectionStatus::Disconnected)) {
                            return;
                        }
                        let idx = devices.read().iter().position(|(n, _)| n == &name);
                        if let Some(i) = idx {
                            devices.write()[i].1 = ConnectionStatus::Connecting;
                        }
                        let ok = connect_device(name.clone()).await.unwrap_or(false);
                        let idx = devices.read().iter().position(|(n, _)| n == &name);
                        if let Some(i) = idx {
                            devices.write()[i].1 = if ok {
                                ConnectionStatus::Connected
                            } else {
                                ConnectionStatus::Disconnected
                            };
                        }
                    }
                }
            },
            span { class: "{icon_class}", "{icon}" }
            span { class: "device-name", "{name}" }
            if is_connected {
                div { class: "device-actions",
                    button {
                        class: "btn-disconnect",
                        onclick: {
                            let name = name.clone();
                            move |e: Event<MouseData>| {
                                e.stop_propagation();
                                let name = name.clone();
                                async move {
                                    let idx = devices.read().iter().position(|(n, _)| n == &name);
                                    if let Some(i) = idx {
                                        devices.write()[i].1 = ConnectionStatus::Connecting;
                                    }
                                    let ok = disconnect_device(name.clone()).await.unwrap_or(false);
                                    let idx = devices.read().iter().position(|(n, _)| n == &name);
                                    if let Some(i) = idx {
                                        devices.write()[i].1 = if ok {
                                            ConnectionStatus::Disconnected
                                        } else {
                                            ConnectionStatus::Connected
                                        };
                                    }
                                }
                            }
                        },
                        "{disconnect_label}"
                    }
                    button {
                        class: "btn-settings",
                        onclick: {
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
                        oninput: move |e| *volume.write() = e.value().parse().unwrap_or(75),
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
