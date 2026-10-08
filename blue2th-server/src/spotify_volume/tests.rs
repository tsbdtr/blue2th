// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

// Criterion: `on_user_set(p)` stores the level, and `desired()` reports it.
#[test]
fn test_policy_desired_follows_a_user_level() {
    let mut policy = Policy::new();
    assert_eq!(policy.on_user_set(60), 60);
    assert_eq!(policy.desired(), Some(60));
}

// Criterion: `on_user_set(p)` clamps to `0..=100` — the stored level is what
// `PUT /me/player/volume` accepts, and 0 is a real level, not "unset".
#[test]
fn test_policy_clamps_a_user_level_to_100() {
    let mut policy = Policy::new();
    assert_eq!(policy.on_user_set(150), 100);
    assert_eq!(policy.desired(), Some(100));
    assert_eq!(policy.on_user_set(0), 0);
    assert_eq!(policy.desired(), Some(0));
}

// Criterion (nominal steps 2-4): the level the user chose is written back
// once `librespot` has been respawned and the poll sees it back at 100.
#[test]
fn test_policy_restores_the_desired_level_after_a_respawn() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    assert_eq!(
        policy.on_observed(Some(60)),
        None,
        "the level matches: nothing to write"
    );

    policy.mark_respawned();
    assert_eq!(
        policy.on_observed(Some(100)),
        Some(60),
        "a respawn snapped librespot back to 100: restore the user's 60"
    );
    assert_eq!(policy.desired(), Some(60));
}

// Criterion (nominal step 2): a change made in a Spotify client, seen by the
// poll while no respawn is pending, is adopted as desired and not fought.
#[test]
fn test_policy_adopts_a_level_changed_in_a_spotify_client_when_no_respawn_is_pending() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    assert_eq!(
        policy.on_observed(Some(35)),
        None,
        "a client change is the user's choice: nothing to write"
    );
    assert_eq!(policy.desired(), Some(35), "the observed level is adopted");
}

// Criterion: a failed `PUT` keeps the mark, so the restore is retried at the
// next poll; `written()` is what clears it — after which 100 observed again
// is a plain client change and is adopted.
#[test]
fn test_policy_keeps_the_respawn_mark_until_a_write_succeeds() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    policy.mark_respawned();

    assert_eq!(policy.on_observed(Some(100)), Some(60));
    // No `written()`: the PUT failed.
    assert_eq!(
        policy.on_observed(Some(100)),
        Some(60),
        "a failed write must be retried at the next poll"
    );
    assert_eq!(
        policy.desired(),
        Some(60),
        "a pending restore must not adopt the respawn's 100"
    );

    policy.written();
    assert_eq!(
        policy.on_observed(Some(60)),
        None,
        "the restored level needs no further write"
    );
    assert_eq!(
        policy.on_observed(Some(100)),
        None,
        "once written, the mark is gone: a later 100 is a client change"
    );
    assert_eq!(policy.desired(), Some(100));
}

// Criterion (non-nominal): no device / 204 → `None` observed, nothing to
// write, and the mark stays so the restore happens once a device is back.
#[test]
fn test_policy_does_nothing_while_no_level_is_observed() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    policy.mark_respawned();

    assert_eq!(policy.on_observed(None), None);
    assert_eq!(policy.on_observed(None), None);
    assert_eq!(
        policy.desired(),
        Some(60),
        "an absent level must never be read as 0 or adopted"
    );
    assert_eq!(
        policy.on_observed(Some(100)),
        Some(60),
        "the mark must survive the polls that saw no device"
    );
}

// Criterion (non-nominal): a respawn with no desired level known has nothing
// to restore; the first level seen becomes the desired one.
#[test]
fn test_policy_has_nothing_to_restore_before_any_level_is_known() {
    let mut policy = Policy::new();
    assert_eq!(policy.desired(), None);
    policy.mark_respawned();

    assert_eq!(
        policy.on_observed(Some(100)),
        None,
        "nothing was chosen, so nothing is restored"
    );
    assert_eq!(
        policy.desired(),
        Some(100),
        "the first observed level is adopted as desired"
    );
    assert_eq!(
        policy.on_observed(Some(100)),
        None,
        "an empty restore must not leave the mark armed"
    );
}

// Criterion (nominal step 5): with the lock on, desired is 100 and any other
// observed level is corrected; 100 observed needs no write.
#[test]
fn test_policy_lock_reasserts_100_on_any_other_level() {
    let mut policy = Policy::new();
    policy.set_lock(true);

    assert_eq!(policy.desired(), Some(100));
    assert_eq!(policy.on_observed(Some(30)), Some(100));
    assert_eq!(
        policy.on_observed(Some(100)),
        None,
        "already at 100: nothing to write"
    );
    assert_eq!(policy.on_observed(Some(99)), Some(100));
    assert_eq!(policy.desired(), Some(100));
}

// Criterion: the lock wins over a user level, whether the level was set
// before or after the lock — a locked backend never adopts anything else.
#[test]
fn test_policy_lock_wins_over_a_user_level() {
    let mut policy = Policy::new();
    policy.on_user_set(40);
    policy.set_lock(true);
    assert_eq!(policy.desired(), Some(100));
    assert_eq!(
        policy.on_observed(Some(40)),
        Some(100),
        "the level chosen before the lock is overridden"
    );

    policy.on_user_set(40);
    assert_eq!(policy.desired(), Some(100), "a locked policy stays at 100");
    assert_eq!(policy.on_observed(Some(40)), Some(100));
}

// Criterion (non-nominal): lock turned on at 40 → the next poll writes 100.
#[test]
fn test_policy_turning_the_lock_on_writes_100_at_the_next_poll() {
    let mut policy = Policy::new();
    policy.on_user_set(40);
    assert_eq!(policy.on_observed(Some(40)), None);

    policy.set_lock(true);
    assert_eq!(policy.on_observed(Some(40)), Some(100));
}

// Rule: only the edge of the lock acts. `POST /config` re-applies the
// stored lock on every push, and every push carries the whole config —
// so an unchanged `false` must keep the remembered level and
// a pending restore, or the next respawn would have nothing to put back.
#[test]
fn test_policy_reapplying_an_unchanged_lock_keeps_the_desired_level_and_the_mark() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    policy.mark_respawned();

    policy.set_lock(false);
    assert_eq!(
        policy.desired(),
        Some(60),
        "re-applying lock=false must not forget the user's level"
    );
    assert_eq!(
        policy.on_observed(Some(100)),
        Some(60),
        "re-applying lock=false must not drop a pending restore"
    );

    policy.set_lock(true);
    policy.mark_respawned();
    policy.set_lock(true);
    assert_eq!(
        policy.on_observed(Some(40)),
        Some(100),
        "re-applying lock=true keeps pinning 100"
    );
}

// Criterion (non-nominal): lock off → desired becomes the next observed
// level, and nothing is written; the pinned 100 does not linger.
#[test]
fn test_policy_unlocking_adopts_the_next_observed_level_without_writing() {
    let mut policy = Policy::new();
    policy.set_lock(true);
    assert_eq!(policy.on_observed(Some(100)), None);

    policy.set_lock(false);
    assert_eq!(
        policy.on_observed(Some(45)),
        None,
        "unlocking must not fight the level the user picks next"
    );
    assert_eq!(policy.desired(), Some(45));
}

// Criterion: the lock also survives a respawn — with the lock on, the
// restore target is 100, never a level remembered from before the lock.
#[test]
fn test_policy_lock_restores_100_after_a_respawn() {
    let mut policy = Policy::new();
    policy.on_user_set(60);
    policy.set_lock(true);
    policy.mark_respawned();

    assert_eq!(policy.on_observed(Some(100)), None);
    assert_eq!(policy.on_observed(Some(60)), Some(100));
}
