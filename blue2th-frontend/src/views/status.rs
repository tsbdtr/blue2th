// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app::{protocol_message, use_backend_health, use_locale, Route, SettingsState};
use crate::{backend, settings};
use dioxus::prelude::*;

/// The backend status encart: the active backend's name (or `-`), a reachability
/// dot, and — on tap — the quick-switch list of configured backends.
///
/// The switch goes through `backend::activate_backend`, the very same path the
/// settings page uses, so the shortcut can never drift from the page.
#[component]
pub(crate) fn BackendStatus(error: Signal<Option<String>>) -> Element {
    use_locale();
    // Reachable is not the same as usable since phase 6.4: an unpaired backend
    // answers `/health` and 401s everything else. Nor since #33: one that
    // answers may speak a contract this app cannot follow. Read through the
    // shared hook so the dot and the banner classify the same backend the same
    // way.
    let health = use_backend_health();
    let mut app_settings = use_context::<SettingsState>().0;
    let navigator = use_navigator();
    let mut open = use_signal(|| false);

    let label = app_settings.read().active_label();
    let tooltip = match health {
        settings::BackendHealth::Offline => rust_i18n::t!("server.offline").to_string(),
        // Names the machine to update: "incompatible" alone leaves the user
        // with nothing to do.
        settings::BackendHealth::Incompatible(mismatch) => protocol_message(mismatch),
        settings::BackendHealth::Unpaired => rust_i18n::t!("app_settings.not_paired").to_string(),
        settings::BackendHealth::Ready => rust_i18n::t!("server.online").to_string(),
    };
    // One read for the whole list: the active index comes from the same snapshot
    // as the names, so the menu can never mark a row the list no longer holds.
    let entries: Vec<(usize, String, bool)> = {
        let snapshot = app_settings.read();
        let active = snapshot.active;
        snapshot
            .backends
            .iter()
            .enumerate()
            // Owned name: the rsx below outlives this borrow of the signal.
            .map(|(i, b)| (i, b.name.clone(), Some(i) == active))
            .collect()
    };

    rsx! {
        div { class: "backend-switch",
            div {
                class: "backend-status backend-card",
                title: "{tooltip}",
                onclick: move |_| {
                    let now = open();
                    *open.write() = !now;
                },
                span { class: "backend-status-label", "{label}" }
                span { class: "backend-status-sep" }
                span {
                    class: "backend-status-dot",
                    style: format!(
                        "display:inline-block;width:12px;height:12px;border-radius:50%;background:{};",
                        // Amber for the in-between state: reachable, but nothing
                        // will work until the code is exchanged.
                        match health {
                            settings::BackendHealth::Offline => "#ef4444",
                            // Its own colour: an incompatible backend and an
                            // unreachable one call for different actions.
                            settings::BackendHealth::Incompatible(_) => "#f97316",
                            settings::BackendHealth::Unpaired => "#f59e0b",
                            settings::BackendHealth::Ready => "#22c55e",
                        },
                    ),
                }
            }
            if open() {
                div { class: "backend-menu",
                    if entries.is_empty() {
                        // Nothing to switch to: point at the place that fixes it
                        // rather than showing an empty menu.
                        button {
                            class: "backend-menu-item",
                            onclick: move |_| {
                                *open.write() = false;
                                navigator.push(Route::AppSettingsPage {});
                            },
                            "{rust_i18n::t!(\"app_settings.no_backend_yet\")}"
                        }
                    }
                    for (index, name, active) in entries {
                        button {
                            class: if active { "backend-menu-item active" } else { "backend-menu-item" },
                            onclick: move |_| {
                                *open.write() = false;
                                if active {
                                    return;
                                }
                                let mut error = error;
                                spawn(async move {
                                    // Owned copy: the switch is applied to it and
                                    // written back, never to a borrowed signal
                                    // across the await.
                                    let mut next = app_settings.peek().clone();
                                    let outcome = backend::activate_backend(&mut next, index).await;
                                    *app_settings.write() = next;
                                    if let Err(e) = outcome {
                                        *error.write() = Some(e.to_string());
                                    }
                                });
                            },
                            "{name}"
                        }
                    }
                }
            }
        }
    }
}

/// Gear control closing the status row, routing to the settings page.
#[component]
pub(crate) fn SettingsButton() -> Element {
    use_locale();
    let navigator = use_navigator();
    let label = rust_i18n::t!("app_settings.title");
    rsx! {
        button {
            class: "settings-button",
            title: "{label}",
            aria_label: "{label}",
            onclick: move |_| {
                navigator.push(Route::AppSettingsPage {});
            },
            "⚙"
        }
    }
}
