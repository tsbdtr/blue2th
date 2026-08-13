// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for the Spotify OAuth redirect parsing (phase 5.2). The JNI reader is
//! Android-only and manual; the parsing it feeds is pure and covered here.

use blue2th::deep_link::{parse_spotify_callback, take_pending_deep_link, SpotifyCallback};

#[test]
fn test_parse_spotify_callback_extracts_code_and_state() {
    let parsed = parse_spotify_callback("blue2th://spotify-callback?code=AQD123abc&state=xyz789");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Authorized {
            code: "AQD123abc".to_string(),
            state: "xyz789".to_string(),
        })
    );
}

#[test]
fn test_parse_spotify_callback_accepts_reversed_and_extra_params() {
    let parsed =
        parse_spotify_callback("blue2th://spotify-callback?state=xyz789&foo=bar&code=AQD123abc");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Authorized {
            code: "AQD123abc".to_string(),
            state: "xyz789".to_string(),
        })
    );
}

#[test]
fn test_parse_spotify_callback_tolerates_trailing_slash_and_fragment() {
    let parsed = parse_spotify_callback("blue2th://spotify-callback/?code=abc&state=def#done");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Authorized {
            code: "abc".to_string(),
            state: "def".to_string(),
        })
    );
}

#[test]
fn test_parse_spotify_callback_percent_decodes_values() {
    let parsed = parse_spotify_callback("blue2th://spotify-callback?code=a%2Fb%2Bc&state=s%20t");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Authorized {
            code: "a/b+c".to_string(),
            state: "s t".to_string(),
        })
    );
}

#[test]
fn test_parse_spotify_callback_malformed_escape_does_not_panic() {
    // A stray `%` — including one before a multi-byte character — is kept as-is
    // rather than splitting the string and panicking.
    let parsed = parse_spotify_callback("blue2th://spotify-callback?code=a%zz&state=%aé");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Authorized {
            code: "a%zz".to_string(),
            state: "%aé".to_string(),
        })
    );
}

#[test]
fn test_parse_spotify_callback_denial_is_reported() {
    let parsed =
        parse_spotify_callback("blue2th://spotify-callback?error=access_denied&state=xyz789");
    assert_eq!(
        parsed,
        Some(SpotifyCallback::Denied("access_denied".to_string()))
    );
}

#[test]
fn test_parse_spotify_callback_ignores_foreign_uri() {
    assert_eq!(
        parse_spotify_callback("https://example.com/spotify-callback?code=abc&state=def"),
        None
    );
    assert_eq!(
        parse_spotify_callback("blue2th://other?code=abc&state=def"),
        None
    );
}

#[test]
fn test_parse_spotify_callback_without_query_is_none() {
    // The plain launcher intent carries no query — it must not be mistaken for a
    // redirect, and must not panic.
    assert_eq!(parse_spotify_callback("blue2th://spotify-callback"), None);
}

#[test]
fn test_parse_spotify_callback_missing_state_is_none() {
    // A code without the CSRF state cannot be exchanged; ignore it rather than
    // posting an incomplete callback to the backend.
    assert_eq!(
        parse_spotify_callback("blue2th://spotify-callback?code=AQD123abc"),
        None
    );
}

#[test]
fn test_take_pending_deep_link_is_none_off_android() {
    // The desktop/test build has no Android intent to consume.
    assert_eq!(take_pending_deep_link(), None);
}
