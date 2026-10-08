// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app::{protocol_message, use_locale, SettingsState};
use crate::{backend, discovery, jni_util, settings};
use dioxus::prelude::*;

/// Sync the active backend's config through the one rule every client follows
/// (#160) — push a change the backend has not acknowledged, otherwise read and
/// adopt what it has — and publish the result to the cache and the shared
/// signal, unless an edit landed meanwhile (see [`settings::settle_sync`]).
pub(crate) async fn sync_backend_config(
    mut app_settings: Signal<settings::AppSettings>,
) -> Result<(), backend::BackendError> {
    // Owned copies: the snapshot the sync started from, and the one it updates.
    let before = app_settings.peek().clone();
    let mut synced = before.clone();
    let outcome = backend::sync_config(&mut synced).await;
    // Settled whatever the outcome: a refused push keeps its pending mark.
    let settled = settings::settle_sync(&before, synced, app_settings.peek().clone());
    // Owned copy: the cache keeps its own settings.
    settings::set_current(settled.clone());
    *app_settings.write() = settled;
    outcome
}

/// One row of the discovery list (phase 6.6): a service the browse found, ready
/// to render — its classification already resolved into what the row shows and
/// what it offers.
struct DiscoveryRow {
    /// The service as found. Owned, because the rsx outlives the borrow of the
    /// signal the classification read from.
    service: blue2th_proto::DiscoveredBackend,
    /// What the corner badge means, as its tooltip. The status is shown as an
    /// icon rather than a label: a word next to a name and an address is three
    /// competing things on one card, and the address is the one that must stay
    /// readable.
    status_title: String,
    /// Whether the card carries the "in sync" badge — the app already knows this
    /// backend at this address, so there is nothing to do.
    synced: bool,
    /// Whether the card offers to create an entry for this backend.
    addable: bool,
    /// A repair the user must confirm first, when auto-repair is off.
    confirm: Option<PendingRepair>,
}

/// A move the app spotted but will not write until the user says so.
struct PendingRepair {
    /// Index of the known entry to move.
    index: usize,
    /// The normalised address it now answers at.
    url: String,
    /// The question to put on the button.
    prompt: String,
}

/// The app settings page (phase 6.2). Built to grow: this slice ships only the
/// **Backends** section — add, test, activate and delete the backends the app
/// knows, one active at a time.
#[component]
pub(crate) fn AppSettingsPage() -> Element {
    use_locale();
    let navigator = use_navigator();
    let mut app_settings = use_context::<SettingsState>().0;
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut notice: Signal<Option<String>> = use_signal(|| None);
    let mut name_draft = use_signal(String::new);
    let mut url_draft = use_signal(String::new);
    let mut code_draft = use_signal(String::new);
    // A pairing exchange in flight. The armed code is one-shot and attempt
    // capped, so a second tap could only ever spend an attempt and report a
    // failure for a code that in fact just worked.
    let pairing = use_signal(|| false);
    // The browser manages exactly one backend — the page origin — and follows
    // the backend's config rather than owning a list (#160).
    let browser = settings::CLIENT_KIND == settings::ClientKind::Browser;
    // Set when a fresh pairing's `GET /config` failed: the name and the toggles
    // stay hidden until the backend's real values are known.
    let mut config_unread = use_signal(|| false);
    // Opening the page syncs the active backend's config (#160): the toggles
    // then show what the backend applies, not a copy another client changed.
    use_hook(move || {
        spawn(async move {
            if let Err(e) = sync_backend_config(app_settings).await {
                // Unpaired: the Pairing section already says what to do.
                if !e.is_not_paired() {
                    *error.write() = Some(e.to_string());
                }
            }
        });
    });
    // The browser's name field edits the active entry, the one at the origin.
    let active_name: Option<(usize, String)> = {
        let snapshot = app_settings.read();
        snapshot
            .active
            // Owned name: the rsx below outlives this borrow of the signal.
            .and_then(|i| snapshot.backends.get(i).map(|b| (i, b.name.clone())))
    };

    // One read for the whole list (see `BackendStatus`): names, addresses and the
    // active index all come from the same snapshot.
    // The toggle applies to the active backend: it is the one the app talks to.
    let active_restore: Option<(usize, bool)> = {
        let settings = app_settings.read();
        settings.active.and_then(|i| {
            settings
                .backends
                .get(i)
                .map(|b| (i, b.restore_during_playback))
        })
    };
    // The phase 6.5 toggle sits in the same section and reads the same snapshot:
    // both settings describe what the *active* backend does with a speaker that
    // went away.
    let active_auto_reconnect: Option<(usize, bool)> = {
        let settings = app_settings.read();
        settings
            .active
            .and_then(|i| settings.backends.get(i).map(|b| (i, b.auto_reconnect)))
    };
    // Pairing applies to the active backend too: it is the one the app talks to.
    let active_pairing: Option<(usize, String, settings::PairingMethod, bool)> = {
        let snapshot = app_settings.read();
        snapshot.active.and_then(|i| {
            snapshot
                .backends
                .get(i)
                .map(|b| (i, b.url.clone(), b.pairing, b.token.is_some()))
        })
    };
    let paired_now = active_pairing
        .as_ref()
        .is_some_and(|(_, _, _, paired)| *paired);
    let entries: Vec<(usize, String, String, bool)> = {
        let snapshot = app_settings.read();
        let active = snapshot.active;
        snapshot
            .backends
            .iter()
            .enumerate()
            // Owned name/URL: the rsx below outlives this borrow of the signal.
            .map(|(i, b)| (i, b.name.clone(), b.url.clone(), Some(i) == active))
            .collect()
    };

    // ── Discovery (phase 6.6) ────────────────────────────────────────────────
    let mut scanning = use_signal(|| false);
    let mut scanned = use_signal(|| false);
    let mut found: Signal<Vec<blue2th_proto::DiscoveredBackend>> = use_signal(Vec::new);
    // A ROM that cannot resolve `MulticastLock` renders the button disabled
    // rather than failing on tap: the verdict is cached, so this costs no JNI.
    let can_search = discovery::search_enabled(jni_util::multicast_supported());
    let (auto_repair, adds_backends) = {
        let snapshot = app_settings.read();
        (snapshot.auto_repair_url, snapshot.discovery_adds_backends)
    };
    // Re-classified on every render against the current settings, so an entry
    // repaired a moment ago immediately reads as up to date.
    let results: Vec<DiscoveryRow> = {
        let snapshot = app_settings.read();
        found
            .read()
            .iter()
            .map(|service| {
                let (status_title, synced, addable, confirm) =
                    match settings::reconcile(&snapshot, service) {
                        settings::DiscoveryAction::UpToDate => (
                            rust_i18n::t!("app_settings.discovered_up_to_date").to_string(),
                            true,
                            false,
                            None,
                        ),
                        // Auto-repairs are applied by the scan itself, so seeing
                        // one here means the write is still pending this frame:
                        // the card already reads as settled.
                        settings::DiscoveryAction::Repair { .. } => (
                            rust_i18n::t!("app_settings.discovered_known").to_string(),
                            true,
                            false,
                            None,
                        ),
                        settings::DiscoveryAction::ConfirmRepair { index, url } => {
                            // Built here rather than in the rsx: `t!` with named
                            // arguments is not a formatted-segment expression.
                            let prompt = rust_i18n::t!(
                                "app_settings.confirm_repair",
                                name = service.name.as_str(),
                                url = url.as_str()
                            )
                            .to_string();
                            (
                                rust_i18n::t!("app_settings.discovered_known").to_string(),
                                false,
                                false,
                                Some(PendingRepair { index, url, prompt }),
                            )
                        },
                        settings::DiscoveryAction::Addable => (
                            rust_i18n::t!("app_settings.discovered_new").to_string(),
                            false,
                            true,
                            None,
                        ),
                        // Found and listed, but nothing is offered: adding is off,
                        // or the advertised address is unusable.
                        settings::DiscoveryAction::Ignored => (
                            rust_i18n::t!("app_settings.discovered_new").to_string(),
                            false,
                            false,
                            None,
                        ),
                    };
                DiscoveryRow {
                    // Owned copy: the rsx below outlives this borrow of the signal.
                    service: service.clone(),
                    status_title,
                    synced,
                    addable,
                    confirm,
                }
            })
            .collect()
    };

    rsx! {
        div { class: "settings-page",
            div { class: "settings-header",
                button {
                    class: "settings-back",
                    onclick: move |_| { navigator.go_back(); },
                    "‹"
                }
                span { class: "settings-title", "{rust_i18n::t!(\"app_settings.title\")}" }
            }

            // No mDNS in the browser, and only one backend to find (#160).
            if !browser {
                div { class: "settings-section",
                    div { class: "settings-section-title", "{rust_i18n::t!(\"app_settings.section_discovery\")}" }

                    label { class: "settings-toggle",
                        input {
                            r#type: "checkbox",
                            checked: auto_repair,
                            onchange: move |e| {
                                let mut next = app_settings.peek().clone();
                                next.set_auto_repair_url(e.checked());
                                // Owned copy: the cache keeps its own settings.
                                settings::set_current(next.clone());
                                *app_settings.write() = next;
                            },
                        }
                        span { class: "settings-toggle-label",
                            "{rust_i18n::t!(\"app_settings.auto_repair_url\")}"
                        }
                    }
                    div { class: "settings-hint", "{rust_i18n::t!(\"app_settings.auto_repair_url_hint\")}" }

                    label { class: "settings-toggle",
                        input {
                            r#type: "checkbox",
                            checked: adds_backends,
                            onchange: move |e| {
                                let mut next = app_settings.peek().clone();
                                next.set_discovery_adds_backends(e.checked());
                                settings::set_current(next.clone());
                                *app_settings.write() = next;
                            },
                        }
                        span { class: "settings-toggle-label",
                            "{rust_i18n::t!(\"app_settings.discovery_adds_backends\")}"
                        }
                    }
                    div { class: "settings-hint",
                        "{rust_i18n::t!(\"app_settings.discovery_adds_backends_hint\")}"
                    }

                    // The search action and everything it produces live in one framed
                    // block: the two toggles above configure discovery, this is
                    // discovery itself, and the results belong to the button that
                    // fetched them.
                    div { class: "discovery-panel",
                        button {
                            class: "settings-action",
                            disabled: !can_search || scanning(),
                            onclick: move |_| {
                                if scanning() {
                                    return;
                                }
                                *scanning.write() = true;
                                *scanned.write() = true;
                                *error.write() = None;
                                *notice.write() = None;
                                spawn(async move {
                                    // No mDNS in the browser (#159): the button is
                                    // disabled there, and a stray tap reports why.
                                    #[cfg(not(target_arch = "wasm32"))]
                                    let outcome = discovery::browse(discovery::BROWSE_TIMEOUT).await;
                                    #[cfg(target_arch = "wasm32")]
                                    let outcome: Result<Vec<blue2th_proto::DiscoveredBackend>, _> =
                                        Err(discovery::DiscoveryError::Unsupported);
                                    match outcome {
                                        Ok(services) => {
                                            // Every change lands in one write, so a scan
                                            // finding two moved backends redraws once.
                                            let mut next = app_settings.peek().clone();
                                            // A pre-6.6 entry learns the id of the machine
                                            // answering at its address, so the *next* lease
                                            // change repairs it instead of offering it as new.
                                            let mut changed = next.adopt_discovered_ids(&services);
                                            let mut repaired = false;
                                            for service in &services {
                                                if let settings::DiscoveryAction::Repair { index, url } =
                                                    settings::reconcile(&next, service)
                                                {
                                                    repaired |= next.set_url(index, &url).is_ok();
                                                }
                                            }
                                            changed |= repaired;
                                            if changed {
                                                settings::set_current(next.clone());
                                                *app_settings.write() = next;
                                            }
                                            // Only a moved address is worth saying: adopting
                                            // an id changes nothing the user can see.
                                            if repaired {
                                                *notice.write() = Some(
                                                    rust_i18n::t!("app_settings.address_repaired").to_string(),
                                                );
                                            }
                                            *found.write() = services;
                                        },
                                        Err(e) => *error.write() = Some(e.to_string()),
                                    }
                                    *scanning.write() = false;
                                });
                            },
                            if scanning() {
                                "{rust_i18n::t!(\"app_settings.searching\")}"
                            } else {
                                "{rust_i18n::t!(\"app_settings.search_network\")}"
                            }
                        }

                        if !can_search {
                            div { class: "settings-hint",
                                "{rust_i18n::t!(\"app_settings.discovery_unsupported\")}"
                            }
                        }
                        // Finding nothing is a neutral state, never an error: a guest
                        // Wi-Fi, a filtered multicast or a backend that is simply down
                        // all look the same from here, and typing the address still
                        // works.
                        if scanned() && !scanning() && results.is_empty() {
                            div { class: "settings-empty",
                                "{rust_i18n::t!(\"app_settings.no_backend_found\")}"
                            }
                        }

                        for DiscoveryRow { service, status_title, synced, addable, confirm } in results {
                            div { key: "{service.url}", class: "discovery-card",
                                div { class: "discovery-card-name", "{service.name}" }
                                div { class: "discovery-card-url", "{service.url}" }
                                // Corner badges, overlapping the card's top edge so
                                // they read as a mark on the card rather than a third
                                // line competing with the name and the address. Last
                                // in the DOM because the add button consumes
                                // `service`, and absolutely positioned anyway, so the
                                // order here says nothing about where they land.
                                div { class: "discovery-card-badges",
                                    if synced {
                                        span {
                                            class: "discovery-synced",
                                            title: "{status_title}",
                                            "⟳"
                                        }
                                    }
                                    if addable {
                                        button {
                                            class: "discovery-add",
                                            // The only label this button gets: the
                                            // glyph carries the meaning, the tooltip
                                            // and the accessible name carry the words.
                                            title: "{status_title}",
                                            "aria-label": "{rust_i18n::t!(\"app_settings.add\")}",
                                            onclick: move |_| {
                                                let mut next = app_settings.peek().clone();
                                                // Being found grants nothing: the entry
                                                // is created unpaired and the code is
                                                // still due.
                                                match next.add_discovered(&service) {
                                                    Ok(_) => {
                                                        settings::set_current(next.clone());
                                                        *app_settings.write() = next;
                                                    },
                                                    Err(err) => *error.write() = Some(err.to_string()),
                                                }
                                            },
                                            "+"
                                        }
                                    }
                                }
                                if let Some(PendingRepair { index, url, prompt }) = confirm {
                                    button {
                                        class: "discovery-confirm",
                                        onclick: move |_| {
                                            let mut next = app_settings.peek().clone();
                                            match next.set_url(index, &url) {
                                                Ok(()) => {
                                                    settings::set_current(next.clone());
                                                    *app_settings.write() = next;
                                                    *notice.write() = Some(
                                                        rust_i18n::t!("app_settings.address_repaired")
                                                            .to_string(),
                                                    );
                                                },
                                                Err(err) => *error.write() = Some(err.to_string()),
                                            }
                                        },
                                        "{prompt}"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // The browser's one backend: the origin, read-only, and the name the
            // backend answers to — shown once paired and read (#160).
            if browser {
                div { class: "settings-section",
                    div { class: "settings-section-title", "{rust_i18n::t!(\"app_settings.section_backend\")}" }
                    // Owned copy: the rsx below outlives this render's snapshot.
                    if let Some((_, url, _, paired)) = active_pairing.clone() {
                        div { class: "backend-row active",
                            div { class: "backend-row-meta",
                                span { class: "backend-row-url", "{url}" }
                            }
                        }
                        if !paired {
                            div { class: "settings-hint",
                                "{rust_i18n::t!(\"app_settings.pair_browser_hint\")}"
                            }
                        } else if !config_unread() {
                            if let Some((index, name)) = active_name {
                                input {
                                    class: "backend-input",
                                    r#type: "text",
                                    maxlength: "{blue2th_proto::MAX_BACKEND_NAME_LEN}",
                                    placeholder: "{rust_i18n::t!(\"app_settings.name_placeholder\")}",
                                    value: "{name}",
                                    // On commit (Enter, or leaving the field), not on
                                    // every keystroke: each one would be a push.
                                    onchange: move |e| {
                                        // Owned copy: mutated, then written back to
                                        // the shared signal.
                                        let mut next = app_settings.peek().clone();
                                        if let Err(err) = next.set_name(index, &e.value()) {
                                            *notice.write() = None;
                                            *error.write() = Some(err.to_string());
                                            return;
                                        }
                                        // Owned copy: the cache keeps its own settings.
                                        settings::set_current(next.clone());
                                        *app_settings.write() = next;
                                        let mut error = error;
                                        spawn(async move {
                                            // The backend advertises the name, so the
                                            // edit is meaningless until it knows; a
                                            // refused push stays pending (#160).
                                            *error.write() =
                                                sync_backend_config(app_settings).await.err().map(|e| e.to_string());
                                        });
                                    },
                                }
                            }
                        }
                    } else {
                        div { class: "settings-empty", "{rust_i18n::t!(\"app_settings.no_backend_yet\")}" }
                    }
                }
            }

            if !browser {
                div { class: "settings-section",
                    div { class: "settings-section-title", "{rust_i18n::t!(\"app_settings.backends\")}" }

                    if entries.is_empty() {
                        div { class: "settings-empty", "{rust_i18n::t!(\"app_settings.no_backend_yet\")}" }
                    }
                    for (index, name, url, active) in entries {
                        div { class: if active { "backend-row active" } else { "backend-row" },
                            div { class: "backend-row-meta",
                                span { class: "backend-row-name", "{name}" }
                                span { class: "backend-row-url", "{url}" }
                            }
                            button {
                                class: "backend-row-action",
                                disabled: active,
                                onclick: move |_| {
                                    let mut error = error;
                                    let mut notice = notice;
                                    spawn(async move {
                                        // Owned copy: mutated by the switch, then
                                        // written back to the shared signal.
                                        let mut next = app_settings.peek().clone();
                                        let outcome = backend::activate_backend(&mut next, index).await;
                                        *app_settings.write() = next;
                                        // Feedback left over from a previous action
                                        // would read as this one's outcome.
                                        *notice.write() = None;
                                        *error.write() = outcome.err().map(|e| e.to_string());
                                    });
                                },
                                if active {
                                    "{rust_i18n::t!(\"app_settings.active\")}"
                                } else {
                                    "{rust_i18n::t!(\"app_settings.activate\")}"
                                }
                            }
                            button {
                                class: "backend-row-delete",
                                title: "{rust_i18n::t!(\"app_settings.delete\")}",
                                aria_label: "{rust_i18n::t!(\"app_settings.delete\")}",
                                onclick: move |_| {
                                    *notice.write() = None;
                                    *error.write() = None;
                                    spawn(async move {
                                        let mut next = app_settings.peek().clone();
                                        // Deleting a backend that is streaming must
                                        // quieten it and stop its Spotify source:
                                        // forgetting it locally would leave the PC
                                        // playing to the speakers with no way left in
                                        // the app to stop it.
                                        let released = backend::remove_backend(&mut next, index).await;
                                        // The local removal happened whatever the
                                        // remote calls did, so the list is updated
                                        // either way.
                                        *app_settings.write() = next;
                                        if let Err(e) = released {
                                            *error.write() = Some(e.to_string());
                                        }
                                    });
                                },
                                "✕"
                            }
                        }
                    }

                    div { class: "backend-form",
                        input {
                            class: "backend-input",
                            r#type: "text",
                            maxlength: "{blue2th_proto::MAX_BACKEND_NAME_LEN}",
                            placeholder: "{rust_i18n::t!(\"app_settings.name_placeholder\")}",
                            value: "{name_draft}",
                            oninput: move |e| *name_draft.write() = e.value(),
                        }
                        input {
                            class: "backend-input",
                            r#type: "text",
                            placeholder: "{rust_i18n::t!(\"app_settings.url_placeholder\")}",
                            value: "{url_draft}",
                            oninput: move |e| *url_draft.write() = e.value(),
                        }
                        div { class: "backend-form-actions",
                            button {
                                class: "backend-test",
                                onclick: move |_| {
                                    let url = url_draft();
                                    let mut error = error;
                                    let mut notice = notice;
                                    spawn(async move {
                                        // Exactly one of the two is shown: a stale
                                        // error next to a fresh "it answered" (or the
                                        // reverse) is unreadable.
                                        match backend::test_backend(&url).await {
                                            Ok(_) => {
                                                *error.write() = None;
                                                *notice.write() = Some(
                                                    rust_i18n::t!("app_settings.test_ok").to_string(),
                                                );
                                            },
                                            Err(e) => {
                                                *notice.write() = None;
                                                *error.write() = Some(e.to_string());
                                            },
                                        }
                                    });
                                },
                                "{rust_i18n::t!(\"app_settings.test\")}"
                            }
                            button {
                                class: "backend-add",
                                onclick: move |_| {
                                    let mut next = app_settings.peek().clone();
                                    match next.add(&name_draft(), &url_draft()) {
                                        Ok(()) => {
                                            let added = next.backends.len().saturating_sub(1);
                                            // First backend added: it becomes the active
                                            // one, or the app would still know no address.
                                            let activating = next.active.is_none();
                                            // Owned copy: the process-wide cache keeps
                                            // its own settings beyond this handler.
                                            settings::set_current(next.clone());
                                            *app_settings.write() = next;
                                            *name_draft.write() = String::new();
                                            *url_draft.write() = String::new();
                                            *notice.write() = None;
                                            *error.write() = None;
                                            if activating {
                                                // Through the shared activation path, so
                                                // the name reaches the backend: storing
                                                // it locally alone left the Connect
                                                // device advertising the old one.
                                                let mut error = error;
                                                spawn(async move {
                                                    let mut next = app_settings.peek().clone();
                                                    let outcome =
                                                        backend::activate_backend(&mut next, added)
                                                            .await;
                                                    *app_settings.write() = next;
                                                    if let Err(e) = outcome {
                                                        *error.write() = Some(e.to_string());
                                                    }
                                                });
                                            }
                                        },
                                        Err(e) => {
                                            *notice.write() = None;
                                            *error.write() = Some(e.to_string());
                                        },
                                    }
                                },
                                "{rust_i18n::t!(\"app_settings.add\")}"
                            }
                        }
                    }

                }
            }

            div { class: "settings-section",
                div { class: "settings-section-title", "{rust_i18n::t!(\"app_settings.section_pairing\")}" }

                // Owned copy: the event closures below outlive this render, and
                // the address must be the one the section was drawn for.
                if let Some((index, url, method, paired)) = active_pairing.clone() {
                    div { class: if paired { "settings-hint paired" } else { "settings-hint" },
                        if paired {
                            "{rust_i18n::t!(\"app_settings.paired_ok\")}"
                        } else {
                            "{rust_i18n::t!(\"app_settings.not_paired\")}"
                        }
                    }
                    // The browser has no camera path back into the page: a code
                    // is the only way to pair it (#160).
                    if !browser {
                        div { class: "settings-hint", "{rust_i18n::t!(\"app_settings.pairing_method\")}" }
                        div { class: "backend-form-actions",
                            button {
                                class: if method == settings::PairingMethod::Code { "backend-row-action active" } else { "backend-row-action" },
                                onclick: move |_| {
                                    let mut next = app_settings.peek().clone();
                                    if let Err(e) = next.set_pairing_method(index, settings::PairingMethod::Code) {
                                        *error.write() = Some(e.to_string());
                                        return;
                                    }
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                },
                                "{rust_i18n::t!(\"app_settings.pairing_method_code\")}"
                            }
                            button {
                                class: if method == settings::PairingMethod::Qr { "backend-row-action active" } else { "backend-row-action" },
                                onclick: move |_| {
                                    let mut next = app_settings.peek().clone();
                                    if let Err(e) = next.set_pairing_method(index, settings::PairingMethod::Qr) {
                                        *error.write() = Some(e.to_string());
                                        return;
                                    }
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                },
                                "{rust_i18n::t!(\"app_settings.pairing_method_qr\")}"
                            }
                        }
                    }

                    if browser || method == settings::PairingMethod::Code {
                        input {
                            class: "backend-input",
                            r#type: "text",
                            // An unpaired browser opens here to type the code.
                            autofocus: browser && !paired,
                            placeholder: "{rust_i18n::t!(\"app_settings.pairing_code_placeholder\")}",
                            value: "{code_draft}",
                            oninput: move |e| *code_draft.write() = e.value(),
                        }
                        button {
                            class: "backend-add",
                            // Every submission spends one of the server's five
                            // attempts, after which the armed code is dead: an
                            // empty box, or a second tap while the first is still
                            // in flight, must not cost the user a retry.
                            disabled: pairing() || code_draft().trim().is_empty(),
                            onclick: move |_| {
                                // Normalised: the code is read off a terminal and
                                // typed, so a stray space or Android's lower-case
                                // tail is a failed attempt the user cannot see.
                                let code = blue2th_proto::normalize_pairing_code(&code_draft());
                                if code.is_empty() {
                                    return;
                                }
                                let url = url.clone();
                                let mut error = error;
                                let mut notice = notice;
                                let mut code_draft = code_draft;
                                let mut pairing = pairing;
                                *pairing.write() = true;
                                spawn(async move {
                                    // The contract first: pairing with a backend
                                    // this app cannot talk to spends an attempt
                                    // for a token nothing could use.
                                    let compatible = match backend::check_backend_protocol(&url).await {
                                        Ok(compatible) => compatible,
                                        Err(e) => {
                                            *pairing.write() = false;
                                            *notice.write() = None;
                                            *error.write() = Some(match e.protocol_mismatch() {
                                                Some(mismatch) => protocol_message(mismatch),
                                                None => e.to_string(),
                                            });
                                            return;
                                        },
                                    };
                                    // The one call that carries no bearer: the
                                    // app has none until this succeeds.
                                    let outcome = backend::pair(&compatible, &code).await;
                                    *pairing.write() = false;
                                    match outcome {
                                        Ok(token) => {
                                            let mut next = app_settings.peek().clone();
                                            match next.set_token(index, Some(token)) {
                                                Ok(()) => {
                                                    settings::set_current(next.clone());
                                                    *app_settings.write() = next;
                                                    *code_draft.write() = String::new();
                                                    *error.write() = None;
                                                    *notice.write() = Some(
                                                        rust_i18n::t!("app_settings.paired_ok").to_string(),
                                                    );
                                                    // Paired: the backend learns the
                                                    // typed name, or the app adopts its
                                                    // config (#160). The browser shows
                                                    // the backend's real settings, never
                                                    // a guess, so it waits for this sync.
                                                    // The token is kept whatever it does.
                                                    *config_unread.write() = browser;
                                                    match sync_backend_config(app_settings).await {
                                                        Ok(()) => *config_unread.write() = false,
                                                        Err(e) => {
                                                            *notice.write() = None;
                                                            *error.write() = Some(e.to_string());
                                                        },
                                                    }
                                                },
                                                Err(e) => *error.write() = Some(e.to_string()),
                                            }
                                        },
                                        Err(e) => {
                                            *notice.write() = None;
                                            *error.write() = Some(e.to_string());
                                        },
                                    }
                                });
                            },
                            "{rust_i18n::t!(\"app_settings.pair\")}"
                        }
                    } else {
                        // Nothing to type on this path: the QR carries the code,
                        // and the deep link brings it back into the app.
                        div { class: "settings-hint",
                            "{rust_i18n::t!(\"app_settings.pairing_qr_hint\")}"
                        }
                    }
                } else {
                    div { class: "settings-empty", "{rust_i18n::t!(\"app_settings.no_backend_yet\")}" }
                }
            }

            // In the browser the toggles show the backend's real state, so they
            // wait for a pairing and a read (#160); the Backend section says why.
            if !browser || (paired_now && !config_unread()) {
                div { class: "settings-section",
                    div { class: "settings-section-title", "{rust_i18n::t!(\"app_settings.section_playback\")}" }

                    if let Some((index, restoring)) = active_restore {
                        label { class: "settings-toggle",
                            input {
                                r#type: "checkbox",
                                checked: restoring,
                                onchange: move |e| {
                                    // `checked()`, like the per-device toggles: the
                                    // box's state, not its (unset) `value` attribute.
                                    let enabled = e.checked();
                                    let mut next = app_settings.peek().clone();
                                    if let Err(err) = next.set_restore_during_playback(index, enabled) {
                                        *error.write() = Some(err.to_string());
                                        return;
                                    }
                                    // Owned copy: the cache keeps its own settings.
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                    let mut error = error;
                                    spawn(async move {
                                        // The backend decides the restoration, so the
                                        // toggle is meaningless until it knows; a
                                        // refused push stays pending (#160).
                                        if let Err(e) = sync_backend_config(app_settings).await {
                                            *error.write() = Some(e.to_string());
                                        }
                                    });
                                },
                            }
                            span { class: "settings-toggle-label",
                                "{rust_i18n::t!(\"app_settings.restore_during_playback\")}"
                            }
                        }
                        div { class: "settings-hint",
                            "{rust_i18n::t!(\"app_settings.restore_during_playback_hint\")}"
                        }
                    } else {
                        div { class: "settings-empty", "{rust_i18n::t!(\"app_settings.no_backend_yet\")}" }
                    }

                    if let Some((index, reconnecting)) = active_auto_reconnect {
                        label { class: "settings-toggle",
                            input {
                                r#type: "checkbox",
                                checked: reconnecting,
                                onchange: move |e| {
                                    // `checked()`, like every other toggle here: the
                                    // box's state, not its (unset) `value` attribute.
                                    let enabled = e.checked();
                                    let mut next = app_settings.peek().clone();
                                    if let Err(err) = next.set_auto_reconnect(index, enabled) {
                                        *error.write() = Some(err.to_string());
                                        return;
                                    }
                                    // Owned copy: the cache keeps its own settings.
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                    let mut error = error;
                                    spawn(async move {
                                        // The backend does the dialling, so the toggle
                                        // is meaningless until it knows; a refused
                                        // push stays pending (#160).
                                        if let Err(e) = sync_backend_config(app_settings).await {
                                            *error.write() = Some(e.to_string());
                                        }
                                    });
                                },
                            }
                            span { class: "settings-toggle-label",
                                "{rust_i18n::t!(\"app_settings.auto_reconnect\")}"
                            }
                        }
                        div { class: "settings-hint",
                            "{rust_i18n::t!(\"app_settings.auto_reconnect_hint\")}"
                        }
                    }
                }
            }

            // Feedback for every section above, in its own card — rendered only
            // when there is something to say, or an empty bordered box would sit
            // under the page for the whole session.
            if notice().is_some() || error().is_some() {
                div { class: "settings-section",
                    if let Some(message) = notice() {
                        div { class: "settings-notice", "{message}" }
                    }
                    if let Some(message) = error() {
                        div { class: "toast-error", "{message}" }
                    }
                }
            }
        }
    }
}
