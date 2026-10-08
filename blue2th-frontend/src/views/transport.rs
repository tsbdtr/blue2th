// SPDX-License-Identifier: MIT OR Apache-2.0

use super::app::{use_backend_health, use_locale, SpotifyUi};
use super::spotify::SpotifyTransportButton;
use crate::{backend, settings, timer};
use dioxus::prelude::*;

/// Vertical travel (px) past which a drag on the transport handle is treated as
/// an expand/collapse gesture rather than a tap.
const TRANSPORT_DRAG_THRESHOLD_PX: f64 = 24.0;

/// How long the vertical volume panel stays open after the last interaction.
const VOLUME_PANEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Bottom transport bar (phase 3): play/pause, stop, and a PipeWire-sink volume
/// slider. Collapsed it is a thin bar at the bottom of the player stage; expanded
/// it covers the device list (but never the pinned speakers above it). Controls
/// are disabled until a speaker is connected, since `/play` needs a target sink.
#[component]
pub(crate) fn TransportBar(
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
            timer::sleep(VOLUME_PANEL_TIMEOUT).await;
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
                        timer::sleep(std::time::Duration::from_millis(450)).await;
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
