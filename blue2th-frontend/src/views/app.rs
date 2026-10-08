// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app_settings::{sync_backend_config, AppSettingsPage};
use super::home::Home;
use crate::{backend, deep_link, lifecycle, settings, timer, MAIN_CSS, TAILWIND_CSS};
use dioxus::prelude::*;

/// How often the app re-checks the PC backend's reachability.
const BACKEND_HEALTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the app checks whether Android handed it a custom-scheme redirect
/// (the Spotify OAuth callback). Short enough that the login feels immediate on
/// return from the browser; the check is a single JNI call when idle.
const DEEP_LINK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether the PC backend is currently reachable, shared via context. A newtype
/// rather than a bare `Signal<bool>` so a second boolean put in context later
/// cannot silently resolve to this one.
#[derive(Clone, Copy)]
pub(crate) struct BackendOnline(pub(crate) Signal<bool>);

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
pub(crate) struct BackendGone(pub(crate) Signal<bool>);

/// The active backend's health, as every gate on screen must agree on it.
///
/// Calls hooks: invoke it once, unconditionally, at the top of a component body.
/// Shared rather than re-derived per component so the status dot, the banner and
/// every disabled control can never disagree about the same backend (#33).
pub(crate) fn use_backend_health() -> settings::BackendHealth {
    let backend_online = use_context::<BackendOnline>().0;
    let backend_protocol = use_context::<BackendProtocol>().0;
    let app_settings = use_context::<SettingsState>().0;
    let paired = app_settings.read().active_token().is_some();
    settings::backend_health(backend_online(), paired, backend_protocol())
}

/// The localised message naming which of the two machines to update. One mapping
/// for the standing banner, the status dot's tooltip and both pairing paths, so
/// their wording cannot drift apart. Pure.
pub(crate) fn protocol_message(mismatch: blue2th_proto::ProtocolMismatch) -> String {
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
pub(crate) struct SettingsState(pub(crate) Signal<settings::AppSettings>);

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
        timer::sleep(PAIRING_RECHECK_INTERVAL).await;
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
pub(crate) struct SpotifyUi {
    /// The `librespot` Connect backend is running on the PC (phase 5.1).
    pub(crate) running: Signal<bool>,
    /// The OAuth login is done and the server holds tokens (phase 5.2).
    pub(crate) connected: Signal<bool>,
    /// Latest now-playing snapshot pushed over SSE.
    pub(crate) now_playing: Signal<Option<blue2th_proto::NowPlaying>>,
    /// Whether the login dialog is open.
    pub(crate) show_login: Signal<bool>,
    /// Failure raised by a root background task — the deep-link poll (Spotify
    /// login *and*, since phase 6.4, pairing) and the now-playing feed — mirrored
    /// into the screen's toast, since those tasks run outside any screen and
    /// cannot reach its local signal.
    pub(crate) background_error: Signal<Option<String>>,
}

// Reads the locale from context, sets the global rust-i18n locale, and subscribes
// the calling component to locale changes so it re-renders when the locale changes.
pub(crate) fn use_locale() {
    let locale = use_context::<Signal<String>>();
    rust_i18n::set_locale(&locale());
}

#[derive(Routable, Clone, PartialEq)]
pub(crate) enum Route {
    #[route("/")]
    Home {},
    #[route("/settings")]
    AppSettingsPage {},
}

#[component]
pub(crate) fn App() -> Element {
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

    // Settings are read once from the app's storage (the phone's, or the
    // browser's `localStorage`) and shared from the root: every screen must
    // agree on which backend is active. Created before the health loop, which
    // writes the backend's config into them in the browser (#160).
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
                // Becoming usable again (#160): sync the config. A change that
                // failed while the backend was down goes out now; otherwise the
                // app adopts what the backend holds, so it never re-imposes a
                // stale copy over another client's change. The first transition
                // after start is also the sync on start.
                if reachable && mismatch.is_none() && !was_usable {
                    // An unpaired app has nothing to sync: the settings page
                    // already says so.
                    if let Err(e) = sync_backend_config(app_settings).await {
                        if !e.is_not_paired() {
                            let mut background_error = spotify_ui.background_error;
                            *background_error.write() = Some(e.to_string());
                        }
                    }
                }
                timer::sleep(BACKEND_HEALTH_INTERVAL).await;
            }
        });
    });

    // Hand the app's runtime to the JNI lifecycle hooks, so the activity can
    // report from a Java thread whether blue2th is on screen, backgrounded or
    // closing — the backend cannot tell a frozen app from a dead one otherwise.
    #[cfg(not(target_arch = "wasm32"))]
    use_hook(|| {
        spawn(async {
            lifecycle::arm(tokio::runtime::Handle::current());
        });
    });
    // The browser reports from the page's own lifecycle events instead (#160).
    #[cfg(target_arch = "wasm32")]
    use_hook(lifecycle::install_page_listeners);

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
                timer::sleep(SPOTIFY_POLL_INTERVAL).await;
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
                timer::sleep(SSE_RETRY_DELAY).await;
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
                timer::sleep(DEEP_LINK_POLL_INTERVAL).await;
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
                    let compatible = match backend::check_backend_protocol(&link.url).await {
                        Ok(compatible) => compatible,
                        Err(e) => {
                            *background_error.write() = Some(match e.protocol_mismatch() {
                                Some(mismatch) => protocol_message(mismatch),
                                None => e.to_string(),
                            });
                            continue;
                        },
                    };
                    match backend::pair(&compatible, &link.code).await {
                        Ok(token) => {
                            let mut next = app_settings.peek().clone();
                            match next.upsert_from_pair_link(&link, &token) {
                                Ok(_) => {
                                    settings::set_current(next.clone());
                                    *app_settings.write() = next;
                                    // Paired: adopt the backend's config, or push
                                    // a change still waiting for it (#160).
                                    if let Err(e) = sync_backend_config(app_settings).await {
                                        *background_error.write() = Some(e.to_string());
                                    }
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
