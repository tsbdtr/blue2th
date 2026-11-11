// SPDX-License-Identifier: MIT OR Apache-2.0

//! #197: the code queue (one active code at a time, its five minutes counted
//! from its activation), the single PAKE session, the attempt cap shared by
//! `finish` and `POST /pair`, and the per-address limit on `start`. Pure: every
//! instant is passed in.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use blue2th_proto::pake::{check_confirm_b, confirm_a, Binding, ClientHalf, SharedKey};

use super::*;

/// The token a pairing releases.
const TOKEN: &str = "stored-token-value";

/// The backend's own key hash.
const OWN: Binding = Binding::Spki([0x11; 32]);

/// A relay's key hash: well-formed, and not the backend's.
const RELAY: Binding = Binding::Spki([0x22; 32]);

/// The first and second codes the `arm_codes` seam arms.
const FIRST: &str = "K7M2QX";
const SECOND: &str = "ABCDEF";

/// A fixed instant, so nothing depends on the wall clock.
fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A store with `FIRST` then `SECOND` armed at `t0()`, through the seam.
fn two_codes() -> AuthStore {
    let mut store = AuthStore::with_token(TOKEN);
    let mut draws = [FIRST, SECOND].into_iter();
    let codes = store.arm_codes(2, || {
        PairingCode::armed(draws.next().unwrap_or_default(), t0() + PAIRING_TTL)
    });
    assert_eq!(codes, [FIRST, SECOND]);
    store
}

/// A store with `FIRST` alone armed at `t0()`.
fn one_code() -> AuthStore {
    let mut store = AuthStore::with_token(TOKEN);
    let codes = store.arm_codes(1, || PairingCode::armed(FIRST, t0() + PAIRING_TTL));
    assert_eq!(codes, [FIRST]);
    store
}

/// The app's side of one exchange, once `start` answered.
struct Attempt {
    session: String,
    key: SharedKey,
}

impl Attempt {
    /// The app's confirmation over `binding`.
    fn confirm(&self, binding: &Binding) -> Vec<u8> {
        confirm_a(&self.key, binding).to_vec()
    }
}

/// Run the app's half on `code` against `store.start_pake` at `now`. `None`
/// when `start` refused.
fn begin(store: &mut AuthStore, code: &str, now: SystemTime) -> Option<Attempt> {
    let (client, msg) = ClientHalf::start(code);
    let started = store.start_pake(&msg, now).ok()?;
    let key = client.finish(&started.msg).ok()?;
    Some(Attempt {
        session: started.session,
        key,
    })
}

/// `begin`, asserted to open a session.
fn opened(store: &mut AuthStore, code: &str, now: SystemTime) -> Attempt {
    let attempt = begin(store, code, now);
    assert!(attempt.is_some(), "start must open a session on {code}");
    attempt.expect("asserted above")
}

/// A whole exchange on `code`, the app's confirmation computed over
/// `binding`, the store checking against [`OWN`].
fn exchange(
    store: &mut AuthStore,
    code: &str,
    binding: &Binding,
    now: SystemTime,
) -> Result<PakeFinished, PairError> {
    let attempt = opened(store, code, now);
    store.finish_pake(&attempt.session, &attempt.confirm(binding), &OWN, now)
}

/// Whether a whole, honest exchange on `code` releases the token.
fn pairs(store: &mut AuthStore, code: &str, now: SystemTime) -> bool {
    let Some(attempt) = begin(store, code, now) else {
        return false;
    };
    store
        .finish_pake(&attempt.session, &attempt.confirm(&OWN), &OWN, now)
        .is_ok_and(|finished| finished.token == TOKEN)
}

// ---- The attempt cap ----

// Criterion: `MAX_PAIRING_ATTEMPTS` is 3.
#[test]
fn test_max_pairing_attempts_is_three() {
    assert_eq!(MAX_PAIRING_ATTEMPTS, 3);
}

// ---- The exchange ----

// Criterion: the right code, no relay — `finish` answers `{confirm, token}`,
// and the backend's confirmation is one the app accepts over the same key
// hash.
#[test]
fn test_finish_with_the_right_code_releases_the_token_and_a_valid_confirm() {
    let mut store = one_code();
    let attempt = opened(&mut store, FIRST, t0());
    let finished = store.finish_pake(&attempt.session, &attempt.confirm(&OWN), &OWN, t0());

    assert_eq!(finished.as_ref().map(|f| f.token.as_str()), Ok(TOKEN));
    assert!(
        finished
            .as_ref()
            .is_ok_and(|f| check_confirm_b(&attempt.key, &OWN, &f.confirm)),
        "the backend's confirmation must check out on the app's side"
    );
    assert_eq!(store.paired(), 1);
}

// Criterion (guard, relay): a `finish` whose confirmation was computed with
// the right code but over another key hash than the backend's own is
// refused, releases no token and counts one failure. The code check passes
// it; only the binding check refuses it.
#[test]
fn test_finish_over_another_key_hash_is_refused_and_counts_one_failure() {
    let mut store = one_code();

    assert_eq!(
        exchange(&mut store, FIRST, &RELAY, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 1);
    assert_eq!(store.paired(), 0);
    assert!(
        pairs(&mut store, FIRST, t0()),
        "the code stays active after one refusal"
    );
}

// Criterion: a `finish` computed with a wrong code is refused with no token
// and one failure counted; the code stays active.
#[test]
fn test_finish_with_a_wrong_code_is_refused_and_counts_one_failure() {
    let mut store = one_code();

    assert_eq!(
        exchange(&mut store, "K7M2QY", &OWN, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 1);
    assert!(pairs(&mut store, FIRST, t0()), "the code stays active");
}

// Criterion (guard, the label): a confirmation computed over
// `blue2th-plain-http` is refused by a TLS backend, whose binding is its key
// hash.
#[test]
fn test_finish_over_the_plain_http_label_is_refused_by_a_tls_store() {
    let mut store = one_code();
    assert_eq!(
        exchange(&mut store, FIRST, &Binding::PlainHttp, t0()),
        Err(PairError::Rejected)
    );
    assert!(
        pairs(&mut store, FIRST, t0()),
        "and the right binding still pairs, so the refusal was the label's"
    );
}

// Criterion (guard, exactly 3, one count for both routes): two failures —
// one per route — leave the active code pairing.
#[test]
fn test_two_failures_across_both_routes_leave_the_active_code() {
    let mut store = two_codes();
    assert_eq!(
        exchange(&mut store, FIRST, &RELAY, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.redeem("QQQQQQ", t0()), Err(PairError::Rejected));
    assert_eq!(store.failures(), 2);

    assert!(
        pairs(&mut store, FIRST, t0()),
        "two failures leave the codes"
    );
}

// Criterion (guard, exactly 3): the third failure, whichever route it comes
// through, cancels every remaining code — the active one and the one not yet
// active. Near-misses: a cap of 4; a cancellation of the active code only,
// which would activate `SECOND`.
#[test]
fn test_the_third_failure_across_both_routes_cancels_every_code() {
    let mut store = two_codes();
    assert_eq!(
        exchange(&mut store, "K7M2QY", &OWN, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.redeem("QQQQQQ", t0()), Err(PairError::Rejected));
    assert_eq!(
        exchange(&mut store, FIRST, &RELAY, t0()),
        Err(PairError::Rejected)
    );

    assert_eq!(store.failures(), 3);
    assert_eq!(store.armed(), None, "no code is left active");
    assert_eq!(store.active_position(), None);
    assert!(
        begin(&mut store, FIRST, t0()).is_none(),
        "start finds no active code"
    );
    assert_eq!(store.redeem(FIRST, t0()), Err(PairError::Rejected));
    assert_eq!(
        store.redeem(SECOND, t0()),
        Err(PairError::Rejected),
        "the code not yet active is cancelled too"
    );
}

// ---- Failures that are not guesses ----

// Criterion (guard): a `finish` on an unknown session counts no failure —
// the empty session and a prefix of the open one included, the two values
// a prefix or emptiness check would match. Three of them, then the open
// session pairs.
#[test]
fn test_finish_on_unknown_sessions_counts_nothing_and_the_open_one_pairs() {
    let mut store = one_code();
    let attempt = opened(&mut store, FIRST, t0());
    let confirm = attempt.confirm(&OWN);
    assert!(attempt.session.len() >= 2, "a real session id");
    let prefix = &attempt.session[..attempt.session.len() / 2];

    for unknown in ["", prefix, "deadbeefdeadbeefdeadbeefdeadbeef"] {
        assert_eq!(
            store.finish_pake(unknown, &confirm, &OWN, t0()),
            Err(PairError::Rejected),
            "session {unknown:?} is not the open one"
        );
    }
    assert_eq!(store.failures(), 0, "nothing was guessed");
    assert_eq!(
        store
            .finish_pake(&attempt.session, &confirm, &OWN, t0())
            .map(|f| f.token),
        Ok(TOKEN.to_string())
    );
}

// Criterion: a session lives 30 s — a `finish` at 29 s is accepted.
#[test]
fn test_a_finish_29_seconds_after_start_is_accepted() {
    assert_eq!(PAKE_SESSION_TTL, secs(30));
    let mut store = one_code();
    let attempt = opened(&mut store, FIRST, t0());
    assert_eq!(
        store
            .finish_pake(
                &attempt.session,
                &attempt.confirm(&OWN),
                &OWN,
                t0() + secs(29)
            )
            .map(|f| f.token),
        Ok(TOKEN.to_string())
    );
}

// Criterion: a session lives 30 s — a `finish` at 30 s is refused, counts no
// failure, and leaves the code active.
#[test]
fn test_a_finish_30_seconds_after_start_is_refused_without_counting() {
    let mut store = one_code();
    let attempt = opened(&mut store, FIRST, t0());
    assert_eq!(
        store.finish_pake(
            &attempt.session,
            &attempt.confirm(&OWN),
            &OWN,
            t0() + secs(30)
        ),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 0);
    assert!(
        pairs(&mut store, FIRST, t0() + secs(31)),
        "the code is still active"
    );
}

// Criterion: one session at a time — a second `start` replaces the first,
// whose `finish` is then refused without counting; the second pairs.
#[test]
fn test_a_second_start_replaces_the_first_session() {
    let mut store = one_code();
    let first = opened(&mut store, FIRST, t0());
    let second = opened(&mut store, FIRST, t0() + secs(1));

    assert_eq!(
        store.finish_pake(&first.session, &first.confirm(&OWN), &OWN, t0() + secs(2)),
        Err(PairError::Rejected),
        "the replaced session is gone"
    );
    assert_eq!(store.failures(), 0);
    assert_eq!(
        store
            .finish_pake(&second.session, &second.confirm(&OWN), &OWN, t0() + secs(2))
            .map(|f| f.token),
        Ok(TOKEN.to_string())
    );
}

// Criterion: `start` with no active code — none armed, or all used — is
// refused, counts no failure and opens no session.
#[test]
fn test_start_without_an_active_code_is_refused_without_counting() {
    let mut store = AuthStore::with_token(TOKEN);
    let (_, msg) = ClientHalf::start(FIRST);
    assert_eq!(store.start_pake(&msg, t0()), Err(PairError::Rejected));
    assert_eq!(store.failures(), 0);

    let mut store = one_code();
    assert!(pairs(&mut store, FIRST, t0()), "the one code pairs");
    let (_, msg) = ClientHalf::start(FIRST);
    assert_eq!(store.start_pake(&msg, t0()), Err(PairError::Rejected));
    assert_eq!(store.failures(), 0);
}

// Criterion: `start` with no active code counts no failure — also when the
// active code has expired but the queue task has not advanced past it yet, the
// one case where codes are still armed, so the counter could move.
#[test]
fn test_start_on_an_expired_code_not_yet_advanced_counts_no_failure() {
    let mut store = two_codes();
    let (_, msg) = ClientHalf::start(FIRST);
    assert_eq!(
        store.start_pake(&msg, t0() + PAIRING_TTL),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 0);
    assert_eq!(store.active_position(), Some((1, 2)), "nothing advanced");
}

// Criterion: an attempt with no code armed is not a guess at anything, so it
// counts nothing — on either route, before any code was armed and after the
// last one was used. The counter is what the `k/3` of the refusal line and
// the queue end's "no pairing code left" read.
#[test]
fn test_an_attempt_with_no_code_armed_counts_no_failure() {
    let mut store = AuthStore::with_token(TOKEN);
    assert_eq!(store.redeem("QQQQQQ", t0()), Err(PairError::Rejected));
    assert_eq!(store.failures(), 0, "nothing armed yet");

    let mut store = one_code();
    assert!(pairs(&mut store, FIRST, t0()), "the one code pairs");
    assert_eq!(store.redeem("QQQQQQ", t0()), Err(PairError::Rejected));
    assert_eq!(store.failures(), 0, "the last code was used");
}

// Criterion: a successful `finish` consumes its session — the same request
// replayed releases nothing more, and counts nothing.
#[test]
fn test_a_finish_consumes_its_session() {
    let mut store = two_codes();
    let attempt = opened(&mut store, FIRST, t0());
    let confirm = attempt.confirm(&OWN);
    assert!(store
        .finish_pake(&attempt.session, &confirm, &OWN, t0())
        .is_ok());

    assert_eq!(
        store.finish_pake(&attempt.session, &confirm, &OWN, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 0);
    assert_eq!(store.paired(), 1);
}

// Transition: the browser takes the active code during an app's session —
// that session's `finish` is refused, not counted, and the next code
// activates.
#[test]
fn test_a_session_whose_code_the_browser_took_is_refused_without_counting() {
    let mut store = two_codes();
    let attempt = opened(&mut store, FIRST, t0());
    assert_eq!(store.redeem(FIRST, t0()), Ok(TOKEN.to_string()));

    assert_eq!(
        store.finish_pake(&attempt.session, &attempt.confirm(&OWN), &OWN, t0()),
        Err(PairError::Rejected)
    );
    assert_eq!(store.failures(), 0);
    assert_eq!(store.active_position(), Some((2, 2)));
}

// Transition: the active code expires during a session — `finish` refused,
// not counted, even before the pairing task advanced the queue.
#[test]
fn test_a_session_whose_code_expired_is_refused_without_counting() {
    let mut store = one_code();
    let attempt = opened(&mut store, FIRST, t0() + PAIRING_TTL - secs(10));

    assert_eq!(
        store.finish_pake(
            &attempt.session,
            &attempt.confirm(&OWN),
            &OWN,
            t0() + PAIRING_TTL + secs(5),
        ),
        Err(PairError::Rejected),
        "the session is 15 s old, but its code expired"
    );
    assert_eq!(store.failures(), 0);
}

// ---- Codes one at a time ----

// Criterion (guard, only the active code): with two codes armed, the second
// submitted to `POST /pair` before the first is consumed is refused —
// today's any-armed-code rule would accept it. Once the first is consumed,
// the second redeems.
#[test]
fn test_redeem_accepts_only_the_active_code() {
    let mut store = two_codes();
    assert_eq!(store.redeem(SECOND, t0()), Err(PairError::Rejected));
    assert_eq!(store.redeem(FIRST, t0()), Ok(TOKEN.to_string()));
    assert_eq!(store.redeem(SECOND, t0()), Ok(TOKEN.to_string()));
}

// Criterion (guard, only the active code): `finish` too accepts only the
// active code — an exchange run on the second while the first is active is
// refused.
#[test]
fn test_finish_accepts_only_the_active_code() {
    let mut store = two_codes();
    assert_eq!(
        exchange(&mut store, SECOND, &OWN, t0()),
        Err(PairError::Rejected)
    );
    assert!(pairs(&mut store, FIRST, t0()));
    assert!(pairs(&mut store, SECOND, t0()));
}

// Criterion: a code is activated when the previous one is consumed — the
// first at start — and the queue reports `k/n` for the active one.
#[test]
fn test_the_next_code_activates_when_the_active_one_is_consumed() {
    let mut store = two_codes();
    assert_eq!(store.active_position(), Some((1, 2)));
    assert_eq!(store.armed().map(PairingCode::code), Some(FIRST));

    assert!(pairs(&mut store, FIRST, t0()));
    assert_eq!(store.active_position(), Some((2, 2)));
    assert_eq!(store.armed().map(PairingCode::code), Some(SECOND));

    assert_eq!(store.redeem(SECOND, t0()), Ok(TOKEN.to_string()));
    assert_eq!(store.active_position(), None);
    assert_eq!(store.paired(), 2, "one success per route, counted together");
}

// Criterion (guard, TTL from activation): code 2, activated when code 1 is
// consumed at `t = t0 + 4 min`, is accepted at `t + 4 min 59 s` — when
// today's rule, counted from the start, would refuse it.
#[test]
fn test_a_code_activated_late_is_accepted_4_min_59_after_its_activation() {
    let mut store = two_codes();
    let t = t0() + secs(240);
    assert_eq!(store.redeem(FIRST, t), Ok(TOKEN.to_string()));
    assert_eq!(store.redeem(SECOND, t + secs(299)), Ok(TOKEN.to_string()));
}

// Criterion: the same code 2 is refused at `t + 5 min`.
#[test]
fn test_a_code_activated_late_is_refused_5_min_after_its_activation() {
    let mut store = two_codes();
    let t = t0() + secs(240);
    assert_eq!(store.redeem(FIRST, t), Ok(TOKEN.to_string()));
    assert_eq!(
        store.redeem(SECOND, t + PAIRING_TTL),
        Err(PairError::Rejected)
    );
}

// Criterion: a code is activated when the previous one expires — the
// queue advances at the expiry instant, not a second before, and the next
// code's five minutes start there. After the last, the queue ends.
#[test]
fn test_advance_activates_the_next_code_at_expiry_and_then_ends() {
    let mut store = two_codes();
    assert_eq!(
        store.advance(t0() + PAIRING_TTL - secs(1)),
        QueueStep::Unchanged
    );
    assert_eq!(store.active_position(), Some((1, 2)));

    assert_eq!(store.advance(t0() + PAIRING_TTL), QueueStep::Activated);
    assert_eq!(store.active_position(), Some((2, 2)));
    assert_eq!(
        store.armed().map(PairingCode::expires_at),
        Some(t0() + PAIRING_TTL + PAIRING_TTL)
    );

    assert_eq!(
        store.advance(t0() + PAIRING_TTL + PAIRING_TTL),
        QueueStep::Ended
    );
    assert_eq!(store.active_position(), None);
    assert_eq!(store.armed(), None);
}

// Criterion: code 2, activated on code 1's expiry at `t0 + 5 min`, is
// accepted at `t0 + 5 min + 4 min 59 s`, by `finish` as by `POST /pair`.
#[test]
fn test_a_code_activated_on_expiry_pairs_4_min_59_later() {
    let at = t0() + PAIRING_TTL + secs(299);

    let mut store = two_codes();
    assert_eq!(store.advance(t0() + PAIRING_TTL), QueueStep::Activated);
    assert!(pairs(&mut store, SECOND, at));

    let mut store = two_codes();
    assert_eq!(store.advance(t0() + PAIRING_TTL), QueueStep::Activated);
    assert_eq!(store.redeem(SECOND, at), Ok(TOKEN.to_string()));
}

// ---- The rate limit on `start` ----

const A: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)));
const B: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 43)));

// Criterion: from one source address, 5 `start` within 10 s pass and the
// sixth is refused, with the wait until the oldest leaves the window.
#[test]
fn test_rate_limit_passes_five_and_refuses_the_sixth_with_its_wait() {
    assert_eq!((PAIR_START_LIMIT, PAIR_START_WINDOW), (5, secs(10)));
    let base = Instant::now();
    let mut limiter = PairRateLimiter::new();
    for n in 1..=5 {
        assert_eq!(limiter.check(A, base), Ok(()), "request {n}");
    }
    assert_eq!(limiter.check(A, base), Err(secs(10)));
    assert_eq!(limiter.check(A, base + secs(3)), Err(secs(7)));
}

// Criterion: the window slides — five requests a second apart, the sixth is
// refused just short of 10 s after the first and passes at 10 s, when the
// first has left; the one after it is refused again.
#[test]
fn test_rate_limit_window_slides() {
    let base = Instant::now();
    let mut limiter = PairRateLimiter::new();
    for n in 0..5 {
        assert_eq!(limiter.check(A, base + secs(n)), Ok(()));
    }
    assert!(limiter
        .check(A, base + secs(10) - Duration::from_millis(1))
        .is_err());
    assert_eq!(limiter.check(A, base + secs(10)), Ok(()));
    assert!(limiter.check(A, base + secs(10)).is_err());
}

// Criterion (guard, per address): the sixth from address A is refused while
// the first from address B passes.
#[test]
fn test_rate_limit_is_per_source_address() {
    let base = Instant::now();
    let mut limiter = PairRateLimiter::new();
    for _ in 0..5 {
        assert_eq!(limiter.check(A, base), Ok(()));
    }
    assert!(limiter.check(A, base).is_err());
    assert_eq!(limiter.check(B, base), Ok(()));
}

// Criterion (guard, no address is no exemption): requests with no known peer
// address are limited under one shared key — the sixth is refused — and
// leave a known address unaffected.
#[test]
fn test_rate_limit_counts_unknown_addresses_under_one_shared_key() {
    let base = Instant::now();
    let mut limiter = PairRateLimiter::new();
    for _ in 0..5 {
        assert_eq!(limiter.check(None, base), Ok(()));
    }
    assert!(
        limiter.check(None, base).is_err(),
        "a missing address must not be exempt"
    );
    assert_eq!(limiter.check(A, base), Ok(()));
}

// Criterion: `Retry-After` is in whole seconds, rounded up — never telling a
// client to come back before it would pass.
#[test]
fn test_retry_after_rounds_the_wait_up_to_whole_seconds() {
    assert_eq!(retry_after_secs(Duration::from_millis(7_200)), 8);
    assert_eq!(retry_after_secs(Duration::from_millis(100)), 1);
    assert_eq!(retry_after_secs(secs(9)), 9);
    assert_eq!(retry_after_secs(secs(10)), 10);
}
