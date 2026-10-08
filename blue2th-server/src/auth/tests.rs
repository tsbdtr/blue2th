// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

/// A private, per-test store path under the system temp dir. Never the real
/// `~/.local/state/blue2th/`: a test run must not be able to unpair the
/// operator's phone.
fn temp_store(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blue2th-test-auth-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("auth.json")
}

/// A fixed instant to anchor the expiry tests, so they never depend on the
/// wall clock.
fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// Arm exactly one code at `t0()` and return it — the single-code start
/// every pre-#160 test was written against. An empty string when the store
/// hands back no code, which no redemption accepts.
fn single_code(store: &mut AuthStore) -> String {
    let codes = store.arm_pairing(1, t0());
    assert_eq!(codes.len(), 1, "arming one code must hand back one code");
    codes.into_iter().next().unwrap_or_default()
}

/// Arm `count` codes at `t0()`, asserting the store handed back that many,
/// so a guard test below never passes on a store that armed fewer.
fn armed_codes(store: &mut AuthStore, count: u32) -> Vec<String> {
    let codes = store.arm_pairing(count, t0());
    assert_eq!(
        codes.len(),
        count as usize,
        "--pair {count} must arm {count} codes, got {codes:?}"
    );
    codes
}

// Criterion: `generate_token()` yields a URL-safe token of at least 32 bytes
// of entropy. Base64url/hex carry at most 6/4 bits per character, so a token
// shorter than 43 characters cannot hold 32 bytes.
#[test]
fn test_generate_token_is_long_and_url_safe() {
    let token = generate_token();
    assert!(
        token.len() >= 43,
        "a {TOKEN_ENTROPY_BYTES}-byte token cannot be encoded in {} characters: {token}",
        token.len()
    );
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "the token must be URL-safe (it travels in a header and a settings blob), got {token}"
    );
}

// Criterion: the token is different on every call — a predictable token is
// no token at all.
#[test]
fn test_generate_token_differs_on_every_call() {
    let tokens: Vec<String> = (0..8).map(|_| generate_token()).collect();
    let mut unique = tokens.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), tokens.len(), "tokens repeated: {tokens:?}");
}

// Criterion: `PairingCode::mint()` is short and URL-safe — it is typed by
// hand and rides inside a `blue2th://pair?…` deep link.
#[test]
fn test_pairing_code_mint_is_short_and_url_safe() {
    let code = PairingCode::mint(t0());
    assert_eq!(code.code().chars().count(), PAIRING_CODE_LEN);
    assert!(
        code.code().chars().all(|c| c.is_ascii_alphanumeric()),
        "a typed code must stay alphanumeric, got {}",
        code.code()
    );
}

// Criterion: a minted code is different every time, or the "one-shot" rule
// would protect nothing.
#[test]
fn test_pairing_code_mint_differs_on_every_call() {
    let codes: Vec<String> = (0..8)
        .map(|_| PairingCode::mint(t0()).code().to_string())
        .collect();
    let mut unique = codes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), codes.len(), "codes repeated: {codes:?}");
}

// Criterion: `PairingCode::mint()` expires after `PAIRING_TTL`.
#[test]
fn test_pairing_code_mint_expires_after_the_ttl() {
    let code = PairingCode::mint(t0());
    assert_eq!(code.expires_at(), t0() + PAIRING_TTL);
}

// Criterion: `verify_code` accepts an unexpired, exactly matching code.
#[test]
fn test_verify_code_accepts_the_armed_code() {
    let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
    assert_eq!(verify_code(Some(&stored), "K7M2QX", t0()), Ok(()));
}

// Criterion: only an *exactly* matching code is accepted — not a prefix, not
// a code with something appended, not a different one. Case and surrounding
// whitespace are the sole tolerance, and only because a phone keyboard adds
// them (see `test_verify_code_accepts_a_hand_typed_code`).
#[test]
fn test_verify_code_rejects_anything_but_an_exact_match() {
    let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
    for submitted in ["K7M2Q", "K7M2QX7", "AAAAAA", "", "K7M2 QX"] {
        assert_eq!(
            verify_code(Some(&stored), submitted, t0()),
            Err(PairError::Rejected),
            "{submitted:?} is not the armed code"
        );
    }
}

// Criterion (usability): the code is typed on a phone, which capitalises the
// first character only and happily leaves a trailing space. Neither is a
// wrong code, and treating them as one spends a silent attempt.
#[test]
fn test_verify_code_accepts_a_hand_typed_code() {
    let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
    for submitted in ["k7m2qx", "K7m2qx", " K7M2QX ", "K7M2QX\n"] {
        assert_eq!(
            verify_code(Some(&stored), submitted, t0()),
            Ok(()),
            "{submitted:?} is the armed code as a phone keyboard renders it"
        );
    }
}

// Criterion (non-nominal, security): an expired code and an unknown one are
// **indistinguishable** — same error value and same message, so the reply
// cannot be used to enumerate. "No code armed" reads the same way.
#[test]
fn test_verify_code_cannot_tell_expired_from_unknown_or_unarmed() {
    let expired = PairingCode::armed("K7M2QX", t0());
    let armed = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
    let after_ttl = t0() + PAIRING_TTL + Duration::from_secs(1);

    let expired_err = verify_code(Some(&expired), "K7M2QX", after_ttl)
        .expect_err("an expired code must be refused");
    let unknown_err =
        verify_code(Some(&armed), "AAAAAA", t0()).expect_err("an unknown code must be refused");
    let unarmed_err =
        verify_code(None, "K7M2QX", t0()).expect_err("with no code armed nothing is accepted");

    assert_eq!(expired_err, unknown_err);
    assert_eq!(expired_err, unarmed_err);
    assert_eq!(expired_err.to_string(), unknown_err.to_string());
    assert_eq!(expired_err.to_string(), unarmed_err.to_string());
}

// Criterion: the code expires after `PAIRING_TTL` — accepted right up to the
// deadline, refused past it.
#[test]
fn test_verify_code_rejects_a_code_past_its_expiry() {
    let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
    assert_eq!(
        verify_code(
            Some(&stored),
            "K7M2QX",
            t0() + PAIRING_TTL - Duration::from_secs(1)
        ),
        Ok(())
    );
    assert_eq!(
        verify_code(
            Some(&stored),
            "K7M2QX",
            t0() + PAIRING_TTL + Duration::from_secs(1)
        ),
        Err(PairError::Rejected)
    );
}

// Criterion: `POST /pair` with a valid armed code returns the token — at the
// store level, redeeming returns exactly the stored token.
#[test]
fn test_redeem_returns_the_stored_token() {
    let mut store = AuthStore::with_token("stored-token-value");
    let code = single_code(&mut store);
    assert_eq!(
        store.redeem(&code, t0()),
        Ok("stored-token-value".to_string())
    );
}

// Criterion: a code is one-shot — the second redemption of the same code
// fails, even straight away and even though it has not expired.
#[test]
fn test_redeem_consumes_the_code_so_a_second_use_fails() {
    let mut store = AuthStore::with_token("stored-token-value");
    let code = single_code(&mut store);
    assert!(store.redeem(&code, t0()).is_ok());
    assert_eq!(store.redeem(&code, t0()), Err(PairError::Rejected));
}

// Criterion: after `MAX_PAIRING_ATTEMPTS` failures the armed code is
// invalidated — brute force on the one open door is capped.
#[test]
fn test_redeem_invalidates_the_code_after_the_attempt_cap() {
    let mut store = AuthStore::with_token("stored-token-value");
    let code = single_code(&mut store);
    for _ in 0..MAX_PAIRING_ATTEMPTS {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    assert_eq!(
        store.redeem(&code, t0()),
        Err(PairError::Rejected),
        "the armed code must be dead once the cap is reached"
    );
}

// Criterion (non-nominal): pairing while no code is armed is refused, with
// the same error as a wrong code.
#[test]
fn test_redeem_without_an_armed_code_is_refused() {
    let mut store = AuthStore::with_token("stored-token-value");
    assert!(store.armed().is_none());
    assert_eq!(store.redeem("K7M2QX", t0()), Err(PairError::Rejected));
}

// Criterion: arming a code replaces the previous one — the operator restarts
// the server (or runs `--pair`) to mint a fresh code, and the old one dies.
#[test]
fn test_arming_a_new_code_retires_the_previous_one() {
    let mut store = AuthStore::with_token("stored-token-value");
    let first = single_code(&mut store);
    let second = single_code(&mut store);
    assert_ne!(first, second);
    assert_eq!(store.redeem(&first, t0()), Err(PairError::Rejected));
    assert!(store.redeem(&second, t0()).is_ok());
}

// ---- #160: `--pair <n>` arms several codes at once ----

// Criterion: `--pair <n>` with n in 1..=10 arms n **distinct** codes — at
// both ends of the range.
#[test]
fn test_arm_pairing_arms_the_requested_number_of_distinct_codes() {
    for count in [1u32, 2, 10] {
        let mut store = AuthStore::with_token("stored-token-value");
        let codes = armed_codes(&mut store, count);
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            codes.len(),
            "every armed code must be distinct, got {codes:?}"
        );
        assert!(
            codes.iter().all(|c| c.len() == PAIRING_CODE_LEN),
            "every armed code is a typed six-character code, got {codes:?}"
        );
    }
}

// Criterion (guard, "n **distinct** codes"): a minted code equal to one
// already armed is drawn again, so two codes never collapse into one code
// redeemable twice. Random minting never collides in a test, so the
// minter hands back the same code twice on purpose.
#[test]
fn test_arm_codes_draws_again_a_code_already_armed() {
    let mut store = AuthStore::with_token("stored-token-value");
    let mut draws = ["K7M2QX", "K7M2QX", "ABCDEF"].into_iter().cycle();
    let codes = store.arm_codes(2, || {
        PairingCode::armed(draws.next().unwrap_or_default(), t0() + PAIRING_TTL)
    });
    assert_eq!(codes, ["K7M2QX", "ABCDEF"]);
}

// Criterion: any armed, unexpired, unconsumed code redeems for the API
// token; redeeming one leaves the others valid. Redeemed in reverse order
// so a store that only ever checks the first (or the last) code fails.
#[test]
fn test_redeem_accepts_every_armed_code_and_one_leaves_the_others_valid() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 3);
    for code in codes.iter().rev() {
        assert_eq!(
            store.redeem(code, t0()),
            Ok("stored-token-value".to_string()),
            "code {code} of {codes:?} must redeem once"
        );
    }
}

// Criterion: each code works once — every one of them, not only the first.
#[test]
fn test_redeem_consumes_each_of_several_codes_once() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 3);
    for code in &codes {
        assert!(store.redeem(code, t0()).is_ok(), "first use of {code}");
        assert_eq!(
            store.redeem(code, t0()),
            Err(PairError::Rejected),
            "second use of {code} must be refused"
        );
    }
}

// Criterion: the `MAX_PAIRING_ATTEMPTS`-th failure (5) invalidates every
// armed code, not just one of them. Near-miss: a cap that retires one code
// per failure past it — the refusals below would then retire the rest one
// by one, so the store is checked empty first, and the codes are tried
// last one first.
#[test]
fn test_redeem_cap_invalidates_every_armed_code() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 3);
    for _ in 0..MAX_PAIRING_ATTEMPTS {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    assert_eq!(store.armed(), None, "the cap leaves no code armed");
    for code in codes.iter().rev() {
        assert_eq!(
            store.redeem(code, t0()),
            Err(PairError::Rejected),
            "{code} must be dead once the shared cap is reached"
        );
    }
}

// Criterion (guard): failed attempts are counted **once for all codes**.
// Near-miss: 3 failures, then a successful redemption of the first code,
// then 2 more failures. A per-code count that charges each failure to the
// first live code leaves 3 on the consumed one and 2 on the second, so no
// code reaches 5 and the third stays valid — only a shared counter kills
// the second and the third. Also pins that a success does not reset it.
#[test]
fn test_redeem_counts_failures_once_across_all_codes() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 3);
    for _ in 0..3 {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    let first = codes.first().cloned().unwrap_or_default();
    assert!(store.redeem(&first, t0()).is_ok(), "{first} is still valid");
    for _ in 0..2 {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    for code in codes.iter().skip(1) {
        assert_eq!(
            store.redeem(code, t0()),
            Err(PairError::Rejected),
            "five failures in total must invalidate {code}"
        );
    }
}

// Criterion (guard, the other side of the cap): one failure short of the
// cap leaves every code valid — the cap is 5 for the whole set. Near-miss:
// a cap shared out among the armed codes, or a failure charged once per
// armed code (3 codes x 2 failures past 5), kills them earlier.
#[test]
fn test_redeem_below_the_shared_cap_leaves_every_code_valid() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 3);
    for _ in 0..MAX_PAIRING_ATTEMPTS - 1 {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    for code in &codes {
        assert!(
            store.redeem(code, t0()).is_ok(),
            "{code} must survive {} failures",
            MAX_PAIRING_ATTEMPTS - 1
        );
    }
}

// Criterion: every code expires `PAIRING_TTL` after it was armed.
#[test]
fn test_each_armed_code_expires_after_the_ttl() {
    let mut store = AuthStore::with_token("stored-token-value");
    let codes = armed_codes(&mut store, 2);
    let first = codes.first().cloned().unwrap_or_default();
    let second = codes.get(1).cloned().unwrap_or_default();
    assert!(
        store
            .redeem(&first, t0() + PAIRING_TTL - Duration::from_secs(1))
            .is_ok(),
        "a code is valid until its TTL runs out"
    );
    assert_eq!(
        store.redeem(&second, t0() + PAIRING_TTL),
        Err(PairError::Rejected),
        "the second code expires with the TTL too"
    );
}

// Criterion: arming again replaces every previously armed code, and the
// fresh set starts with no failure counted against it.
#[test]
fn test_arming_again_retires_every_previous_code_and_resets_the_cap() {
    let mut store = AuthStore::with_token("stored-token-value");
    let old = armed_codes(&mut store, 2);
    for _ in 0..MAX_PAIRING_ATTEMPTS - 1 {
        assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    }
    let fresh = armed_codes(&mut store, 2);
    for code in &old {
        if !fresh.contains(code) {
            assert_eq!(store.redeem(code, t0()), Err(PairError::Rejected));
        }
    }
    // The two refused old codes and this one make 3 failures against the
    // fresh set — under the cap only if re-arming reset it (4 + 3 = 7
    // otherwise).
    assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
    for code in &fresh {
        assert!(store.redeem(code, t0()).is_ok(), "fresh code {code}");
    }
}

// Criterion: the auth token is persisted and reloaded on restart, so a
// paired phone keeps working across a server restart.
#[test]
fn test_auth_store_round_trips_the_token_through_its_store() {
    let path = temp_store("round-trip");
    let minted = AuthStore::with_store(Some(path.clone()))
        .token()
        .to_string();
    let reloaded = AuthStore::with_store(Some(path.clone()));
    assert_eq!(reloaded.token(), minted);
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion: the token store is written `0600` — it is the credential to the
// whole API, and every user on the box can otherwise read it.
#[cfg(unix)]
#[test]
fn test_auth_store_persists_the_token_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let path = temp_store("permissions");
    let _store = AuthStore::with_store(Some(path.clone()));
    let mode = std::fs::metadata(&path)
        .expect("the token store must have been written")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "got {mode:o}");
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion (non-nominal): a malformed store mints a new token rather than
// disabling authentication — and persists it, so the next start is stable.
#[test]
fn test_auth_store_with_a_malformed_store_mints_a_new_token() {
    let path = temp_store("malformed");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the store directory");
    }
    std::fs::write(&path, "{ not json at all").expect("write a malformed store");

    let store = AuthStore::with_store(Some(path.clone()));
    assert!(
        !store.token().trim().is_empty(),
        "a malformed store must never leave the API unauthenticated"
    );
    let reloaded = AuthStore::with_store(Some(path.clone()));
    assert_eq!(
        reloaded.token(),
        store.token(),
        "the newly minted token must have been persisted"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion: a missing store mints and persists a token on the first run —
// authentication is mandatory from the very first start, no migration.
#[test]
fn test_auth_store_with_a_missing_store_mints_and_persists() {
    let path = temp_store("missing");
    let store = AuthStore::with_store(Some(path.clone()));
    assert!(!store.token().trim().is_empty());
    assert!(
        path.exists(),
        "the token must have been written to {path:?}"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion (non-nominal): a store that was reloaded reports no minting, so
// a plain restart leaves the one open door shut.
#[test]
fn test_auth_store_reloading_a_token_reports_no_minting() {
    let path = temp_store("reloaded-not-minted");
    let first = AuthStore::with_store(Some(path.clone()));
    assert!(
        first.minted_a_new_token(),
        "the very first run has nobody paired and must offer a code"
    );
    assert!(
        !AuthStore::with_store(Some(path.clone())).minted_a_new_token(),
        "a restart that reloaded its token must not re-open pairing"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion (non-nominal, spec): a missing or malformed store mints a token
// and therefore *invalidates every paired client* — the caller has to know,
// or the operator's phone would stop working with no code armed to fix it.
#[test]
fn test_auth_store_reports_minting_when_the_store_is_unusable() {
    for (name, body) in [
        ("unusable-malformed", "{ not json at all"),
        ("unusable-empty-token", r#"{"token":""}"#),
    ] {
        let path = temp_store(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the store directory");
        }
        std::fs::write(&path, body).expect("write the unusable store");
        assert!(
            AuthStore::with_store(Some(path.clone())).minted_a_new_token(),
            "{body} leaves every client unpaired, so pairing must re-open"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }
}

// Criterion (test isolation): `new()` is disk-free — two instances hold
// different tokens, which they could not if either had read a shared file.
#[test]
fn test_auth_store_new_is_disk_free() {
    assert_ne!(AuthStore::new().token(), AuthStore::new().token());
}

// Criterion: `rotate` mints a new token and persists it, so an operator can
// revoke every paired client.
#[test]
fn test_rotate_replaces_and_persists_the_token() {
    let path = temp_store("rotate");
    let mut store = AuthStore::with_store(Some(path.clone()));
    let before = store.token().to_string();
    let after = store.rotate().to_string();
    assert_ne!(before, after);
    assert_eq!(
        AuthStore::with_store(Some(path.clone())).token(),
        after,
        "the rotated token must be the one a restart reloads"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}

// Criterion: every route requires `Authorization: Bearer <token>` — the
// header parser reads the credential, case-insensitively on the scheme.
#[test]
fn test_bearer_token_extracts_the_credential() {
    assert_eq!(bearer_token(Some("Bearer abc123")), Some("abc123"));
    assert_eq!(bearer_token(Some("bearer abc123")), Some("abc123"));
    assert_eq!(bearer_token(Some("Bearer   abc123")), Some("abc123"));
}

// Criterion: a missing or malformed `Authorization` header carries no
// credential at all (and must therefore be refused).
#[test]
fn test_bearer_token_rejects_a_missing_or_malformed_header() {
    for header in [
        None,
        Some(""),
        Some("Bearer"),
        Some("Bearer "),
        Some("abc123"),
        Some("Basic abc123"),
    ] {
        assert_eq!(bearer_token(header), None, "{header:?} carries no bearer");
    }
}

// Criterion: the guard accepts exactly the stored token and nothing else.
#[test]
fn test_is_authorised_accepts_only_the_stored_token() {
    assert!(is_authorised("s3cret", Some("Bearer s3cret")));
    assert!(!is_authorised("s3cret", Some("Bearer s3cre")));
    assert!(!is_authorised("s3cret", Some("Bearer s3cret ")));
    assert!(!is_authorised("s3cret", Some("Bearer S3CRET")));
    assert!(!is_authorised("s3cret", None));
}

// Criterion: the store applies the same rule as the pure helper — the guard
// has one implementation, not two.
#[test]
fn test_auth_store_authorises_its_own_token_only() {
    let store = AuthStore::with_token("s3cret");
    assert!(store.authorises(Some("Bearer s3cret")));
    assert!(!store.authorises(Some("Bearer other")));
    assert!(!store.authorises(None));
}
