// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

/// The backend's own key hash in these tests.
const OWN: [u8; 32] = [0x11; 32];

/// A relay's key hash: well-formed, and not the backend's.
const RELAY: [u8; 32] = [0x22; 32];

/// The armed code, as the server minted it.
const CODE: &str = "K7M2QX";

/// The plain-HTTP label copied into a 32-byte array, zero-padded: the hash an
/// implementation that stuffed the label into an SPKI slot would produce.
fn label_as_spki() -> [u8; 32] {
    let mut padded = [0u8; 32];
    for (slot, byte) in padded.iter_mut().zip(PLAIN_HTTP_LABEL.as_bytes()) {
        *slot = *byte;
    }
    padded
}

/// Run the exchange: the app on `app_code`, the backend on `server_code`.
/// Both finishes are expected to succeed — SPAKE2 does not detect a wrong
/// code, the keys simply differ.
fn exchange(app_code: &str, server_code: &str) -> (SharedKey, SharedKey) {
    let (client, client_msg) = ClientHalf::start(app_code);
    let (server, server_msg) = ServerHalf::start(server_code);
    let client_key = client
        .finish(&server_msg)
        .expect("the app's half finishes on a well-formed message");
    let server_key = server
        .finish(&client_msg)
        .expect("the backend's half finishes on a well-formed message");
    (client_key, server_key)
}

// Criterion: client and server compute the same bytes through
// `blue2th_proto::pake` — one test runs the client side against the server
// side and pins that they agree: the same key, and each side accepts the
// other's confirmation over the same binding.
#[test]
fn test_client_and_server_halves_agree_on_the_right_code() {
    let (client_key, server_key) = exchange(CODE, CODE);
    let binding = Binding::Spki(OWN);

    assert_eq!(client_key.as_bytes(), server_key.as_bytes());
    assert!(
        check_confirm_a(&server_key, &binding, &confirm_a(&client_key, &binding)),
        "the backend must accept the app's confirmation"
    );
    assert!(
        check_confirm_b(&client_key, &binding, &confirm_b(&server_key, &binding)),
        "the app must accept the backend's confirmation"
    );
}

// Criterion: a `finish` computed with a wrong code is refused — the two
// halves derive different keys, so the app's confirmation fails the
// backend's check.
#[test]
fn test_a_wrong_code_derives_another_key_and_fails_the_check() {
    let (client_key, server_key) = exchange("K7M2QY", CODE);
    let binding = Binding::Spki(OWN);

    assert_ne!(client_key.as_bytes(), server_key.as_bytes());
    assert!(
        !check_confirm_a(&server_key, &binding, &confirm_a(&client_key, &binding)),
        "a confirmation under another key must be refused"
    );
}

// Criterion (guard, relay): the right code, but the app's confirmation was
// computed over another key hash than the backend's own. The keys agree — a
// code check alone would pass it — and only the binding check refuses it.
#[test]
fn test_a_confirmation_over_another_key_hash_is_refused() {
    let (client_key, server_key) = exchange(CODE, CODE);
    assert_eq!(
        client_key.as_bytes(),
        server_key.as_bytes(),
        "the relay holds the right code: the keys agree"
    );

    let own = Binding::Spki(OWN);
    let relayed = confirm_a(&client_key, &Binding::Spki(RELAY));
    assert!(
        check_confirm_a(&server_key, &own, &confirm_a(&client_key, &own)),
        "over the backend's own key hash, the confirmation passes"
    );
    assert!(
        !check_confirm_a(&server_key, &own, &relayed),
        "over the relay's key hash, it must be refused"
    );
}

// Criterion (guard, the label binding): a confirmation computed over
// `blue2th-plain-http` is refused where an SPKI binding is expected — the
// all-zero hash and the label zero-padded into a hash included, the two
// encodings an implementation could confuse it with.
#[test]
fn test_the_plain_http_label_is_refused_where_a_key_hash_is_expected() {
    let (client_key, server_key) = exchange(CODE, CODE);
    let over_label = confirm_a(&client_key, &Binding::PlainHttp);

    assert!(
        check_confirm_a(&server_key, &Binding::PlainHttp, &over_label),
        "in plain-HTTP mode both sides bind to the label and agree"
    );
    for hash in [OWN, [0u8; 32], label_as_spki()] {
        assert!(
            !check_confirm_a(&server_key, &Binding::Spki(hash), &over_label),
            "a TLS backend ({hash:02x?}) must refuse a confirmation over the label"
        );
    }
}

// Criterion: the PAKE password is the code after `normalize_pairing_code`,
// on both sides — a code typed in lower case, with stray spaces, pairs.
#[test]
fn test_a_lower_case_code_agrees_with_the_minted_one() {
    let (client_key, server_key) = exchange("  k7m2qx ", CODE);
    assert_eq!(client_key.as_bytes(), server_key.as_bytes());

    let (client_key, server_key) = exchange(CODE, "k7M2qX");
    assert_eq!(client_key.as_bytes(), server_key.as_bytes());
}

// Criterion: domain-separated MAC inputs, a distinct label per
// confirmation. Near-miss: one MAC for both, which lets a relay hand the
// app's confirmation back as the backend's.
#[test]
fn test_the_two_confirmations_differ_under_one_key() {
    let (client_key, server_key) = exchange(CODE, CODE);
    let binding = Binding::Spki(OWN);
    let app = confirm_a(&client_key, &binding);

    assert_ne!(app, confirm_b(&server_key, &binding));
    assert!(
        check_confirm_a(&server_key, &binding, &app),
        "the app's confirmation passes as the app's"
    );
    assert!(
        !check_confirm_b(&client_key, &binding, &app),
        "the app's own confirmation reflected back must not pass as the backend's"
    );
}

// Criterion: SPAKE2 values — 33-byte messages, a 32-byte key; a
// confirmation is `CONFIRM_LEN` bytes.
#[test]
fn test_messages_keys_and_confirmations_have_their_pinned_lengths() {
    assert_eq!(MESSAGE_LEN, 33);
    assert_eq!(KEY_LEN, 32);

    let (client, client_msg) = ClientHalf::start(CODE);
    let (server, server_msg) = ServerHalf::start(CODE);
    assert_eq!(client_msg.len(), MESSAGE_LEN);
    assert_eq!(server_msg.len(), MESSAGE_LEN);

    let key = client.finish(&server_msg).expect("finish the app's half");
    let _ = server
        .finish(&client_msg)
        .expect("finish the backend's half");
    assert_eq!(key.as_bytes().len(), KEY_LEN);
    assert_eq!(confirm_a(&key, &Binding::Spki(OWN)).len(), CONFIRM_LEN);
}

// Criterion (guard): a message is randomised. Near-miss: a message derived
// from the code alone, which a recorded exchange would let anyone test
// every code against offline.
#[test]
fn test_two_starts_on_one_code_send_different_messages() {
    let (_, first) = ClientHalf::start(CODE);
    let (_, second) = ClientHalf::start(CODE);
    assert_eq!(first.len(), MESSAGE_LEN, "a real message, not an empty one");
    assert_ne!(first, second);

    let (_, first) = ServerHalf::start(CODE);
    let (_, second) = ServerHalf::start(CODE);
    assert_eq!(first.len(), MESSAGE_LEN, "a real message, not an empty one");
    assert_ne!(first, second);
}

// Criterion: SPAKE2 values — `spake2` 0.4.0, `Ed25519Group`, asymmetric
// (`start_a` on the app, `start_b` on the backend), identities `blue2th-app`
// and `blue2th-server`, the password the normalized code. Pinned against the
// raw crate on both sides: any other group, role, identity or password
// derives another key.
#[test]
fn test_each_half_matches_raw_spake2_with_the_pinned_identities() {
    use spake2::{Ed25519Group, Identity, Password, Spake2};

    assert_eq!(APP_IDENTITY, "blue2th-app");
    assert_eq!(SERVER_IDENTITY, "blue2th-server");
    let password = Password::new(CODE.as_bytes());
    let app_id = Identity::new(b"blue2th-app");
    let server_id = Identity::new(b"blue2th-server");

    // Our app half against a raw side B.
    let (client, client_msg) = ClientHalf::start("k7m2qx");
    let (raw_b, raw_b_msg) = Spake2::<Ed25519Group>::start_b(&password, &app_id, &server_id);
    let ours = client
        .finish(&raw_b_msg)
        .expect("finish against raw side B");
    let theirs = raw_b.finish(&client_msg).expect("raw side B finishes");
    assert_eq!(ours.as_bytes().as_slice(), theirs.as_slice());

    // Our backend half against a raw side A.
    let (server, server_msg) = ServerHalf::start("k7m2qx");
    let (raw_a, raw_a_msg) = Spake2::<Ed25519Group>::start_a(&password, &app_id, &server_id);
    let ours = server
        .finish(&raw_a_msg)
        .expect("finish against raw side A");
    let theirs = raw_a.finish(&server_msg).expect("raw side A finishes");
    assert_eq!(ours.as_bytes().as_slice(), theirs.as_slice());
}

// Criterion (guard, asymmetric roles): two app halves on the same code never
// derive one key. Near-miss: the symmetric mode, where they would — and a
// relay could reflect the app's own message back at it.
#[test]
fn test_two_app_halves_on_one_code_never_agree() {
    let (client_key, server_key) = exchange(CODE, CODE);
    assert_eq!(
        client_key.as_bytes(),
        server_key.as_bytes(),
        "app and backend halves agree"
    );

    let (first, first_msg) = ClientHalf::start(CODE);
    let (second, second_msg) = ClientHalf::start(CODE);
    let first_key = first.finish(&second_msg);
    let second_key = second.finish(&first_msg);
    let agreed = match (&first_key, &second_key) {
        (Ok(a), Ok(b)) => a.as_bytes() == b.as_bytes(),
        _ => false,
    };
    assert!(!agreed, "two side-A halves must not derive a shared key");
}

// Criterion: a malformed peer message (empty, or one byte short) is refused
// by `finish` rather than turned into a key.
#[test]
fn test_finish_refuses_a_malformed_message() {
    let (_, well_formed) = ServerHalf::start(CODE);
    assert_eq!(well_formed.len(), MESSAGE_LEN);
    let short = &well_formed[..MESSAGE_LEN - 1];

    for message in [&[][..], short] {
        let (client, _) = ClientHalf::start(CODE);
        assert_eq!(
            client.finish(message).err(),
            Some(PakeError),
            "a {}-byte message must be refused",
            message.len()
        );
        let (server, _) = ServerHalf::start(CODE);
        assert_eq!(server.finish(message).err(), Some(PakeError));
    }
}

// Criterion (guard): a confirmation that is empty, truncated or one bit off
// is refused. Near-miss: a comparison over the shorter length, under which
// the empty confirmation would match everything.
#[test]
fn test_a_truncated_or_empty_confirmation_is_refused() {
    let (client_key, server_key) = exchange(CODE, CODE);
    let binding = Binding::Spki(OWN);
    let right = confirm_a(&client_key, &binding);
    assert!(check_confirm_a(&server_key, &binding, &right));

    let mut flipped = right;
    if let Some(last) = flipped.last_mut() {
        *last ^= 0x01;
    }
    for presented in [&[][..], &right[..CONFIRM_LEN - 1], &flipped[..]] {
        assert!(
            !check_confirm_a(&server_key, &binding, presented),
            "{presented:02x?} must be refused"
        );
        assert!(!check_confirm_b(&server_key, &binding, presented));
    }
}

// Criterion: confirmations are compared in constant time — the comparison is
// at least right: equal slices match, a difference anywhere (first byte, last
// byte, length) does not.
#[test]
fn test_constant_time_eq_tells_equal_from_different() {
    let a = [7u8, 1, 2, 3, 4];
    assert!(constant_time_eq(&a, &a));
    assert!(!constant_time_eq(&a, &[8, 1, 2, 3, 4]));
    assert!(!constant_time_eq(&a, &[7, 1, 2, 3, 5]));
    assert!(!constant_time_eq(&a, &[7, 1, 2, 3]));
    assert!(!constant_time_eq(&a[..4], &a));
}

// Criterion: the binding is built from the pin the app stores, 64 lower-case
// hex characters; there is no way to build an empty one. Near-misses: the
// empty string, one character short, one too many, a non-hex character.
#[test]
fn test_binding_from_pin_hex_accepts_only_a_full_pin() {
    let pin = "11".repeat(32);
    assert_eq!(Binding::from_pin_hex(&pin), Some(Binding::Spki(OWN)));

    let short = &pin[..63];
    let long = format!("{pin}1");
    let not_hex = format!("g{}", &pin[1..]);
    // `11…` has no letter, so it cannot tell upper from lower case: a pin
    // with letters can. `Pin::to_hex` writes lower case, so an upper-case
    // pin is not one the app stored.
    let lettered = "ab".repeat(32);
    assert_eq!(
        Binding::from_pin_hex(&lettered),
        Some(Binding::Spki([0xab; 32]))
    );
    let upper = lettered.to_uppercase();
    for refused in ["", short, long.as_str(), not_hex.as_str(), upper.as_str()] {
        assert_eq!(
            Binding::from_pin_hex(refused),
            None,
            "{refused:?} is not a pin"
        );
    }
}

// Criterion: every field of the exchange travels as lower-case hex; a field
// that is not hex for exactly the expected length is refused, the empty one
// included.
#[test]
fn test_hex_round_trips_and_refuses_a_wrong_length_or_digit() {
    assert_eq!(to_hex(&[0x00, 0xab, 0xff]), "00abff");
    assert_eq!(from_hex("00abff", 3), Some(vec![0x00, 0xab, 0xff]));

    assert_eq!(from_hex("00abff", 2), None, "one byte too many");
    assert_eq!(from_hex("00abff", 4), None, "one byte short");
    assert_eq!(from_hex("", MESSAGE_LEN), None, "empty");
    assert_eq!(from_hex("", 0), None, "an empty field is never a value");
    assert_eq!(from_hex("abc", 2), None, "odd length");
    assert_eq!(from_hex("0g", 1), None, "not a hex digit");
}
