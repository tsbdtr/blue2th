// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app::{use_locale, Route};
use super::devices::BackendScan;
use crate::settings;
use dioxus::prelude::*;

/// Whether the start page has already been applied: it is decided once per app
/// start, not every time `Home` mounts — or going back from the settings page
/// would bounce straight back to it.
static START_PAGE_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[component]
pub(crate) fn Home() -> Element {
    use_locale();
    let navigator = use_navigator();
    // An unpaired browser opens where it pairs (#160); the phone stays here.
    use_effect(move || {
        if START_PAGE_APPLIED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let paired = settings::current().active_token().is_some();
        if settings::start_page(settings::CLIENT_KIND, paired) == settings::StartPage::Settings {
            navigator.push(Route::AppSettingsPage {});
        }
    });

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
            }
            BackendScan {}
        }
    }
}
