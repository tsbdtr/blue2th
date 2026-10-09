// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

use blue2th_frontend::{backend, deep_link, discovery, jni_util, lifecycle, settings, timer};

mod views;

use views::app::App;

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_ICON: Asset = asset!("/assets/bluetooth.svg");

fn main() {
    dioxus::launch(App);
}
