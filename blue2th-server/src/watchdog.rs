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

//! Idle watchdog for the now-playing SSE feed (phase 5.2).
//!
//! Playback deliberately keeps going while the app sits in the background, so the
//! backend needs some other way to notice that nobody is there any more: a
//! swipe-away, a crash, an OOM kill or a dropped network would otherwise leave the
//! PC streaming to nobody.
//!
//! The app already holds the `/spotify/now-playing` SSE stream open for as long as
//! it runs, so that connection *is* a heartbeat — no extra route, no extra traffic,
//! no timer on the phone. This module counts the readers and, once the last one has
//! been gone long enough, the router pauses playback.
//!
//! Losing the feed is ambiguous on its own: Android freezes a backgrounded app,
//! which drops the connection even though the user is deliberately listening on.
//! So the app reports what it is doing (`POST /client/presence`) and the grace
//! period follows: short-ish in the foreground (only a crash can cut the feed
//! there), long in the background (the freeze is expected), and a `Gone` report
//! pauses at once without waiting for any of it.

use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use blue2th_proto::ClientPresence;

/// Grace period while the app says it is on screen: only a crash or a kill can
/// cut the feed there, so this needs no slack for a frozen process.
pub const FOREGROUND_GRACE: Duration = Duration::from_secs(10 * 60);

/// Grace period while the app says it is backgrounded. Android freezes the
/// process — and with it the SSE connection — within seconds, so this is not a
/// liveness measure at all: it is the backstop for an app that was killed while
/// backgrounded and will never report `Gone`. Hence the deliberately long value:
/// it must never cut a listening session short.
pub const BACKGROUND_GRACE: Duration = Duration::from_secs(30 * 60);

/// How often the watchdog re-checks. Well under either grace period, so the pause
/// lands close to the deadline without polling tightly.
pub const WATCHDOG_TICK: Duration = Duration::from_secs(10);

/// How long a silent feed is tolerated for a given presence. Pure.
pub fn grace_for(presence: ClientPresence) -> Duration {
    match presence {
        ClientPresence::Foreground => FOREGROUND_GRACE,
        // `Gone` is handled by pausing immediately; should one still be pending
        // here, treat it like the background backstop rather than never firing.
        ClientPresence::Background | ClientPresence::Gone => BACKGROUND_GRACE,
    }
}

/// Whether an idle feed should trigger a pause: no reader left, and the last one
/// gone for at least `grace`. Pure — the caller supplies the elapsed time.
pub fn should_pause_on_idle(readers: usize, empty_for: Option<Duration>, grace: Duration) -> bool {
    readers == 0 && empty_for.is_some_and(|elapsed| elapsed >= grace)
}

/// Reader count for the now-playing SSE feed, when it last fell to zero, and the
/// app's last reported presence.
#[derive(Debug, Default)]
pub struct SseWatch {
    readers: AtomicUsize,
    /// The app's last report. `None` (never reported) is treated as foreground:
    /// an older client that does not post its presence keeps the tighter grace.
    presence: std::sync::Mutex<Option<ClientPresence>>,
    /// When the count last reached zero, as a monotonic instant. `None` while at
    /// least one reader is connected.
    empty_since: std::sync::Mutex<Option<Instant>>,
    /// Set once the watchdog has paused for the current idle period, so it fires
    /// once per departure instead of every tick.
    paused: AtomicBool,
}

impl SseWatch {
    /// Register a reader. The returned guard must live as long as the stream: it
    /// is what detects the client leaving, however the stream ends.
    pub fn subscribe(self: &std::sync::Arc<Self>) -> SseGuard {
        self.readers.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut empty_since) = self.empty_since.lock() {
            *empty_since = None;
        }
        // A returning reader re-arms the watchdog for the next departure.
        self.paused.store(false, Ordering::SeqCst);
        SseGuard {
            watch: std::sync::Arc::clone(self),
        }
    }

    /// Whether playback should be paused now, claiming the right to do it so the
    /// following ticks stay quiet until a reader comes back.
    pub fn claim_idle_pause(&self, grace: Duration) -> bool {
        if self.paused.load(Ordering::SeqCst) {
            return false;
        }
        let empty_for = self
            .empty_since
            .lock()
            .ok()
            .and_then(|since| since.map(|instant| instant.elapsed()));
        if !should_pause_on_idle(self.readers.load(Ordering::SeqCst), empty_for, grace) {
            return false;
        }
        self.paused.store(true, Ordering::SeqCst);
        true
    }

    /// Drop a reader, starting the idle clock when it was the last one.
    fn release(&self) {
        // `fetch_sub` returns the previous value: 1 means we just removed the last.
        if self.readers.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut empty_since) = self.empty_since.lock() {
                *empty_since = Some(Instant::now());
            }
        }
    }

    /// Record what the app says it is doing.
    pub fn set_presence(&self, presence: ClientPresence) {
        if let Ok(mut slot) = self.presence.lock() {
            *slot = Some(presence);
        }
        // A report means the app is alive right now, so the next departure gets a
        // fresh claim — otherwise a pause claimed earlier would suppress it.
        self.paused.store(false, Ordering::SeqCst);
    }

    /// The app's last reported presence, defaulting to foreground.
    pub fn presence(&self) -> ClientPresence {
        self.presence
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .unwrap_or(ClientPresence::Foreground)
    }

    /// Current reader count (tests and diagnostics).
    pub fn readers(&self) -> usize {
        self.readers.load(Ordering::SeqCst)
    }
}

/// Keeps a reader counted for as long as it is held. Dropping it — the stream
/// ending, the client vanishing, the task being cancelled — releases the reader.
pub struct SseGuard {
    watch: std::sync::Arc<SseWatch>,
}

impl Drop for SseGuard {
    fn drop(&mut self) {
        self.watch.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Criterion: a feed with no reader for longer than the grace period is idle.
    #[test]
    fn test_should_pause_on_idle_after_grace_with_no_reader() {
        assert!(should_pause_on_idle(
            0,
            Some(FOREGROUND_GRACE + Duration::from_secs(1)),
            FOREGROUND_GRACE
        ));
    }

    // Criterion: a reader still connected never triggers a pause, however long the
    // feed has been up — this must not become a second foreground detector.
    #[test]
    fn test_should_pause_on_idle_never_with_a_reader() {
        assert!(!should_pause_on_idle(
            1,
            Some(BACKGROUND_GRACE * 10),
            FOREGROUND_GRACE
        ));
    }

    // Criterion: inside the grace period (a restarting app, a brief network drop)
    // playback is left alone; so is a feed that never had a reader.
    #[test]
    fn test_should_pause_on_idle_waits_out_the_grace_period() {
        assert!(!should_pause_on_idle(
            0,
            Some(FOREGROUND_GRACE - Duration::from_secs(1)),
            FOREGROUND_GRACE
        ));
        assert!(!should_pause_on_idle(0, None, FOREGROUND_GRACE));
    }

    // Criterion: the guard counts a reader while alive and releases it on drop,
    // which is what turns a vanished client into an idle feed.
    #[test]
    fn test_guard_counts_readers_and_releases_on_drop() {
        let watch = std::sync::Arc::new(SseWatch::default());
        let first = watch.subscribe();
        let second = watch.subscribe();
        assert_eq!(watch.readers(), 2);

        drop(second);
        assert_eq!(watch.readers(), 1);
        // Still one reader: not idle yet, whatever the grace period.
        assert!(!watch.claim_idle_pause(Duration::from_secs(0)));

        drop(first);
        assert_eq!(watch.readers(), 0);
        assert!(watch.claim_idle_pause(Duration::from_secs(0)));
    }

    // Criterion: a backgrounded app gets a far longer grace than one on screen —
    // Android freezes it within seconds, so the dropped feed says nothing about
    // the user having left.
    #[test]
    fn test_grace_follows_the_reported_presence() {
        assert_eq!(grace_for(ClientPresence::Foreground), FOREGROUND_GRACE);
        assert_eq!(grace_for(ClientPresence::Background), BACKGROUND_GRACE);
        assert!(BACKGROUND_GRACE > FOREGROUND_GRACE);
    }

    // Criterion: presence defaults to foreground until the app reports, so a
    // client that never posts keeps the tighter grace.
    #[test]
    fn test_presence_defaults_to_foreground_and_is_recorded() {
        let watch = SseWatch::default();
        assert_eq!(watch.presence(), ClientPresence::Foreground);
        watch.set_presence(ClientPresence::Background);
        assert_eq!(watch.presence(), ClientPresence::Background);
    }

    // Criterion: the pause is claimed once per departure, so the watchdog does not
    // re-pause on every tick while the app stays away.
    #[test]
    fn test_idle_pause_is_claimed_once_per_departure() {
        let watch = std::sync::Arc::new(SseWatch::default());
        drop(watch.subscribe());
        assert!(watch.claim_idle_pause(Duration::from_secs(0)));
        assert!(!watch.claim_idle_pause(Duration::from_secs(0)));

        // A reader coming back re-arms it for the next departure.
        drop(watch.subscribe());
        assert!(watch.claim_idle_pause(Duration::from_secs(0)));
    }
}
