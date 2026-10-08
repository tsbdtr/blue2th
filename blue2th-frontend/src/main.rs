// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

mod backend;
mod deep_link;
mod discovery;
mod jni_util;
// Presence reports: the JNI hooks on Android (their tokio half is native-only,
// #159), the page lifecycle events in the browser (#160).
mod lifecycle;
mod settings;
mod timer;
mod views;

use views::app::App;

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_ICON: Asset = asset!("/assets/bluetooth.svg");

fn main() {
    dioxus::launch(App);
}
