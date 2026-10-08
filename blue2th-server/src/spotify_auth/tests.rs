// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use blue2th_proto::NowPlayingState;

// Criterion: `pkce_challenge(verifier)` yields base64url(sha256(verifier)) with
// no padding — the RFC 7636 Appendix B known vector.
#[test]
fn test_pkce_challenge_matches_known_vector() {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    assert_eq!(pkce_challenge(verifier), expected);
}

// Criterion: the PKCE challenge carries no base64 padding (`=`).
#[test]
fn test_pkce_challenge_has_no_padding() {
    assert!(!pkce_challenge("some-verifier-value-1234567890").contains('='));
}

// Criterion: `build_authorize_url` targets accounts.spotify.com/authorize with
// response_type=code and code_challenge_method=S256.
#[test]
fn test_build_authorize_url_has_authorize_endpoint_and_pkce_params() {
    let url = build_authorize_url(
        "client-123",
        "blue2th://spotify-callback",
        "challenge-xyz",
        SPOTIFY_SCOPES,
        "state-abc",
    );
    assert!(
        url.contains("accounts.spotify.com/authorize"),
        "url must target the authorize endpoint: {url}"
    );
    assert!(
        url.contains("response_type=code"),
        "url must request an authorization code: {url}"
    );
    assert!(
        url.contains("code_challenge_method=S256"),
        "url must use the S256 PKCE method: {url}"
    );
}

// Criterion: `build_authorize_url` embeds the client id, challenge and state.
#[test]
fn test_build_authorize_url_embeds_client_id_challenge_and_state() {
    let url = build_authorize_url(
        "client-123",
        "blue2th://spotify-callback",
        "challenge-xyz",
        SPOTIFY_SCOPES,
        "state-abc",
    );
    assert!(
        url.contains("client-123"),
        "url must carry the client id: {url}"
    );
    assert!(
        url.contains("challenge-xyz"),
        "url must carry the challenge: {url}"
    );
    assert!(
        url.contains("state-abc"),
        "url must carry the CSRF state: {url}"
    );
}

// Criterion: `build_authorize_url` URL-encodes the custom-scheme redirect uri
// (the `:` / `/` must not appear raw in the query).
#[test]
fn test_build_authorize_url_encodes_redirect_uri() {
    let url = build_authorize_url(
        "client-123",
        "blue2th://spotify-callback",
        "challenge-xyz",
        SPOTIFY_SCOPES,
        "state-abc",
    );
    assert!(
        url.contains("blue2th%3A%2F%2Fspotify-callback"),
        "redirect uri must be URL-encoded: {url}"
    );
}

// Criterion: a playing `/me/player` fixture maps to a Playing NowPlaying with
// title, artist, progress and duration.
#[test]
fn test_parse_now_playing_maps_playing_fixture() {
    let body = r#"{
            "is_playing": true,
            "progress_ms": 12000,
            "item": {
                "name": "Song",
                "duration_ms": 210000,
                "artists": [{ "name": "Artist" }],
                "album": { "name": "Album" }
            }
        }"#;
    let np = parse_now_playing(body);
    assert_eq!(np.state, NowPlayingState::Playing);
    assert_eq!(np.title.as_deref(), Some("Song"));
    assert_eq!(np.artist.as_deref(), Some("Artist"));
    assert_eq!(np.progress_ms, Some(12_000));
    assert_eq!(np.duration_ms, Some(210_000));
}

// Criterion: a paused `/me/player` fixture maps to a Paused NowPlaying.
#[test]
fn test_parse_now_playing_maps_paused_fixture() {
    let body = r#"{
            "is_playing": false,
            "progress_ms": 3000,
            "item": {
                "name": "Track",
                "duration_ms": 180000,
                "artists": [{ "name": "Band" }],
                "album": { "name": "Record" }
            }
        }"#;
    let np = parse_now_playing(body);
    assert_eq!(np.state, NowPlayingState::Paused);
    assert_eq!(np.title.as_deref(), Some("Track"));
}

// Criterion: an empty body (Web API 204 / no active device) maps to Idle.
#[test]
fn test_parse_now_playing_empty_body_is_idle() {
    let np = parse_now_playing("");
    assert_eq!(np.state, NowPlayingState::Idle);
    assert_eq!(np.title, None);
    assert_eq!(np.artist, None);
}

// Edge case: a 200 body with no `item` (Web API returns `{}` when nothing is
// loaded / no active device) maps to Idle rather than a title-less Playing.
#[test]
fn test_parse_now_playing_no_item_is_idle() {
    let np = parse_now_playing("{}");
    assert_eq!(np.state, NowPlayingState::Idle);
    assert_eq!(np.title, None);
    assert_eq!(np.artist, None);
}

// Edge case: a malformed body is treated as Idle rather than propagated as an
// error, so a transient bad payload never breaks the now-playing SSE feed.
#[test]
fn test_parse_now_playing_malformed_body_is_idle() {
    let np = parse_now_playing("not json at all");
    assert_eq!(np.state, NowPlayingState::Idle);
}

// Criterion (#58): `parse_now_playing` fills `volume_percent` from
// `device.volume_percent`, so the app can show the Connect level.
#[test]
fn test_parse_now_playing_reads_the_device_volume() {
    let body = r#"{
            "is_playing": true,
            "progress_ms": 12000,
            "device": {
                "id": "abc",
                "is_active": true,
                "name": "blue2th-PC",
                "volume_percent": 60
            },
            "item": {
                "name": "Song",
                "duration_ms": 210000,
                "artists": [{ "name": "Artist" }],
                "album": { "name": "Album" }
            }
        }"#;
    let np = parse_now_playing(body);
    assert_eq!(np.state, NowPlayingState::Playing);
    assert_eq!(np.volume_percent, Some(60));
}

// Criterion (#58): a level of 0 is a real level, not "unset" — the parser
// must carry it as `Some(0)`, never fold it into `None`.
#[test]
fn test_parse_now_playing_reads_a_zero_device_volume() {
    let body = r#"{
            "is_playing": false,
            "device": { "id": "abc", "volume_percent": 0 },
            "item": { "name": "Song" }
        }"#;
    assert_eq!(parse_now_playing(body).volume_percent, Some(0));
}

// Criterion (#58): no `device` object at all → `None`, never 0.
#[test]
fn test_parse_now_playing_without_a_device_has_no_volume() {
    let body = r#"{
            "is_playing": true,
            "item": { "name": "Song", "duration_ms": 1000 }
        }"#;
    let np = parse_now_playing(body);
    assert_eq!(np.state, NowPlayingState::Playing);
    assert_eq!(np.volume_percent, None);
}

// Criterion (#58): a `null` field (the Web API reports it as nullable) and a
// `null` device both read as `None`; an empty body (204) too.
#[test]
fn test_parse_now_playing_null_device_volume_is_none() {
    let with_null_field = r#"{
            "is_playing": true,
            "device": { "id": "abc", "volume_percent": null },
            "item": { "name": "Song" }
        }"#;
    assert_eq!(parse_now_playing(with_null_field).volume_percent, None);

    let with_null_device = r#"{
            "is_playing": true,
            "device": null,
            "item": { "name": "Song" }
        }"#;
    assert_eq!(parse_now_playing(with_null_device).volume_percent, None);

    assert_eq!(parse_now_playing("").volume_percent, None);
}

// Rule: a level above 100 is not one the Web API documents, and it is not
// one the policy may adopt — a remembered 200 would be written back after
// a respawn, refused by the API, and retried at every poll. Dropped to
// `None` at the parser, like an absent field.
#[test]
fn test_parse_now_playing_drops_a_device_volume_above_100() {
    for level in ["101", "200", "1000000000000"] {
        let body = format!(
            r#"{{"is_playing": true, "device": {{"id": "abc", "volume_percent": {level}}}, "item": {{"name": "Song"}}}}"#
        );
        let np = parse_now_playing(&body);
        assert_eq!(np.volume_percent, None, "{level} % must not be a level");
        assert_eq!(
            np.title.as_deref(),
            Some("Song"),
            "the rest of the snapshot must survive a bad level"
        );
    }
}

// Criterion (#58): a device that is present on an idle body (no `item`)
// still reports its level — the poll needs it to restore after a respawn
// even while nothing is playing.
#[test]
fn test_parse_now_playing_reads_the_device_volume_while_idle() {
    let body = r#"{
            "is_playing": false,
            "device": { "id": "abc", "volume_percent": 100 }
        }"#;
    let np = parse_now_playing(body);
    assert_eq!(np.state, NowPlayingState::Idle);
    assert_eq!(np.volume_percent, Some(100));
}

// Criterion: `needs_refresh` is true once `now` is past `expires_at`.
#[test]
fn test_needs_refresh_true_when_expired() {
    assert!(needs_refresh(100, 200, 10));
}

// Criterion: `needs_refresh` is true within the skew window before expiry.
#[test]
fn test_needs_refresh_true_within_skew_window() {
    // Expires at 100, now 95, skew 10 -> 95 + 10 >= 100 -> true.
    assert!(needs_refresh(100, 95, 10));
}

// Criterion: `needs_refresh` is false while the token is comfortably valid.
#[test]
fn test_needs_refresh_false_when_comfortably_valid() {
    assert!(!needs_refresh(1000, 100, 30));
}

// Criterion: 401 maps to Unauthorized (reauth / Disconnected).
#[test]
fn test_map_api_status_401_is_unauthorized() {
    assert!(matches!(
        map_api_status(401, None),
        SpotifyApiError::Unauthorized
    ));
}

// Criterion: the blue2th-PC device is resolved by name, with its id and
// active flag, so transport can target it instead of the active device.
#[test]
fn test_find_device_resolves_blue2th_pc_by_name() {
    let body = r#"{"devices":[
            {"id":"phone-id","name":"Pixel 7","is_active":true,"type":"Smartphone"},
            {"id":"pc-id","name":"blue2th-PC","is_active":false,"type":"Computer"}
        ]}"#;
    assert_eq!(
        find_device(body, SPOTIFY_DEVICE_NAME),
        Some(Device {
            id: "pc-id".to_string(),
            is_active: false,
        })
    );
}

// Criterion: an active blue2th-PC is reported as such, so no needless
// playback transfer is issued before the command.
#[test]
fn test_find_device_reports_active_device() {
    let body = r#"{"devices":[{"id":"pc-id","name":"blue2th-PC","is_active":true}]}"#;
    assert_eq!(
        find_device(body, SPOTIFY_DEVICE_NAME),
        Some(Device {
            id: "pc-id".to_string(),
            is_active: true,
        })
    );
}

// Criterion: blue2th-PC absent from the list (librespot not running) yields
// None, which the caller maps to BackendNotRunning rather than guessing.
#[test]
fn test_find_device_absent_is_none() {
    let body = r#"{"devices":[{"id":"phone-id","name":"Pixel 7","is_active":true}]}"#;
    assert_eq!(find_device(body, SPOTIFY_DEVICE_NAME), None);
}

// Criterion: a device still initialising carries a null id and cannot be
// targeted; a malformed body must not panic either.
#[test]
fn test_find_device_null_id_or_malformed_body_is_none() {
    let null_id = r#"{"devices":[{"id":null,"name":"blue2th-PC","is_active":false}]}"#;
    assert_eq!(find_device(null_id, SPOTIFY_DEVICE_NAME), None);
    assert_eq!(find_device("not json", SPOTIFY_DEVICE_NAME), None);
    assert_eq!(find_device("{}", SPOTIFY_DEVICE_NAME), None);
}

// Criterion (phase 6.2): the Web API device lookup uses the *configured*
// name. A driver that never got configured still searches for the default.
#[test]
fn test_device_name_defaults_to_the_spotify_device_name() {
    let auth = SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string());
    assert_eq!(auth.device_name(), SPOTIFY_DEVICE_NAME);
}

// Criterion (phase 6.2): once the app renames the backend, the lookup follows.
// This is the 412 trap: `librespot` advertises `Salon` while a hard-coded
// lookup still searches for `blue2th-PC`, and transport silently fails.
#[test]
fn test_device_lookup_follows_the_configured_name() {
    let mut auth = SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string());
    auth.set_device_name("Salon");
    assert_eq!(auth.device_name(), "Salon");

    // What librespot advertises after the rename: only `Salon` is there.
    let body = r#"{"devices":[
            {"id":"phone-id","name":"Pixel 7","is_active":true,"type":"Smartphone"},
            {"id":"pc-id","name":"Salon","is_active":false,"type":"Computer"}
        ]}"#;
    assert_eq!(
        find_device(body, auth.device_name()),
        Some(Device {
            id: "pc-id".to_string(),
            is_active: false,
        }),
        "the lookup must find the renamed Connect device"
    );
    assert_eq!(
        find_device(body, SPOTIFY_DEVICE_NAME),
        None,
        "the constant must no longer be what transport searches for"
    );
}

// Criterion: a saved refresh token is read back, so a server restart restores
// Connected instead of sending the user through the browser again.
#[test]
fn test_refresh_token_round_trips_through_the_store() {
    let path = std::env::temp_dir()
        .join("blue2th-test-token-roundtrip")
        .join(TOKEN_STORE_FILE);
    let _ = std::fs::remove_file(&path);

    save_refresh_token(&path, "AQD-refresh-token").expect("save the refresh token");
    assert_eq!(
        load_refresh_token(Some(&path)),
        Some("AQD-refresh-token".to_string())
    );

    // The credential must not be world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path)
            .expect("stat the token store")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "token store must be owner-only, got {mode:o}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

// Criterion: a missing, empty or malformed store reads as "not logged in"
// rather than failing startup.
#[test]
fn test_load_refresh_token_tolerates_missing_or_malformed_store() {
    let dir = std::env::temp_dir().join("blue2th-test-token-malformed");
    std::fs::create_dir_all(&dir).expect("create the test dir");
    let missing = dir.join("absent.json");
    let _ = std::fs::remove_file(&missing);
    assert_eq!(load_refresh_token(Some(&missing)), None);
    assert_eq!(load_refresh_token(None), None);

    let malformed = dir.join("malformed.json");
    std::fs::write(&malformed, "not json").expect("write the malformed store");
    assert_eq!(load_refresh_token(Some(&malformed)), None);

    let empty = dir.join("empty.json");
    std::fs::write(&empty, r#"{"refresh_token":""}"#).expect("write the empty store");
    assert_eq!(load_refresh_token(Some(&empty)), None);
    let _ = std::fs::remove_dir_all(&dir);
}

// Criterion: an explicitly configured driver never touches the disk, so tests
// can neither read nor delete the real credential.
#[test]
fn test_with_config_driver_has_no_token_store() {
    let auth = SpotifyAuth::with_config(Some("id".to_string()), "blue2th://cb".to_string());
    assert!(auth.store.is_none(), "with_config must stay off-disk");
}

// Criterion: an unconfigured driver refuses every OAuth path up front.
#[test]
fn test_authorize_url_without_client_id_is_not_configured() {
    let mut auth = SpotifyAuth::with_config(None, DEFAULT_REDIRECT_URI.to_string());
    assert!(matches!(
        auth.authorize_url(),
        Err(SpotifyApiError::NotConfigured)
    ));
}

// Criterion: 403 maps to PremiumRequired.
#[test]
fn test_map_api_status_403_is_premium_required() {
    assert!(matches!(
        map_api_status(403, None),
        SpotifyApiError::PremiumRequired
    ));
}

// Criterion: 404 / no device maps to NoActiveDevice.
#[test]
fn test_map_api_status_404_is_no_active_device() {
    assert!(matches!(
        map_api_status(404, None),
        SpotifyApiError::NoActiveDevice
    ));
}

// Criterion: 429 maps to RateLimited carrying the Retry-After seconds.
#[test]
fn test_map_api_status_429_is_rate_limited_with_retry_after() {
    assert!(matches!(
        map_api_status(429, Some(5)),
        SpotifyApiError::RateLimited(Some(5))
    ));
}
