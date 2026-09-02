// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use dioxus::prelude::*;

mod backend;
mod deep_link;
mod discovery;
mod jni_util;
mod lifecycle;
mod settings;

rust_i18n::i18n!("locales", fallback = "fr");

const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
const BLUETOOTH_LOGO: Asset = asset!("/assets/bluetooth.svg");

/// Vertical travel (px) past which a drag on the transport handle is treated as
/// an expand/collapse gesture rather than a tap.
const TRANSPORT_DRAG_THRESHOLD_PX: f64 = 24.0;

/// How often the app re-checks the PC backend's reachability.
const BACKEND_HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the vertical volume panel stays open after the last interaction.
const VOLUME_PANEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// How often the app checks whether Android handed it a custom-scheme redirect
/// (the Spotify OAuth callback). Short enough that the login feels immediate on
/// return from the browser; the check is a single JNI call when idle.
const DEEP_LINK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether the PC backend is currently reachable, shared via context. A newtype
/// rather than a bare `Signal<bool>` so a second boolean put in context later
/// cannot silently resolve to this one.
#[derive(Clone, Copy)]
struct BackendOnline(Signal<bool>);

/// The wire-contract mismatch the last `/health` poll saw, shared via context.
/// `None` means the two ends agree — or that nothing was compared, because the
/// backend could not be reached (#33).
#[derive(Clone, Copy)]
struct BackendProtocol(Signal<Option<blue2th_proto::ProtocolMismatch>>);

/// Whether the backend has been declared **gone**, shared via context.
///
/// Slower and stricter than [`BackendOnline`], which goes false on the very first
/// missed probe so the status dot reacts at once: this one only flips after
/// [`settings::PROBES_BEFORE_CLEARING`] consecutive failures, and it is what tells
/// the screens to drop the state that backend owned rather than keep showing it as
/// if it were still true.
#[derive(Clone, Copy)]
struct BackendGone(Signal<bool>);

/// The active backend's health, as every gate on screen must agree on it.
///
/// Calls hooks: invoke it once, unconditionally, at the top of a component body.
/// Shared rather than re-derived per component so the status dot, the banner and
/// every disabled control can never disagree about the same backend (#33).
fn use_backend_health() -> settings::BackendHealth {
    let backend_online = use_context::<BackendOnline>().0;
    let backend_protocol = use_context::<BackendProtocol>().0;
    let app_settings = use_context::<SettingsState>().0;
    let paired = app_settings.read().active_token().is_some();
    settings::backend_health(backend_online(), paired, backend_protocol())
}

/// The localised message naming which of the two machines to update. One mapping
/// for the standing banner, the status dot's tooltip and both pairing paths, so
/// their wording cannot drift apart. Pure.
fn protocol_message(mismatch: blue2th_proto::ProtocolMismatch) -> String {
    match mismatch {
        blue2th_proto::ProtocolMismatch::BackendTooOld => {
            rust_i18n::t!("protocol.backend_too_old").to_string()
        },
        blue2th_proto::ProtocolMismatch::BackendTooNew => {
            rust_i18n::t!("protocol.backend_too_new").to_string()
        },
    }
}

/// The app settings (known backends + the active one), shared so the status
/// encart, its quick-switch dropdown and the settings page all read and write the
/// same list. Seeded by `App` from the persisted blob.
#[derive(Clone, Copy)]
struct SettingsState(Signal<settings::AppSettings>);

/// How often the app reconciles the Spotify backend and OAuth states.
const SPOTIFY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the now-playing feed waits before resubscribing to a stream that
/// ended (the backend went down, or restarted).
const SSE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// How often a feed stopped by a 401 looks for a token to try instead. The
/// backend is never called meanwhile: this only reads the app's own settings.
const PAIRING_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait until the active backend's token changes — the user paired again, or
/// switched to another backend.
///
/// The now-playing feed parks here after a 401 (phase 6.4): retrying on a timer
/// would hammer the backend with a credential it has already refused, and simply
/// ending the task would leave the feed dead until the next app start.
async fn await_new_token() {
    let refused = settings::current().active_token();
    loop {
        tokio::time::sleep(PAIRING_RECHECK_INTERVAL).await;
        if settings::current().active_token() != refused {
            return;
        }
    }
}

/// Spotify state shared across the app, provided once by [`App`]: the status
/// card, the login dialog and the transport bar must agree on whether the
/// librespot backend runs and whether the OAuth login is done, and the polling
/// tasks must keep running whichever of them is currently visible.
#[derive(Clone, Copy)]
struct SpotifyUi {
    /// The `librespot` Connect backend is running on the PC (phase 5.1).
    running: Signal<bool>,
    /// The OAuth login is done and the server holds tokens (phase 5.2).
    connected: Signal<bool>,
    /// Latest now-playing snapshot pushed over SSE.
    now_playing: Signal<Option<blue2th_proto::NowPlaying>>,
    /// Whether the login dialog is open.
    show_login: Signal<bool>,
    /// Failure raised by a root background task — the deep-link poll (Spotify
    /// login *and*, since phase 6.4, pairing) and the now-playing feed — mirrored
    /// into the screen's toast, since those tasks run outside any screen and
    /// cannot reach its local signal.
    background_error: Signal<Option<String>>,
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
    #[route("/settings")]
    AppSettingsPage {},
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // Periodically probe the PC backend so the whole app knows whether it is
    // reachable. Shared via context: Home shows a status dot and BackendScan gates
    // its scan button on it. Clearing the list is *not* this signal's job — it
    // reacts to the very first missed probe, and a single one means nothing; that
    // is the slower `BackendGone` verdict below.
    let backend_online: Signal<bool> = use_signal(|| false);
    use_context_provider(|| BackendOnline(backend_online));
    // The same poll answers "can we talk to it at all?": the payload already
    // carries the range, so no second request is needed (#33).
    let backend_protocol: Signal<Option<blue2th_proto::ProtocolMismatch>> = use_signal(|| None);
    use_context_provider(|| BackendProtocol(backend_protocol));
    // The slower verdict the same poll produces: the backend is not just missing
    // a beat, it is gone, and the state it owned must stop being shown as if it
    // were still true.
    let backend_gone: Signal<bool> = use_signal(|| false);
    use_context_provider(|| BackendGone(backend_gone));
    use_hook(|| {
        // Signal<bool> is Copy; the spawned task captures its own handle.
        let mut backend_online = backend_online;
        let mut backend_protocol = backend_protocol;
        let mut backend_gone = backend_gone;
        spawn(async move {
            // Local to this task on purpose: nothing else reads the run length,
            // only the verdict it produces, which is the signal above.
            let mut failures: u32 = 0;
            loop {
                let probe = backend::ping_backend().await;
                let reachable = probe.is_ok();
                let mismatch = match probe {
                    Ok(health) => {
                        blue2th_proto::check_protocol(&health, blue2th_proto::PROTOCOL_VERSION)
                            .err()
                    },
                    // Unreachable: nothing was compared, so the mismatch last
                    // seen says nothing about the backend now — it is cleared.
                    Err(_) => None,
                };
                // Usable means reachable *and* speaking the same contract: the
                // name push below is a write, and writing to a backend whose
                // `/config` may have kept its shape while changing its meaning
                // is exactly the guess this check exists to refuse (#33).
                let was_usable = *backend_online.peek() && backend_protocol.peek().is_none();
                if *backend_protocol.peek() != mismatch {
                    *backend_protocol.write() = mismatch;
                }
                if *backend_online.peek() != reachable {
                    *backend_online.write() = reachable;
                }
                // …and the second, slower verdict, which the dot above never
                // waits for: only a run of consecutive failures declares the
                // backend gone, and `track_probe` says so exactly once so the
                // screens clear once instead of on every later probe.
                if settings::track_probe(&mut failures, reachable) {
                    *backend_gone.write() = true;
                } else if reachable && *backend_gone.peek() {
                    *backend_gone.write() = false;
                }
                // Becoming usable again: re-assert the name the app is the source
                // of truth for. This covers a push that failed while the backend
                // was down, a backend (or app) that restarted since, and one just
                // updated out of an incompatible range — otherwise the Connect
                // device would keep advertising whatever name the server last
                // stored.
                if reachable && mismatch.is_none() && !was_usable {
                    backend::push_active_name().await;
                }
                tokio::time::sleep(BACKEND_HEALTH_INTERVAL).await;
            }
        });
    });

    // Hand the app's runtime to the JNI lifecycle hooks, so the activity can
    // report from a Java thread whether blue2th is on screen, backgrounded or
    // closing — the backend cannot tell a frozen app from a dead one otherwise.
    use_hook(|| {
        spawn(async {
            lifecycle::arm(tokio::runtime::Handle::current());
        });
    });

    // Settings are read once from the phone's storage and shared from the root:
    // every screen must agree on which backend is active.
    let app_settings: Signal<settings::AppSettings> = use_signal(settings::current);
    use_context_provider(|| SettingsState(app_settings));

    // Spotify state lives at the root: the polling tasks below must survive
    // navigation and keep feeding the card, the dialog and the transport bar.
    let spotify_ui = SpotifyUi {
        running: use_signal(|| false),
        connected: use_signal(|| false),
        now_playing: use_signal(|| None),
        show_login: use_signal(|| false),
        background_error: use_signal(|| None),
    };
    use_context_provider(|| spotify_ui);

    // The backend is gone: drop the Spotify state it owned. Keeping it would
    // leave the card green over a `librespot` nobody can reach any more, and the
    // now-playing panel showing a track that stopped. The polls above only ever
    // overwrite these while the backend answers, so nothing else would.
    use_effect(move || {
        if !backend_gone() {
            return;
        }
        let mut running = spotify_ui.running;
        let mut connected = spotify_ui.connected;
        let mut now_playing = spotify_ui.now_playing;
        if *running.peek() {
            *running.write() = false;
        }
        if *connected.peek() {
            *connected.write() = false;
        }
        if now_playing.peek().is_some() {
            *now_playing.write() = None;
        }
    });

    // Reconcile both Spotify states in one task: the librespot subprocess can die
    // server-side, and the OAuth session can expire, so neither is inferred from
    // the last action alone.
    use_hook(|| {
        let mut running = spotify_ui.running;
        let mut connected = spotify_ui.connected;
        spawn(async move {
            loop {
                if *backend_online.peek() {
                    if let Ok(state) = backend::spotify_status().await {
                        let is_running = state.status == blue2th_proto::SpotifyStatus::Running;
                        if *running.peek() != is_running {
                            *running.write() = is_running;
                        }
                    }
                    if let Ok(state) = backend::spotify_auth_status().await {
                        let is_connected =
                            state.status == blue2th_proto::SpotifyAuthStatus::Connected;
                        if *connected.peek() != is_connected {
                            *connected.write() = is_connected;
                        }
                    }
                }
                tokio::time::sleep(SPOTIFY_POLL_INTERVAL).await;
            }
        });
    });

    // Now-playing snapshots pushed over SSE; re-subscribes if the stream ends.
    use_hook(|| {
        let mut now_playing = spotify_ui.now_playing;
        let mut background_error = spotify_ui.background_error;
        spawn(async move {
            loop {
                let outcome = backend::subscribe_now_playing(|np| {
                    *now_playing.write() = Some(np);
                })
                .await;
                // A 401 is terminal for *this* token: reconnecting would spin the
                // loop against a credential the backend has already refused. Say
                // so once, then wait for a different token rather than exiting —
                // the user's next move is to pair again, and a task that returned
                // would only come back on an app restart.
                if let Err(e) = &outcome {
                    if e.is_not_paired() {
                        *background_error.write() = Some(e.to_string());
                        await_new_token().await;
                        continue;
                    }
                }
                // The stream closed (backend down or restarted); retry shortly.
                tokio::time::sleep(SSE_RETRY_DELAY).await;
            }
        });
    });

    let settings_state = use_context::<SettingsState>();

    // Consume the OAuth redirect Android routed to us and exchange its one-time
    // code for tokens. Whatever the outcome, the browser round-trip is over, so
    // the login dialog closes and any failure lands in the toast.
    use_hook(|| {
        let mut connected = spotify_ui.connected;
        let mut show_login = spotify_ui.show_login;
        let mut background_error = spotify_ui.background_error;
        let mut app_settings = settings_state.0;
        spawn(async move {
            loop {
                tokio::time::sleep(DEEP_LINK_POLL_INTERVAL).await;
                let Some(uri) = deep_link::take_pending_deep_link() else {
                    continue;
                };

                // A pairing QR and a Spotify redirect arrive through the same
                // intent, so both are read here — the pair link first, since it
                // is the one that can create the backend everything else needs.
                if let Some(link) = blue2th_proto::parse_pair_link(&uri) {
                    // The contract first: a token minted against a backend the
                    // app cannot talk to would be useless, and the failure has
                    // to name which machine to update.
                    if let Err(e) = backend::check_backend_protocol(&link.url).await {
                        *background_error.write() = Some(match e.protocol_mismatch() {
                            Some(mismatch) => protocol_message(mismatch),
                            None => e.to_string(),
                        });
                        continue;
                    }
                    match backend::pair(&link.url, &link.code).await {
                        Ok(token) => {
                            let mut next = app_settings.peek().clone();
                            match next.upsert_from_pair_link(&link, &token) {
                                Ok(_) => {
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                },
                                Err(e) => *background_error.write() = Some(e.to_string()),
                            }
                        },
                        Err(e) => *background_error.write() = Some(e.to_string()),
                    }
                    continue;
                }

                match deep_link::parse_spotify_callback(&uri) {
                    Some(deep_link::SpotifyCallback::Authorized { code, state }) => {
                        match backend::spotify_auth_callback(&code, &state).await {
                            Ok(authorized) => {
                                *connected.write() = authorized.status
                                    == blue2th_proto::SpotifyAuthStatus::Connected;
                            },
                            Err(e) => *background_error.write() = Some(e.to_string()),
                        }
                    },
                    Some(deep_link::SpotifyCallback::Denied(_)) => {
                        *background_error.write() =
                            Some(rust_i18n::t!("spotify.login_cancelled").to_string());
                    },
                    // Not our callback (e.g. the plain launcher intent): ignore it.
                    None => continue,
                }
                *show_login.write() = false;
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

    // Every control below is also gated on this: an incompatible backend must
    // refuse playback rather than fail on it once the request is out (#33), and
    // an unpaired one has no token for `/play` at all (#37).
    let actionable = settings::backend_actionable(use_backend_health());

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

    // The bar shows one source at a time: Spotify once its backend runs, the
    // local test tone otherwise. Transport stays disabled until the OAuth login
    // is done, since the server would only answer 409.
    let spotify = use_context::<SpotifyUi>();
    let spotify_running = (spotify.running)();
    let spotify_connected = (spotify.connected)();
    let now_playing = (spotify.now_playing)();
    let volume_label = rust_i18n::t!("transport.volume");
    // The vertical volume panel: opened from the icon, closed a few seconds after
    // the last interaction. The token makes each interaction cancel the pending
    // close, so the panel never vanishes mid-drag.
    let mut volume_open = use_signal(|| false);
    let volume_hide_token = use_signal(|| 0u32);
    let schedule_volume_hide = move || {
        let mut token = volume_hide_token;
        let mut volume_open = volume_open;
        let ticket = token().wrapping_add(1);
        *token.write() = ticket;
        spawn(async move {
            tokio::time::sleep(VOLUME_PANEL_TIMEOUT).await;
            // A later interaction bumped the token: that one owns the close.
            if *token.peek() == ticket {
                *volume_open.write() = false;
            }
        });
    };

    let spotify_playing = now_playing
        .as_ref()
        .map(|np| np.state == blue2th_proto::NowPlayingState::Playing)
        .unwrap_or(false);
    let (spotify_toggle_icon, spotify_toggle_label, spotify_toggle_action) = if spotify_playing {
        (
            "⏸",
            rust_i18n::t!("spotify.pause").to_string(),
            backend::SpotifyAction::Pause,
        )
    } else {
        (
            "▶",
            rust_i18n::t!("spotify.play").to_string(),
            backend::SpotifyAction::Play,
        )
    };
    let (meta_title, meta_track, meta_status) = if spotify_running {
        let track = now_playing
            .as_ref()
            .filter(|np| np.state != blue2th_proto::NowPlayingState::Idle)
            .map(|np| {
                let title = np
                    .title
                    .clone()
                    .unwrap_or_else(|| rust_i18n::t!("spotify.unknown_track").to_string());
                match np.artist.as_deref().filter(|a| !a.is_empty()) {
                    Some(artist) => format!("{title} — {artist}"),
                    None => title,
                }
            })
            .unwrap_or_else(|| rust_i18n::t!("spotify.nothing_playing").to_string());
        let state = if !spotify_connected {
            rust_i18n::t!("spotify.not_connected").to_string()
        } else {
            match now_playing.as_ref().map(|np| np.state) {
                Some(blue2th_proto::NowPlayingState::Playing) => {
                    rust_i18n::t!("transport.status_playing").to_string()
                },
                Some(blue2th_proto::NowPlayingState::Paused) => {
                    rust_i18n::t!("transport.status_paused").to_string()
                },
                _ => rust_i18n::t!("transport.status_stopped").to_string(),
            }
        };
        (rust_i18n::t!("spotify.title").to_string(), track, state)
    } else {
        (
            rust_i18n::t!("transport.title").to_string(),
            rust_i18n::t!("transport.track").to_string(),
            status_label.to_string(),
        )
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
                    div { class: "transport-title", "{meta_title}" }
                    div { class: "transport-track", "🎵 {meta_track}" }
                    div { class: "transport-status", "{meta_status}" }
                }
            }
            div { class: "transport-controls",
                // Flexible edge, mirroring the volume block on the right: both
                // grow equally, which centres the buttons in the bar whatever the
                // volume badge's width.
                div { class: "transport-spacer" }
                // With the Spotify backend running, the bar drives Spotify; the
                // local test tone would fight it for the same speakers, so the two
                // control sets are mutually exclusive.
                if spotify_running {
                    SpotifyTransportButton {
                        icon: "⏮".to_string(),
                        label: rust_i18n::t!("spotify.previous").to_string(),
                        action: backend::SpotifyAction::Previous,
                        disabled: !spotify_connected || !actionable,
                        primary: false,
                        error,
                    }
                    // One toggle, driven by the SSE state: two separate buttons
                    // could not show what Spotify is actually doing, so pausing
                    // from the Spotify app left the wrong one highlighted here.
                    SpotifyTransportButton {
                        icon: spotify_toggle_icon.to_string(),
                        label: spotify_toggle_label.clone(),
                        action: spotify_toggle_action,
                        disabled: !spotify_connected || !actionable,
                        primary: true,
                        error,
                    }
                    SpotifyTransportButton {
                        icon: "⏭".to_string(),
                        label: rust_i18n::t!("spotify.next").to_string(),
                        action: backend::SpotifyAction::Next,
                        disabled: !spotify_connected || !actionable,
                        primary: false,
                        error,
                    }
                } else {
                button {
                    class: "transport-btn transport-play",
                    disabled: !has_target || !actionable,
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
                    disabled: !has_target || !actionable,
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
                }
                div { class: "transport-volume",
                    // The slider used to sit inline and horizontal, sharing the
                    // bar's width with the controls — too cramped to aim at. It is
                    // now a vertical panel opened from the icon, closing on its own.
                    button {
                        class: "transport-volume-icon",
                        title: "{volume_label}",
                        aria_label: "{volume_label}",
                        disabled: !has_target || !actionable,
                        onpointerdown: move |e: PointerEvent| e.stop_propagation(),
                        onclick: move |_| {
                            if !has_target {
                                return;
                            }
                            let open = !volume_open();
                            *volume_open.write() = open;
                            if open {
                                schedule_volume_hide();
                            }
                        },
                        span { "🔊" }
                    }
                    if volume_open() {
                        div {
                            class: "transport-volume-panel",
                            onpointerdown: move |e: PointerEvent| e.stop_propagation(),
                            input {
                                class: "transport-volume-slider vertical",
                                r#type: "range",
                                min: "0",
                                max: "1",
                                step: "0.01",
                                value: "{vol_draft}",
                                disabled: !has_target || !actionable,
                                // Keep slider drags from bubbling to the bar's
                                // expand/collapse gesture.
                                onpointerdown: move |e| e.stop_propagation(),
                                oninput: move |e| {
                                    *dragging.write() = true;
                                    if let Ok(v) = e.value().parse::<f32>() {
                                        *vol_draft.write() = v;
                                    }
                                    // Any interaction restarts the auto-close delay.
                                    schedule_volume_hide();
                                },
                                onchange: move |e| {
                                    *dragging.write() = false;
                                    schedule_volume_hide();
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
                        }
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

    // Backend/OAuth state and the login dialog are shared: `App` polls them and
    // the transport bar reads the same signals.
    let spotify = use_context::<SpotifyUi>();
    // Not merely "online": an incompatible backend must refuse to start Spotify
    // rather than fail once the request is out (#33), and an unpaired one would
    // only collect a 401 for trying (#37).
    let actionable = settings::backend_actionable(use_backend_health());

    // In-flight guard so a double tap does not fire two start/stop calls.
    let busy = use_signal(|| false);

    let is_running = (spotify.running)();
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
    let disabled = busy() || !actionable || (!is_running && !has_target);
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
                if busy() || !actionable {
                    return;
                }
                // Guard the start precondition client-side too, so the user gets
                // the message without a round-trip to a 400.
                if !is_running && !has_target {
                    *error.write() = Some(rust_i18n::t!("spotify.no_target").to_string());
                    return;
                }
                let mut running = spotify.running;
                let mut show_login = spotify.show_login;
                let connected = spotify.connected;
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
                        Ok(state) => {
                            let now_running = state.status == SpotifyStatus::Running;
                            *running.write() = now_running;
                            // Starting the backend is only half the story: without
                            // the OAuth login the app can show nothing and drive
                            // nothing, so offer it right away instead of leaving
                            // the user to find a second control.
                            if now_running && !*connected.peek() {
                                *show_login.write() = true;
                            }
                        },
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

/// The state a transport action is expected to leave playback in, or `None` when
/// it does not change it (a skip keeps playing, or keeps paused). Pure.
fn optimistic_now_playing_state(
    action: backend::SpotifyAction,
) -> Option<blue2th_proto::NowPlayingState> {
    match action {
        backend::SpotifyAction::Play => Some(blue2th_proto::NowPlayingState::Playing),
        backend::SpotifyAction::Pause => Some(blue2th_proto::NowPlayingState::Paused),
        backend::SpotifyAction::Next | backend::SpotifyAction::Previous => None,
    }
}

/// The backend status encart: the active backend's name (or `-`), a reachability
/// dot, and — on tap — the quick-switch list of configured backends.
///
/// The switch goes through `backend::activate_backend`, the very same path the
/// settings page uses, so the shortcut can never drift from the page.
#[component]
fn BackendStatus(error: Signal<Option<String>>) -> Element {
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
fn SettingsButton() -> Element {
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
fn AppSettingsPage() -> Element {
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
                                match discovery::browse(discovery::BROWSE_TIMEOUT).await {
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

                    if method == settings::PairingMethod::Code {
                        input {
                            class: "backend-input",
                            r#type: "text",
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
                                    if let Err(e) = backend::check_backend_protocol(&url).await {
                                        *pairing.write() = false;
                                        *notice.write() = None;
                                        *error.write() = Some(match e.protocol_mismatch() {
                                            Some(mismatch) => protocol_message(mismatch),
                                            None => e.to_string(),
                                        });
                                        return;
                                    }
                                    // The one call that carries no bearer: the
                                    // app has none until this succeeds.
                                    let outcome = backend::pair(&url, &code).await;
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
                                    // toggle is meaningless until it knows.
                                    if let Err(e) = backend::push_active_config().await {
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
                                    // is meaningless until it knows.
                                    if let Err(e) = backend::push_active_config().await {
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

/// One Spotify transport button in the bottom bar. Disabled until the OAuth
/// login is done; failures land in the shared toast rather than being dropped.
#[component]
fn SpotifyTransportButton(
    icon: String,
    label: String,
    action: backend::SpotifyAction,
    disabled: bool,
    /// Highlight this button as the main action (the play/pause toggle).
    primary: bool,
    error: Signal<Option<String>>,
) -> Element {
    let spotify = use_context::<SpotifyUi>();
    let class = if primary {
        "transport-btn spotify-transport-btn primary"
    } else {
        "transport-btn spotify-transport-btn"
    };
    rsx! {
        button {
            class: "{class}",
            disabled,
            title: "{label}",
            aria_label: "{label}",
            onclick: move |_| {
                if disabled {
                    return;
                }
                let mut error = error;
                let mut now_playing = spotify.now_playing;
                // Assume the command lands, so the icon flips under the finger.
                // Waiting for the SSE feed to confirm would leave the button up to
                // one poll interval behind the sound, which is what makes it feel
                // laggy — the audio path is far shorter than the state path.
                let previous = now_playing.peek().clone();
                if let Some(state) = optimistic_now_playing_state(action) {
                    if let Some(snapshot) = now_playing.write().as_mut() {
                        snapshot.state = state;
                    }
                }
                spawn(async move {
                    if let Err(e) = backend::spotify_transport(action).await {
                        *error.write() = Some(e.to_string());
                        // It did not land after all: put back what the feed last
                        // reported rather than leaving the icon lying.
                        *now_playing.write() = previous;
                    }
                });
            },
            span { "{icon}" }
        }
    }
}

/// Spotify login dialog (phase 5.2): one action that fetches the PKCE authorize
/// URL and hands it to the system browser. It opens right after the Spotify
/// backend starts while the OAuth login is still missing, and closes as soon as
/// the redirect comes back — successful or not, a failure landing in the toast.
#[component]
fn SpotifyLoginDialog(error: Signal<Option<String>>) -> Element {
    use_locale();
    let spotify = use_context::<SpotifyUi>();
    let busy = use_signal(|| false);

    // Surface a login failure raised by the root deep-link task, which runs
    // outside this screen and cannot reach its toast signal.
    use_effect(move || {
        if let Some(message) = (spotify.background_error)() {
            let mut error = error;
            let mut background_error = spotify.background_error;
            *error.write() = Some(message);
            *background_error.write() = None;
        }
    });

    if !(spotify.show_login)() {
        return rsx! {};
    }

    let close = move |_: MouseEvent| {
        let mut show_login = spotify.show_login;
        *show_login.write() = false;
    };

    rsx! {
        div { class: "spotify-dialog-backdrop", onclick: close,
            div {
                class: "spotify-dialog",
                // A click inside the card must not dismiss the dialog.
                onclick: move |e: MouseEvent| e.stop_propagation(),
                div { class: "spotify-dialog-title", "{rust_i18n::t!(\"spotify.login_title\")}" }
                div { class: "spotify-dialog-hint", "{rust_i18n::t!(\"spotify.login_hint\")}" }
                button {
                    class: "btn-spotify-connect",
                    disabled: busy(),
                    onclick: move |_| {
                        let mut busy = busy;
                        let mut error = error;
                        let mut show_login = spotify.show_login;
                        *busy.write() = true;
                        spawn(async move {
                            match backend::spotify_auth_url().await {
                                Ok(resp) => {
                                    // The consent page cannot run in the app's own
                                    // WebView: only a real browser sends the
                                    // blue2th:// redirect back as an intent.
                                    if !deep_link::open_in_browser(&resp.url) {
                                        *error.write() = Some(
                                            rust_i18n::t!("spotify.login_no_browser").to_string(),
                                        );
                                        *show_login.write() = false;
                                    }
                                },
                                Err(e) => {
                                    *error.write() = Some(e.to_string());
                                    *show_login.write() = false;
                                },
                            }
                            *busy.write() = false;
                        });
                    },
                    span { "🎧 " }
                    "{rust_i18n::t!(\"spotify.connect\")}"
                }
                button {
                    class: "spotify-dialog-cancel",
                    onclick: close,
                    "{rust_i18n::t!(\"spotify.login_cancel\")}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{marks_unavailable, signal_bars, sort_scanned};
    use crate::backend::BackendError;

    /// A scanned (disconnected) device. Built by hand: a fixture must not lean
    /// on the code under test.
    fn scanned(address: &str, paired: bool, rssi: Option<i16>) -> blue2th_proto::DeviceInfo {
        blue2th_proto::DeviceInfo {
            address: address.to_string(),
            name: None,
            paired,
            connected: false,
            rssi,
        }
    }

    /// The list as the user reads it, top to bottom.
    fn order(devices: &[blue2th_proto::DeviceInfo]) -> Vec<&str> {
        devices.iter().map(|d| d.address.as_str()).collect()
    }

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

    // AC: a favourite — a device the backend is already bonded with — is listed
    // above every stranger, however much stronger the stranger's signal. This is
    // the whole point: in a crowded place the user's own speaker was buried.
    #[test]
    fn test_sort_scanned_puts_favourites_above_a_stronger_stranger() {
        let mut devices = vec![
            scanned("STRANGER", false, Some(-35)),
            scanned("MINE", true, Some(-90)),
        ];

        sort_scanned(&mut devices);

        assert_eq!(
            order(&devices),
            vec!["MINE", "STRANGER"],
            "pairing must outrank signal strength"
        );
    }

    // AC: signal strength still orders each group, unknown RSSI last.
    #[test]
    fn test_sort_scanned_orders_within_each_group_by_signal() {
        let mut devices = vec![
            scanned("KNOWN_WEAK", true, Some(-88)),
            scanned("NEW_UNKNOWN_RSSI", false, None),
            scanned("NEW_STRONG", false, Some(-40)),
            scanned("KNOWN_UNKNOWN_RSSI", true, None),
            scanned("KNOWN_STRONG", true, Some(-45)),
        ];

        sort_scanned(&mut devices);

        assert_eq!(
            order(&devices),
            vec![
                "KNOWN_STRONG",
                "KNOWN_WEAK",
                "KNOWN_UNKNOWN_RSSI",
                "NEW_STRONG",
                "NEW_UNKNOWN_RSSI",
            ],
            "favourites first, each group strongest first with an unknown RSSI last"
        );
    }

    // AC: two devices the sort cannot tell apart keep the order the scan found
    // them in — the list must not shuffle under the user on every poll.
    #[test]
    fn test_sort_scanned_is_stable_for_devices_it_cannot_tell_apart() {
        let mut devices = vec![
            scanned("FIRST", true, Some(-60)),
            scanned("SECOND", true, Some(-60)),
        ];

        sort_scanned(&mut devices);

        assert_eq!(order(&devices), vec!["FIRST", "SECOND"]);
    }

    // ---- #52: a refused Bluetooth pairing must not grey the row ----

    // AC: a `BackendError` flagged as a pairing failure does not add the address
    // to the `unavailable` set — the speaker was not in pairing mode, so the row
    // stays clickable for a retry.
    #[test]
    fn test_marks_unavailable_is_false_for_a_pairing_failure() {
        assert!(
            !marks_unavailable(&BackendError::pairing_failed()),
            "a refused pairing says nothing about the speaker being reachable"
        );
    }

    // AC: any other connect failure still marks the address unavailable — a
    // paired speaker that will not connect is the one case where the hardware
    // really is the suspect. This is the behaviour the fix must not regress.
    #[test]
    fn test_marks_unavailable_is_true_for_a_plain_backend_error() {
        let err = BackendError::protocol(blue2th_proto::ProtocolMismatch::BackendTooOld);
        assert!(
            marks_unavailable(&err),
            "every non-pairing failure keeps greying the row"
        );
    }

    // AC: an unpaired *backend* (401) is not a Bluetooth pairing failure, so the
    // two flags must not be conflated into one rule.
    #[test]
    fn test_marks_unavailable_is_true_for_an_unpaired_backend() {
        assert!(
            marks_unavailable(&BackendError::not_paired()),
            "app-to-backend pairing is a different failure from speaker pairing"
        );
    }
}
