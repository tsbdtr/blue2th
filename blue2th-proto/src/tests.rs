// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

#[test]
fn test_health_status_round_trips_through_json() {
    let original = HealthStatus::ok("0.1.0");
    let json = serde_json::to_string(&original).expect("serialize HealthStatus");
    let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialize HealthStatus");
    assert_eq!(original, parsed);
}

#[test]
fn test_health_status_ok_sets_status_field() {
    let status = HealthStatus::ok("9.9.9");
    assert_eq!(status.status, "ok");
    assert_eq!(status.version, "9.9.9");
}

#[test]
fn test_adapter_info_round_trips_through_json() {
    let original = AdapterInfo {
        name: "hci0".to_string(),
        address: "AA:BB:CC:DD:EE:FF".to_string(),
        powered: true,
        discovering: false,
    };
    let json = serde_json::to_string(&original).expect("serialize AdapterInfo");
    let parsed: AdapterInfo = serde_json::from_str(&json).expect("deserialize AdapterInfo");
    assert_eq!(original, parsed);
}

#[test]
fn test_device_info_round_trips_with_optional_fields() {
    let original = DeviceInfo {
        address: "11:22:33:44:55:66".to_string(),
        name: Some("L3".to_string()),
        paired: true,
        connected: false,
        rssi: Some(-57),
    };
    let json = serde_json::to_string(&original).expect("serialize DeviceInfo");
    let parsed: DeviceInfo = serde_json::from_str(&json).expect("deserialize DeviceInfo");
    assert_eq!(original, parsed);

    // Absent optionals must round-trip too.
    let nameless = DeviceInfo {
        name: None,
        rssi: None,
        ..original
    };
    let json = serde_json::to_string(&nameless).expect("serialize nameless DeviceInfo");
    let parsed: DeviceInfo = serde_json::from_str(&json).expect("deserialize nameless");
    assert_eq!(nameless, parsed);
}

// Criterion: `GET /playback` returns the current `PlaybackState` —
// PlaybackStatus must round-trip through JSON for every variant.
#[test]
fn test_playback_status_round_trips_through_json() {
    for status in [
        PlaybackStatus::Stopped,
        PlaybackStatus::Playing,
        PlaybackStatus::Paused,
    ] {
        let json = serde_json::to_string(&status).expect("serialize PlaybackStatus");
        let parsed: PlaybackStatus =
            serde_json::from_str(&json).expect("deserialize PlaybackStatus");
        assert_eq!(status, parsed);
    }
}

// Criterion: `GET /playback` returns the current `PlaybackState`.
#[test]
fn test_playback_state_round_trips_through_json() {
    let original = PlaybackState {
        status: PlaybackStatus::Playing,
        volume: 0.5,
        audio_graph: AudioGraphStatus::Responsive,
    };
    let json = serde_json::to_string(&original).expect("serialize PlaybackState");
    let parsed: PlaybackState = serde_json::from_str(&json).expect("deserialize PlaybackState");
    assert_eq!(original, parsed);
}

// ─── #145: `PlaybackState::audio_graph` ──────────────────────────────────

// Criterion (#145): `AudioGraphStatus` defaults to `Responsive` — a reply
// that says nothing about the graph is not a stall.
#[test]
fn test_audio_graph_status_default_is_responsive() {
    assert_eq!(AudioGraphStatus::default(), AudioGraphStatus::Responsive);
}

// Criterion (#145): on the wire the two values are `responsive` and
// `unresponsive`, lowercase like the crate's other enums.
#[test]
fn test_audio_graph_status_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&AudioGraphStatus::Responsive).ok(),
        Some("\"responsive\"".to_string())
    );
    assert_eq!(
        serde_json::to_string(&AudioGraphStatus::Unresponsive).ok(),
        Some("\"unresponsive\"".to_string())
    );
}

// Criterion (#145): a body without `audio_graph` — a backend built before
// this change — decodes as `Responsive`. The body is the shape such a
// backend sends, not one the current DTO produced.
#[test]
fn test_playback_state_without_audio_graph_decodes_as_responsive() {
    let body = r#"{"status":"stopped","volume":0.5}"#;

    let parsed = serde_json::from_str::<PlaybackState>(body);

    assert!(
        matches!(
            &parsed,
            Ok(p) if p.audio_graph == AudioGraphStatus::Responsive
                && p.status == PlaybackStatus::Stopped
                && p.volume == 0.5
        ),
        "got {parsed:?}"
    );
}

// Criterion (#145): a body carrying `audio_graph: unresponsive`
// round-trips, and carries the field under that exact name and value.
// `Unresponsive` rather than the default, so a field that is dropped on
// the way out and defaulted on the way in cannot pass.
#[test]
fn test_playback_state_with_an_unresponsive_graph_round_trips_through_json() {
    let original = PlaybackState {
        status: PlaybackStatus::Paused,
        volume: 0.35,
        audio_graph: AudioGraphStatus::Unresponsive,
    };

    let json = serde_json::to_string(&original).expect("serialize PlaybackState");
    let parsed = serde_json::from_str::<PlaybackState>(&json);

    assert!(
        json.contains(r#""audio_graph":"unresponsive""#),
        "wire shape: {json}"
    );
    assert!(matches!(&parsed, Ok(p) if *p == original), "got {parsed:?}");
}

// Criterion (#145): an app built before this change — a DTO without the
// field — still decodes a body that carries it, so `PROTOCOL_VERSION`
// needs no bump. The mirror is that older DTO, field for field.
#[test]
fn test_playback_state_without_the_field_decodes_a_body_that_carries_it() {
    #[derive(Debug, Deserialize)]
    struct PlaybackStateBeforeAudioGraph {
        status: PlaybackStatus,
        volume: f32,
    }
    let body = r#"{"status":"playing","volume":0.4,"audio_graph":"unresponsive"}"#;

    let parsed = serde_json::from_str::<PlaybackStateBeforeAudioGraph>(body);

    assert!(
        matches!(
            &parsed,
            Ok(p) if p.status == PlaybackStatus::Playing && p.volume == 0.4
        ),
        "got {parsed:?}"
    );
}

// Criterion: `POST /volume` body carries the desired level — VolumeRequest
// must round-trip through JSON.
#[test]
fn test_volume_request_round_trips_through_json() {
    let original = VolumeRequest { level: 0.75 };
    let json = serde_json::to_string(&original).expect("serialize VolumeRequest");
    let parsed: VolumeRequest = serde_json::from_str(&json).expect("deserialize VolumeRequest");
    assert_eq!(original, parsed);
}

// Criterion: the new proto DTOs round-trip through serde — SpeakerTarget.
#[test]
fn test_speaker_target_round_trips_through_json() {
    let original = SpeakerTarget {
        address: "AA:BB:CC:DD:EE:FF".to_string(),
        offset_ms: 120,
    };
    let json = serde_json::to_string(&original).expect("serialize SpeakerTarget");
    let parsed: SpeakerTarget = serde_json::from_str(&json).expect("deserialize SpeakerTarget");
    assert_eq!(original, parsed);
}

// Criterion: the new proto DTOs round-trip through serde — RoutingMode, all
// three variants, serialized lowercase.
#[test]
fn test_routing_mode_round_trips_through_json() {
    for mode in [
        RoutingMode::Idle,
        RoutingMode::Single,
        RoutingMode::Combined,
    ] {
        let json = serde_json::to_string(&mode).expect("serialize RoutingMode");
        let parsed: RoutingMode = serde_json::from_str(&json).expect("deserialize RoutingMode");
        assert_eq!(mode, parsed);
    }
}

// Criterion: the routing mode is serialized in lowercase (shared contract).
#[test]
fn test_routing_mode_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&RoutingMode::Combined).expect("serialize"),
        "\"combined\""
    );
    assert_eq!(
        serde_json::to_string(&RoutingMode::Single).expect("serialize"),
        "\"single\""
    );
    assert_eq!(
        serde_json::to_string(&RoutingMode::Idle).expect("serialize"),
        "\"idle\""
    );
}

// Criterion: `GET /targets` returns the current selection, per-speaker offsets
// and routing mode — TargetsState must round-trip through JSON.
#[test]
fn test_targets_state_round_trips_through_json() {
    let original = TargetsState {
        speakers: vec![
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 250,
            },
        ],
        routing: RoutingMode::Combined,
    };
    let json = serde_json::to_string(&original).expect("serialize TargetsState");
    let parsed: TargetsState = serde_json::from_str(&json).expect("deserialize TargetsState");
    assert_eq!(original, parsed);

    // An empty selection (Idle) must round-trip too.
    let idle = TargetsState {
        speakers: vec![],
        routing: RoutingMode::Idle,
    };
    let json = serde_json::to_string(&idle).expect("serialize idle TargetsState");
    let parsed: TargetsState = serde_json::from_str(&json).expect("deserialize idle TargetsState");
    assert_eq!(idle, parsed);
}

// Criterion: the new proto DTOs round-trip through serde — OffsetRequest.
#[test]
fn test_offset_request_round_trips_through_json() {
    let original = OffsetRequest { offset_ms: 750 };
    let json = serde_json::to_string(&original).expect("serialize OffsetRequest");
    let parsed: OffsetRequest = serde_json::from_str(&json).expect("deserialize OffsetRequest");
    assert_eq!(original, parsed);
}

// Criterion: `SpotifyState` DTO round-trips through serde — every status
// variant survives serialize -> deserialize.
#[test]
fn test_spotify_state_round_trips_through_json() {
    for status in [SpotifyStatus::Stopped, SpotifyStatus::Running] {
        let original = SpotifyState {
            status,
            device_name: "blue2th-PC".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize SpotifyState");
        let parsed: SpotifyState = serde_json::from_str(&json).expect("deserialize SpotifyState");
        assert_eq!(original, parsed);
    }
}

// Criterion: `SpotifyState` DTO round-trips through serde — the status is
// serialized in lowercase (shared mobile<->server contract).
#[test]
fn test_spotify_status_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&SpotifyStatus::Running).expect("serialize"),
        "\"running\""
    );
    assert_eq!(
        serde_json::to_string(&SpotifyStatus::Stopped).expect("serialize"),
        "\"stopped\""
    );
}

// Criterion (phase 5.2): `SpotifyAuthState` round-trips through serde for every
// status variant.
#[test]
fn test_spotify_auth_state_round_trips_through_json() {
    for status in [
        SpotifyAuthStatus::Disconnected,
        SpotifyAuthStatus::Connected,
    ] {
        let original = SpotifyAuthState { status };
        let json = serde_json::to_string(&original).expect("serialize SpotifyAuthState");
        let parsed: SpotifyAuthState =
            serde_json::from_str(&json).expect("deserialize SpotifyAuthState");
        assert_eq!(original, parsed);
    }
}

// Criterion (phase 5.2): the auth status serializes lowercase (shared contract).
#[test]
fn test_spotify_auth_status_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&SpotifyAuthStatus::Connected).expect("serialize"),
        "\"connected\""
    );
    assert_eq!(
        serde_json::to_string(&SpotifyAuthStatus::Disconnected).expect("serialize"),
        "\"disconnected\""
    );
}

// Criterion (phase 5.2): the auth request/response DTOs round-trip through serde.
#[test]
fn test_auth_url_response_round_trips_through_json() {
    let original = AuthUrlResponse {
        url: "https://accounts.spotify.com/authorize?response_type=code".to_string(),
        state: "csrf-abc123".to_string(),
    };
    let json = serde_json::to_string(&original).expect("serialize AuthUrlResponse");
    let parsed: AuthUrlResponse = serde_json::from_str(&json).expect("deserialize AuthUrlResponse");
    assert_eq!(original, parsed);
}

// Criterion (phase 5.2): the auth callback request DTO round-trips through serde.
#[test]
fn test_auth_callback_request_round_trips_through_json() {
    let original = AuthCallbackRequest {
        code: "auth-code-xyz".to_string(),
        state: "csrf-abc123".to_string(),
    };
    let json = serde_json::to_string(&original).expect("serialize AuthCallbackRequest");
    let parsed: AuthCallbackRequest =
        serde_json::from_str(&json).expect("deserialize AuthCallbackRequest");
    assert_eq!(original, parsed);
}

// Criterion (phase 5.2): `NowPlayingState` round-trips and serializes lowercase.
#[test]
fn test_now_playing_state_round_trips_and_serializes_lowercase() {
    for state in [
        NowPlayingState::Idle,
        NowPlayingState::Playing,
        NowPlayingState::Paused,
    ] {
        let json = serde_json::to_string(&state).expect("serialize NowPlayingState");
        let parsed: NowPlayingState =
            serde_json::from_str(&json).expect("deserialize NowPlayingState");
        assert_eq!(state, parsed);
    }
    assert_eq!(
        serde_json::to_string(&NowPlayingState::Playing).expect("serialize"),
        "\"playing\""
    );
    assert_eq!(
        serde_json::to_string(&NowPlayingState::Idle).expect("serialize"),
        "\"idle\""
    );
}

// Criterion (phase 5.2): `NowPlaying` round-trips through serde with a full
// track payload (playing) and with an idle payload (all track fields absent).
#[test]
fn test_now_playing_round_trips_through_json() {
    let playing = NowPlaying {
        state: NowPlayingState::Playing,
        title: Some("Song".to_string()),
        artist: Some("Artist".to_string()),
        album: Some("Album".to_string()),
        progress_ms: Some(12_000),
        duration_ms: Some(210_000),
        volume_percent: Some(100),
    };
    let json = serde_json::to_string(&playing).expect("serialize NowPlaying");
    let parsed: NowPlaying = serde_json::from_str(&json).expect("deserialize NowPlaying");
    assert_eq!(playing, parsed);

    // An idle snapshot (nothing playing) must round-trip too.
    let idle = NowPlaying {
        state: NowPlayingState::Idle,
        title: None,
        artist: None,
        album: None,
        progress_ms: None,
        duration_ms: None,
        volume_percent: None,
    };
    let json = serde_json::to_string(&idle).expect("serialize idle NowPlaying");
    let parsed: NowPlaying = serde_json::from_str(&json).expect("deserialize idle NowPlaying");
    assert_eq!(idle, parsed);
}

// Criterion (phase 5.2): the presence report round-trips through serde, so the
// app and the backend agree on the three states the watchdog keys off.
#[test]
fn test_presence_request_round_trips_through_json() {
    for presence in [
        ClientPresence::Foreground,
        ClientPresence::Background,
        ClientPresence::Gone,
    ] {
        let request = PresenceRequest { presence };
        let json = serde_json::to_string(&request).expect("serialize PresenceRequest");
        let parsed: PresenceRequest =
            serde_json::from_str(&json).expect("deserialize PresenceRequest");
        assert_eq!(request, parsed);
    }
    // The wire form stays lowercase, as for the other status enums.
    let json = serde_json::to_string(&PresenceRequest {
        presence: ClientPresence::Background,
    })
    .expect("serialize PresenceRequest");
    assert_eq!(json, r#"{"presence":"background"}"#);
}

// Criterion (phase 6.2 + 6.3): the config DTO (`ServerConfig`) round-trips
// through serde, restore flag included.
#[test]
fn test_server_config_round_trips_through_json() {
    let original = ServerConfig {
        name: "Salon".to_string(),
        restore_during_playback: false,
        auto_reconnect: true,
        spotify_volume_lock: false,
    };
    let json = serde_json::to_string(&original).expect("serialize ServerConfig");
    let parsed: ServerConfig = serde_json::from_str(&json).expect("deserialize ServerConfig");
    assert_eq!(original, parsed);
    assert!(
        json.contains("\"name\":\"Salon\""),
        "the name must stay on the wire, got {json}"
    );
    assert!(
        json.contains("\"restore_during_playback\":false"),
        "the restore flag must be on the wire, got {json}"
    );
}

// Criterion (phase 6.2 + 6.3): the config request DTO (`ConfigRequest`)
// round-trips through serde — it is what the app pushes to `POST /config`.
#[test]
fn test_config_request_round_trips_through_json() {
    let original = ConfigRequest {
        name: "blue2th-PC".to_string(),
        restore_during_playback: true,
        auto_reconnect: true,
        spotify_volume_lock: None,
    };
    let json = serde_json::to_string(&original).expect("serialize ConfigRequest");
    let parsed: ConfigRequest = serde_json::from_str(&json).expect("deserialize ConfigRequest");
    assert_eq!(original, parsed);
    assert!(
        parsed.restore_during_playback,
        "the flag must survive the round-trip, got {json}"
    );
}

// Criterion (phase 6.3): `ConfigRequest` gains `restore_during_playback` with
// `serde(default)` — a phase 6.2 client pushing only a name must still parse
// (non-nominal: old client, new server), and the default is **on**.
#[test]
fn test_config_request_without_the_flag_defaults_to_restoring() {
    let parsed: ConfigRequest =
        serde_json::from_str(r#"{"name":"Salon"}"#).expect("a name-only body must still parse");
    assert_eq!(parsed.name, "Salon");
    assert!(
        parsed.restore_during_playback,
        "the setting defaults to on, so a name-only body must not silently disable restoration"
    );
}

// Criterion (phase 6.3): `ServerConfig` carries the same `serde(default)`, so
// a phase 6.2-era payload (or on-disk store) still decodes, restoration on.
#[test]
fn test_server_config_without_the_flag_defaults_to_restoring() {
    let parsed: ServerConfig = serde_json::from_str(r#"{"name":"blue2th-PC"}"#)
        .expect("a name-only payload must still parse");
    assert_eq!(parsed.name, "blue2th-PC");
    assert!(parsed.restore_during_playback, "the setting defaults to on");
}

// Criterion (phase 6.3): the flag is a real boolean on the wire — an explicit
// `false` is honoured and never overwritten by the default.
#[test]
fn test_config_request_explicit_false_is_honoured() {
    let parsed: ConfigRequest =
        serde_json::from_str(r#"{"name":"Salon","restore_during_playback":false}"#)
            .expect("an explicit flag must parse");
    assert!(
        !parsed.restore_during_playback,
        "an explicit false must survive the default"
    );
}

// ---- phase 6.5: auto-reconnect on the wire ----

// Criterion: `ServerConfig` carries `auto_reconnect` on the wire and
// round-trips through serde.
#[test]
fn test_server_config_round_trips_with_auto_reconnect() {
    for auto_reconnect in [true, false] {
        let original = ServerConfig {
            name: "Salon".to_string(),
            restore_during_playback: true,
            auto_reconnect,
            spotify_volume_lock: false,
        };
        let json = serde_json::to_string(&original).expect("serialize ServerConfig");
        let parsed: ServerConfig = serde_json::from_str(&json).expect("deserialize ServerConfig");
        assert_eq!(original, parsed);
        assert!(
            json.contains(&format!("\"auto_reconnect\":{auto_reconnect}")),
            "the auto-reconnect flag must be on the wire, got {json}"
        );
    }
}

// Criterion: `ConfigRequest` carries `auto_reconnect` on the wire and
// round-trips through serde — it is what the app pushes to `POST /config`.
#[test]
fn test_config_request_round_trips_with_auto_reconnect() {
    for auto_reconnect in [true, false] {
        let original = ConfigRequest {
            name: "Salon".to_string(),
            restore_during_playback: false,
            auto_reconnect,
            spotify_volume_lock: None,
        };
        let json = serde_json::to_string(&original).expect("serialize ConfigRequest");
        let parsed: ConfigRequest = serde_json::from_str(&json).expect("deserialize ConfigRequest");
        assert_eq!(original, parsed);
        assert_eq!(
            parsed.auto_reconnect, auto_reconnect,
            "the flag must survive the round-trip, got {json}"
        );
    }
}

// Criterion (non-nominal: a phase 6.2/6.3 client pushes `/config`): a JSON
// body omitting the field deserializes to `true` — the feature must not
// silently disable itself for an older app.
#[test]
fn test_config_request_without_auto_reconnect_defaults_to_on() {
    let parsed: ConfigRequest =
        serde_json::from_str(r#"{"name":"Salon","restore_during_playback":false}"#)
            .expect("a phase 6.3 body must still parse");
    assert!(
        parsed.auto_reconnect,
        "a body with no auto_reconnect field must leave the feature on"
    );
}

// Criterion: `ServerConfig` carries the same default, so a pre-6.5 payload
// (or on-disk store) decodes with auto-reconnect on.
#[test]
fn test_server_config_without_auto_reconnect_defaults_to_on() {
    let parsed: ServerConfig = serde_json::from_str(r#"{"name":"blue2th-PC"}"#)
        .expect("a name-only payload must still parse");
    assert!(parsed.auto_reconnect, "the setting defaults to on");
}

// Criterion: the flag is a real boolean on the wire — an explicit `false`
// is honoured and never overwritten by the default.
#[test]
fn test_config_request_explicit_false_auto_reconnect_is_honoured() {
    let parsed: ConfigRequest = serde_json::from_str(r#"{"name":"Salon","auto_reconnect":false}"#)
        .expect("an explicit flag must parse");
    assert!(
        !parsed.auto_reconnect,
        "an explicit false must survive the default"
    );
    assert!(
        parsed.restore_during_playback,
        "the phase 6.3 flag keeps its own default"
    );
}

// ---- #58: the Spotify Connect level ----

// Criterion: `NowPlaying` carries `volume_percent` on the wire, and it
// round-trips as the level the poll observed.
#[test]
fn test_now_playing_round_trips_the_volume_percent() {
    let playing = NowPlaying {
        state: NowPlayingState::Playing,
        title: Some("Song".to_string()),
        artist: None,
        album: None,
        progress_ms: None,
        duration_ms: None,
        volume_percent: Some(60),
    };
    let json = serde_json::to_string(&playing).expect("serialize NowPlaying");
    assert!(
        json.contains("\"volume_percent\":60"),
        "the level must be on the wire, got {json}"
    );
    let parsed: NowPlaying = serde_json::from_str(&json).expect("deserialize NowPlaying");
    assert_eq!(parsed.volume_percent, Some(60));
}

// Criterion: `volume_percent` is `serde(default)` — a payload from a backend
// that predates the field decodes with `None`, never 0 (empty is not a level).
#[test]
fn test_now_playing_without_volume_percent_decodes_as_none() {
    let parsed: NowPlaying = serde_json::from_str(
        r#"{"state":"playing","title":"Song","artist":null,"album":null,"progress_ms":null,"duration_ms":null}"#,
    )
    .expect("a payload without the field must still parse");
    assert_eq!(parsed.volume_percent, None);
}

// Criterion: an explicit `null` is `None` too, not an error and not 0.
#[test]
fn test_now_playing_null_volume_percent_decodes_as_none() {
    let parsed: NowPlaying = serde_json::from_str(
        r#"{"state":"idle","title":null,"artist":null,"album":null,"progress_ms":null,"duration_ms":null,"volume_percent":null}"#,
    )
    .expect("a null level must parse");
    assert_eq!(parsed.volume_percent, None);
}

// Criterion: `SpotifyVolumeRequest { percent }` is what `POST /spotify/volume`
// reads — it round-trips, and the field is named `percent` on the wire.
#[test]
fn test_spotify_volume_request_round_trips_through_json() {
    let original = SpotifyVolumeRequest { percent: 60 };
    let json = serde_json::to_string(&original).expect("serialize SpotifyVolumeRequest");
    assert_eq!(json, r#"{"percent":60}"#);
    let parsed: SpotifyVolumeRequest =
        serde_json::from_str(&json).expect("deserialize SpotifyVolumeRequest");
    assert_eq!(original, parsed);
}

// Criterion: `ServerConfig` carries `spotify_volume_lock` on the wire, in
// both directions — it is what `GET /config` reports.
#[test]
fn test_server_config_round_trips_with_spotify_volume_lock() {
    for lock in [true, false] {
        let original = ServerConfig {
            name: "Salon".to_string(),
            restore_during_playback: true,
            auto_reconnect: true,
            spotify_volume_lock: lock,
        };
        let json = serde_json::to_string(&original).expect("serialize ServerConfig");
        let parsed: ServerConfig = serde_json::from_str(&json).expect("deserialize ServerConfig");
        assert_eq!(original, parsed);
        assert!(
            json.contains(&format!("\"spotify_volume_lock\":{lock}")),
            "the lock must be on the wire, got {json}"
        );
    }
}

// Criterion: `ServerConfig.spotify_volume_lock` is `serde(default)` → false
// — an on-disk `name.json` written before #58 loads with the lock off.
#[test]
fn test_server_config_without_the_lock_defaults_to_off() {
    let parsed: ServerConfig =
        serde_json::from_str(r#"{"name":"blue2th-PC","auto_reconnect":false}"#)
            .expect("a payload without the lock must still parse");
    assert!(!parsed.spotify_volume_lock, "the lock defaults to off");
    assert!(
        !parsed.auto_reconnect,
        "the other flags keep their own values"
    );
}

// Criterion: `ConfigRequest` carries `spotify_volume_lock` on the wire — an
// explicit `true` is honoured, and it round-trips.
#[test]
fn test_config_request_round_trips_with_spotify_volume_lock() {
    let original = ConfigRequest {
        name: "Salon".to_string(),
        restore_during_playback: true,
        auto_reconnect: true,
        spotify_volume_lock: Some(true),
    };
    let json = serde_json::to_string(&original).expect("serialize ConfigRequest");
    let parsed: ConfigRequest = serde_json::from_str(&json).expect("deserialize ConfigRequest");
    assert_eq!(original, parsed);

    let explicit: ConfigRequest =
        serde_json::from_str(r#"{"name":"Salon","spotify_volume_lock":true}"#)
            .expect("an explicit lock must parse");
    assert_eq!(
        explicit.spotify_volume_lock,
        Some(true),
        "an explicit true is honoured"
    );
    assert!(
        explicit.auto_reconnect && explicit.restore_during_playback,
        "the phase 6.3/6.5 flags keep their own defaults"
    );
}

// Criterion (non-nominal: old app): a `POST /config` body without the field
// carries no value at all — omission must never turn the guard on *or off*.
#[test]
fn test_config_request_without_the_lock_carries_none() {
    let parsed: ConfigRequest =
        serde_json::from_str(r#"{"name":"Salon"}"#).expect("a name-only body must still parse");
    assert_eq!(
        parsed.spotify_volume_lock, None,
        "a body with no spotify_volume_lock field must not carry a value the backend would apply"
    );
}

// Criterion (phase 6.2): `validate_backend_name` accepts `Salon`, `blue2th-PC`,
// `salon_tv` and `pc2` — a leading ASCII letter then letters/digits/`-`/`_`.
#[test]
fn test_validate_backend_name_accepts_the_allowed_shapes() {
    for name in ["Salon", "blue2th-PC", "salon_tv", "pc2"] {
        assert_eq!(
            validate_backend_name(name),
            Ok(name.to_string()),
            "{name} must be accepted"
        );
    }
}

// Criterion (phase 6.2): an empty or blank name is rejected — an unnamed
// backend would show as nothing at all in the status encart.
#[test]
fn test_validate_backend_name_rejects_empty_and_blank() {
    assert_eq!(validate_backend_name(""), Err(NameError::Empty));
    assert_eq!(validate_backend_name("   "), Err(NameError::Empty));
    assert_eq!(validate_backend_name("\t\n"), Err(NameError::Empty));
}

// Criterion (phase 6.2): a name starting with a digit or a separator is
// rejected (`2salon`, `-salon`, `_salon`).
#[test]
fn test_validate_backend_name_rejects_a_non_letter_start() {
    for name in ["2salon", "-salon", "_salon"] {
        assert_eq!(
            validate_backend_name(name),
            Err(NameError::BadStart),
            "{name} must be rejected: a name starts with a letter"
        );
    }
}

// Criterion (phase 6.2): a space, an accent, an emoji or punctuation is
// rejected — the value becomes a `librespot --name` argv entry and the string
// the Web API device lookup matches on.
#[test]
fn test_validate_backend_name_rejects_disallowed_characters() {
    for name in ["salon tv", "séjour", "salon!", "salon\u{1F3B5}", "sa/lon"] {
        assert_eq!(
            validate_backend_name(name),
            Err(NameError::BadChar),
            "{name} must be rejected: only ASCII letters, digits, - and _"
        );
    }
}

// Criterion (phase 6.2): the cap is `MAX_BACKEND_NAME_LEN` — exactly that many
// characters is accepted, one more is refused.
#[test]
fn test_validate_backend_name_enforces_the_length_cap() {
    let at_cap: String = "a".repeat(MAX_BACKEND_NAME_LEN);
    assert_eq!(validate_backend_name(&at_cap), Ok(at_cap.clone()));

    let over_cap: String = "a".repeat(MAX_BACKEND_NAME_LEN + 1);
    assert_eq!(validate_backend_name(&over_cap), Err(NameError::TooLong));
}

// Criterion (phase 6.2): the validator trims, and the trimmed value is what
// comes back (that is what gets stored and pushed to the backend).
#[test]
fn test_validate_backend_name_returns_the_trimmed_value() {
    assert_eq!(validate_backend_name("  Salon \n"), Ok("Salon".to_string()));
}

// Criterion (phase 6.2): the default name `blue2th-PC` satisfies its own
// validator — a server that never got configured must not hold a name its own
// rule would refuse.
#[test]
fn test_default_backend_name_satisfies_the_validator() {
    assert_eq!(
        validate_backend_name(DEFAULT_BACKEND_NAME),
        Ok(DEFAULT_BACKEND_NAME.to_string())
    );
    assert!(DEFAULT_BACKEND_NAME.len() <= MAX_BACKEND_NAME_LEN);
}

// ---- backend protocol compatibility check (#33) ----

// Criterion: proto — `PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION`
// exist and are both `1`, and `HealthStatus::ok()` fills the range from them
// so a backend cannot forget to announce it.
#[test]
fn test_health_status_announces_the_compiled_protocol_range() {
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(MIN_SUPPORTED_PROTOCOL_VERSION, 1);

    let status = HealthStatus::ok("0.1.0");
    assert_eq!(
        status.protocol, PROTOCOL_VERSION,
        "the backend must announce the contract it was built with"
    );
    assert_eq!(
        status.protocol_min, MIN_SUPPORTED_PROTOCOL_VERSION,
        "the backend must announce the oldest client it still serves"
    );
}

// Criterion: proto — `HealthStatus` carries `protocol` / `protocol_min` and
// round-trips them through serde, with both on the wire.
#[test]
fn test_health_status_round_trips_the_protocol_fields() {
    let original = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        auth_required: true,
        protocol: 7,
        protocol_min: 3,
    };
    let json = serde_json::to_string(&original).expect("serialize HealthStatus");
    assert!(
        json.contains("\"protocol\":7"),
        "the announced contract must be on the wire, got {json}"
    );
    assert!(
        json.contains("\"protocol_min\":3"),
        "the oldest served contract must be on the wire, got {json}"
    );
    let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialize HealthStatus");
    assert_eq!(original, parsed);
}

// Criterion: proto — `check_protocol()` accepts a client version inside the
// range, **bounds included**: an exact match on either end is compatible.
#[test]
fn test_check_protocol_accepts_a_client_on_either_bound() {
    let health = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        auth_required: false,
        protocol: 4,
        protocol_min: 2,
    };
    for client in [2, 3, 4] {
        assert_eq!(
            check_protocol(&health, client),
            Ok(()),
            "client {client} sits inside 2..=4"
        );
    }
}

// Criterion: proto — a client newer than what the backend speaks is
// `BackendTooOld` (the phone was updated first).
#[test]
fn test_check_protocol_rejects_a_client_newer_than_the_backend() {
    let health = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        auth_required: false,
        protocol: 4,
        protocol_min: 2,
    };
    assert_eq!(
        check_protocol(&health, 5),
        Err(ProtocolMismatch::BackendTooOld)
    );
}

// Criterion: proto — a client older than the backend's minimum is
// `BackendTooNew` (the backend dropped support for apps this old).
#[test]
fn test_check_protocol_rejects_a_client_older_than_the_backend_minimum() {
    let health = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        auth_required: false,
        protocol: 4,
        protocol_min: 2,
    };
    assert_eq!(
        check_protocol(&health, 1),
        Err(ProtocolMismatch::BackendTooNew)
    );
}

// Criterion: proto — a payload carrying neither field parses (both read `0`)
// and is rejected as `BackendTooOld`: a backend predating the mechanism is,
// by definition, one to update.
//
// Built from **raw JSON** rather than `HealthStatus::ok()` on purpose: a
// workspace build compiles one `blue2th-proto`, so an in-process fixture
// could never be incompatible with itself.
#[test]
fn test_check_protocol_reads_absent_fields_as_a_backend_too_old() {
    let parsed: HealthStatus = serde_json::from_str(r#"{"status":"ok","version":"0.1.0"}"#)
        .expect("a payload predating the mechanism must still parse");
    assert_eq!(parsed.protocol, 0, "an absent field reads as 0");
    assert_eq!(parsed.protocol_min, 0, "an absent field reads as 0");
    assert_eq!(
        check_protocol(&parsed, PROTOCOL_VERSION),
        Err(ProtocolMismatch::BackendTooOld)
    );
}

// Edge case: a backend announcing an impossible range (`protocol_min` above
// `protocol`) must still yield one actionable verdict rather than an
// arbitrary one. The upper bound is checked first, so a client outside both
// ends is reported as `BackendTooOld` — the reading that points at the
// machine that announced the nonsense.
#[test]
fn test_check_protocol_on_an_inverted_range_names_the_backend() {
    let health = HealthStatus {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        auth_required: false,
        protocol: 2,
        protocol_min: 5,
    };
    assert_eq!(
        check_protocol(&health, 3),
        Err(ProtocolMismatch::BackendTooOld),
        "a client above the maximum is told to update the backend, whatever the minimum says"
    );
    assert_eq!(
        check_protocol(&health, 1),
        Err(ProtocolMismatch::BackendTooNew),
        "below both ends, the minimum is what rejects the client"
    );
}

// ---- phase 6.4: authenticated LAN API with QR or code pairing ----

/// A well-formed link for the round-trip tests, built by hand rather than
/// through `pair_deep_link` so a fixture never depends on the function under
/// test.
const SAMPLE_URL: &str = "http://192.168.1.107:4000";
const SAMPLE_NAME: &str = "blue2th-PC";
const SAMPLE_CODE: &str = "K7M2QX";

// Criterion: proto — `HealthStatus` gains `auth_required`; it round-trips
// through serde with the flag set.
#[test]
fn test_health_status_round_trips_with_auth_required() {
    let original = HealthStatus::ok("0.1.0").with_auth_required(true);
    let json = serde_json::to_string(&original).expect("serialize HealthStatus");
    assert!(
        json.contains("\"auth_required\":true"),
        "the flag must be on the wire, got {json}"
    );
    let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialize HealthStatus");
    assert_eq!(original, parsed);
    assert!(parsed.auth_required);
}

// Criterion: `auth_required` carries `serde(default)` — an old (pre-6.4)
// `/health` payload must still parse, and read as "no authentication" rather
// than failing the app's reachability probe outright.
#[test]
fn test_health_status_without_auth_required_parses_an_old_payload() {
    let parsed: HealthStatus = serde_json::from_str(r#"{"status":"ok","version":"0.1.0"}"#)
        .expect("a pre-6.4 payload must still parse");
    assert_eq!(parsed.status, "ok");
    assert_eq!(parsed.version, "0.1.0");
    assert!(
        !parsed.auth_required,
        "a payload that never mentioned authentication must not claim it"
    );
}

// Criterion: proto — `PairRequest` round-trips through serde.
// Criterion: a hand-typed code survives the phone keyboard — Android
// capitalises the first character only, and the alphabet is upper-case.
#[test]
fn test_normalize_pairing_code_upper_cases_and_trims() {
    for typed in [" k7m2qx ", "K7m2qx", "k7M2Qx\n", "K7M2QX"] {
        assert_eq!(normalize_pairing_code(typed), "K7M2QX", "typed: {typed:?}");
    }
}

#[test]
fn test_pair_request_round_trips_through_json() {
    let original = PairRequest {
        code: SAMPLE_CODE.to_string(),
    };
    let json = serde_json::to_string(&original).expect("serialize PairRequest");
    assert_eq!(json, format!("{{\"code\":\"{SAMPLE_CODE}\"}}"));
    let parsed: PairRequest = serde_json::from_str(&json).expect("deserialize PairRequest");
    assert_eq!(original, parsed);
}

// Criterion: proto — `PairResponse` round-trips through serde.
#[test]
fn test_pair_response_round_trips_through_json() {
    let original = PairResponse {
        token: "3P0kq9-token-value_XyZ".to_string(),
    };
    let json = serde_json::to_string(&original).expect("serialize PairResponse");
    let parsed: PairResponse = serde_json::from_str(&json).expect("deserialize PairResponse");
    assert_eq!(original, parsed);
    assert!(json.contains("\"token\""), "got {json}");
}

// Criterion: `pair_deep_link(url, name, code)` builds the `blue2th://pair?…`
// URL, and `parse_pair_link` reads it back — the server builds exactly what
// the app parses.
#[test]
fn test_pair_deep_link_round_trips_through_parse_pair_link() {
    let link = pair_deep_link(SAMPLE_URL, SAMPLE_NAME, SAMPLE_CODE);
    assert!(
        link.starts_with(PAIR_DEEP_LINK),
        "the link must use the pair deep link prefix, got {link}"
    );
    assert_eq!(
        parse_pair_link(&link),
        Some(PairLink {
            url: SAMPLE_URL.to_string(),
            name: Some(SAMPLE_NAME.to_string()),
            code: SAMPLE_CODE.to_string(),
        })
    );
}

// Criterion: the QR carries the **code**, never the token — the built link
// holds the code and nothing that looks like a long-lived credential.
#[test]
fn test_pair_deep_link_carries_the_code() {
    let link = pair_deep_link(SAMPLE_URL, SAMPLE_NAME, SAMPLE_CODE);
    assert!(
        link.contains(SAMPLE_CODE),
        "the link must carry the pairing code, got {link}"
    );
    assert!(
        !link.contains("token"),
        "the link must never carry a token, got {link}"
    );
}

// Criterion: `parse_pair_link` rejects a link with no `code` — a malformed
// deep link is ignored, never half-applied.
#[test]
fn test_parse_pair_link_rejects_a_missing_code() {
    let uri = format!("{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name=blue2th-PC");
    assert_eq!(parse_pair_link(&uri), None);
}

// Criterion: `parse_pair_link` rejects a link with no `url` — there would be
// no backend to pair with.
#[test]
fn test_parse_pair_link_rejects_a_missing_url() {
    let uri = format!("{PAIR_DEEP_LINK}?name=blue2th-PC&code={SAMPLE_CODE}");
    assert_eq!(parse_pair_link(&uri), None);
}

// Criterion (non-nominal): a bad URL is refused rather than stored as an
// address every later call would fail on.
#[test]
fn test_parse_pair_link_rejects_a_malformed_url() {
    for bad in [
        "192.168.1.107:4000",
        "http%3A%2F%2F",
        "%20",
        "http%3A%2F%2F%2F",
    ] {
        let uri = format!("{PAIR_DEEP_LINK}?url={bad}&name=blue2th-PC&code={SAMPLE_CODE}");
        assert_eq!(
            parse_pair_link(&uri),
            None,
            "{bad} is not a usable backend address"
        );
    }
}

// Criterion (non-nominal): an unrelated intent — the OAuth callback, the bare
// scheme, the launcher intent — is not a pair link and yields `None`.
#[test]
fn test_parse_pair_link_ignores_an_unrelated_uri() {
    for uri in [
        "blue2th://spotify-callback?code=abc&state=xyz",
        "blue2th://",
        "https://example.com/pair?url=http://x&code=ABC123",
        "",
        "not a uri at all",
    ] {
        assert_eq!(parse_pair_link(uri), None, "{uri} must be ignored");
    }
}

// Criterion: the link's values are percent-decoded, since that is how the
// server encodes a URL holding `:` and `/`.
#[test]
fn test_parse_pair_link_decodes_percent_encoded_values() {
    let uri = format!(
        "{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name=blue2th-PC&code={SAMPLE_CODE}"
    );
    let link = parse_pair_link(&uri).expect("a percent-encoded link must parse");
    assert_eq!(link.url, SAMPLE_URL);
    assert_eq!(link.name.as_deref(), Some(SAMPLE_NAME));
    assert_eq!(link.code, SAMPLE_CODE);
}

// Criterion (non-nominal): a hand-mangled escape must be survived, not
// panicked on. A `%` followed by a multi-byte character is the case that
// would panic if the decoder re-sliced the `&str` instead of reading bytes,
// and a truncated escape at the very end is the one that would index past it.
#[test]
fn test_parse_pair_link_survives_a_mangled_percent_escape() {
    for name in ["%é", "%", "%4", "abc%", "%zz", "%C3%A9"] {
        let uri = format!(
            "{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name={name}&code={SAMPLE_CODE}"
        );
        let parsed = parse_pair_link(&uri);
        assert!(parsed.is_some(), "{name} must not stop the link parsing");
        let link = parsed.expect("just asserted");
        assert_eq!(link.url, SAMPLE_URL);
        assert_eq!(link.code, SAMPLE_CODE);
    }
}

// Criterion: a well-formed escape decodes to the character it stands for,
// multi-byte included — the backend name is free text.
#[test]
fn test_parse_pair_link_decodes_a_multibyte_name() {
    let encoded = pair_deep_link(SAMPLE_URL, "Salon d'été", SAMPLE_CODE);
    let link = parse_pair_link(&encoded).expect("an accented name must round-trip");
    assert_eq!(link.name.as_deref(), Some("Salon d'été"));
}

// ---- phase 6.6: find the backend on the network ----

// Criterion: proto — a `DiscoveredBackend { id, name, url }` DTO round-trips
// through JSON, with and without an id.
#[test]
fn test_discovered_backend_round_trips_through_json() {
    let found = DiscoveredBackend {
        id: Some("3P0kq9-XyZ_backend-id".to_string()),
        name: "blue2th-PC".to_string(),
        url: SAMPLE_URL.to_string(),
    };
    let json = serde_json::to_string(&found).expect("serialize DiscoveredBackend");
    let parsed: DiscoveredBackend =
        serde_json::from_str(&json).expect("deserialize DiscoveredBackend");
    assert_eq!(found, parsed);

    // A backend that advertises no id must round-trip too — it is a usable
    // find, matched on its URL.
    let anonymous = DiscoveredBackend { id: None, ..found };
    let json = serde_json::to_string(&anonymous).expect("serialize idless DiscoveredBackend");
    let parsed: DiscoveredBackend = serde_json::from_str(&json).expect("deserialize idless");
    assert_eq!(anonymous, parsed);
}

// Criterion: `SERVICE_TYPE` (`_blue2th._tcp.local.`) and the TXT keys are
// declared once in proto, so server and app cannot drift apart.
#[test]
fn test_service_type_and_txt_keys_are_declared_once_in_proto() {
    assert_eq!(SERVICE_TYPE, "_blue2th._tcp.local.");
    assert_eq!(TXT_KEY_ID, "id");
    assert_eq!(TXT_KEY_NAME, "name");
}

// Criterion: the pure TXT→DTO helper builds the DTO from the TXT records and
// the resolved host/port — the nominal record carries both keys.
#[test]
fn test_discovered_from_txt_reads_the_id_and_the_name() {
    let found = discovered_from_txt(
        SAMPLE_URL,
        &[(TXT_KEY_ID, "backend-id-42"), (TXT_KEY_NAME, "Salon")],
    );
    assert_eq!(
        found,
        DiscoveredBackend {
            id: Some("backend-id-42".to_string()),
            name: "Salon".to_string(),
            url: SAMPLE_URL.to_string(),
        }
    );
}

// Criterion: a missing `name` falls back to `DEFAULT_BACKEND_NAME` — a record
// without one is still listable, it just shows the default label.
#[test]
fn test_discovered_from_txt_falls_back_to_the_default_name() {
    let found = discovered_from_txt(SAMPLE_URL, &[(TXT_KEY_ID, "backend-id-42")]);
    assert_eq!(found.name, DEFAULT_BACKEND_NAME);
    assert_eq!(found.id.as_deref(), Some("backend-id-42"));
}

// Criterion (non-nominal): a service with no `id` TXT record — hand-rolled or
// pre-6.6 — yields `id: None`, which is a fallback to URL matching, never a
// rejection of the find.
#[test]
fn test_discovered_from_txt_without_an_id_is_not_a_rejection() {
    let found = discovered_from_txt(SAMPLE_URL, &[(TXT_KEY_NAME, "Salon")]);
    assert_eq!(found.id, None);
    assert_eq!(found.name, "Salon");
    assert_eq!(found.url, SAMPLE_URL);
}

// Criterion: a blank/whitespace `id` is treated as absent — an empty string
// would otherwise match no entry and look like a brand new machine.
#[test]
fn test_discovered_from_txt_treats_a_blank_id_as_absent() {
    for blank in ["", " ", "\t", "\n  "] {
        let found = discovered_from_txt(
            SAMPLE_URL,
            &[(TXT_KEY_ID, blank), (TXT_KEY_NAME, SAMPLE_NAME)],
        );
        assert_eq!(found.id, None, "a blank id ({blank:?}) is no id at all");
    }
}

// Criterion: a blank/whitespace `name` also falls back to the default label,
// and unknown TXT keys are ignored rather than refused.
#[test]
fn test_discovered_from_txt_ignores_extras_and_blank_names() {
    let found = discovered_from_txt(
        SAMPLE_URL,
        &[
            (TXT_KEY_NAME, "   "),
            (TXT_KEY_ID, "backend-id-42"),
            ("version", "0.1.0"),
        ],
    );
    assert_eq!(found.name, DEFAULT_BACKEND_NAME);
    assert_eq!(found.id.as_deref(), Some("backend-id-42"));
    assert_eq!(found.url, SAMPLE_URL);
}

// Criterion: only `url` and `code` are required — a link with no name still
// parses, and unknown parameters are ignored rather than refused.
#[test]
fn test_parse_pair_link_accepts_a_nameless_link_and_ignores_extras() {
    let uri =
        format!("{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&code={SAMPLE_CODE}&v=2");
    assert_eq!(
        parse_pair_link(&uri),
        Some(PairLink {
            url: SAMPLE_URL.to_string(),
            name: None,
            code: SAMPLE_CODE.to_string(),
        })
    );
}
