// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app::{use_backend_health, use_locale, SpotifyUi};
use crate::{backend, deep_link, settings};
use dioxus::prelude::*;

/// Spotify source control (phase 5.1): activate/deactivate the `librespot`
/// backend on the PC. Streaming and transport are driven by the official Spotify
/// app (pick `blue2th-PC` as the device); this only toggles the Connect backend
/// and shows its state. The start action is disabled with no target selected,
/// mirroring the server's 400 precondition, and errors surface via the shared
/// `error` toast signal.
#[component]
pub(crate) fn SpotifySource(
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

/// One Spotify transport button in the bottom bar. Disabled until the OAuth
/// login is done; failures land in the shared toast rather than being dropped.
#[component]
pub(crate) fn SpotifyTransportButton(
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
pub(crate) fn SpotifyLoginDialog(error: Signal<Option<String>>) -> Element {
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
