# blue2th Roadmap — Multi-speaker audio via mobile remote + PC backend

## Vision

Stream music to **two classic Bluetooth speakers at once**, fully controlled from
the phone. Because Android forbids a third-party app from routing A2DP audio to
two sinks (the BT stack and A2DP source role are OS-owned, and dual-A2DP is an
OEM feature à la Samsung), the audio engine moves to a **Linux PC backend** where
BlueZ + PipeWire give full user-space control. The phone becomes a **remote**.

Spotify is integrated **without forwarding raw audio** (impossible — DRM/licensing):
the PC becomes a **Spotify Connect** endpoint via `librespot`, and the phone
drives playback through the Spotify Web API. Audio streams from Spotify directly
to the PC, then fans out to the two speakers.

## Architecture

```
 blue2th (Dioxus / Android)              Spotify Web API
   remote UI, Spotify OAuth   ──────────▶  play / transfer / volume
        │  HTTP + SSE/WS                          │
        ▼                                         ▼
 blue2th-server (Rust / Axum, Linux PC)    Spotify servers
   - bluer   → BlueZ (connect A2DP)              │ direct stream
   - librespot → Spotify Connect endpoint  ◀─────┘
   - PipeWire combine-sink → fan-out
        ├──────────────┐
        ▼              ▼
   🔊 Speaker 1    🔊 Speaker 2
```

Speakers pair with the **PC**, not the phone. The PC owns the audio source
(local files, or librespot). The phone never forwards audio.

## Recorded decisions

1. **Repository** — single **cargo workspace** in this repo:
   - `blue2th` — existing Dioxus app (mobile remote).
   - `blue2th-server` — Axum backend on the Linux PC.
   - `blue2th-proto` — DTOs shared between mobile and server (typed contract).
2. **Phone role** — **remote** for the new audio path. The on-phone Android BT
   JNI/A2DP code was kept as legacy through the transition, then deleted once the
   backend path was proven (see below).
3. **Control transport** — **REST first**; **SSE/WebSocket** for the live scan
   stream (phase 1).
4. **Spotify** — `librespot` (PC = Connect endpoint) + Web API control with
   **OAuth Authorization Code + PKCE** (no client secret on mobile). Amazon Music
   is **out of scope** (no public playback API).

## Legacy: the on-phone Android Bluetooth code, removed

The on-phone Bluetooth stack (scan, pair, multi-profile connect/disconnect via JNI
+ reflection on hidden A2DP/HFP APIs) was **retained through the transition**,
hidden behind a flag, in case it became the foundation for an on-phone **LE Audio**
feature.

It was **deleted** once the backend path was proven on hardware: 2579 lines that
compiled on every build, were partly live, and that nobody was going to revive —
Android still does not expose third-party LE Audio, and the phone is a remote now.
Should LE Audio ever land, it would be a new stack against a new API, not this one.

The JNI that remains in the app is unrelated to audio: `jni_util.rs` (the multicast
lock for mDNS, phase 6.6) and `lifecycle.rs` (the presence hooks the watchdog
reads).

## Phases (each ships something testable end-to-end)

Testing discipline mirrors the existing project: pure logic is unit-tested;
hardware paths (BlueZ, PipeWire, librespot) are validated manually and gated out
of CI.

### Phase 0 — Foundations
- **Backend**: Axum skeleton (`/health`), env config, `tracing`. `blue2th-proto`
  crate with request/response DTOs.
- **Mobile**: configurable backend base URL (the PC's IP).
- **Done when**: `curl /health` returns ok; app reaches the backend; route unit test.

### Phase 1 — Bluetooth discovery (`bluer`)
- **Backend**: adapter enumeration + scan via `bluer`; `GET /adapters`,
  `GET /devices` (paired), scan stream over SSE/WS.
- **Mobile**: reuse the existing device-list component, fed by the backend.
- **Done when**: a scan started from the phone shows the speakers seen by the PC.

### Phase 2 — Connect / disconnect a speaker
- **Backend**: `POST /devices/{addr}/connect|disconnect` (`bluer`: pair → trust →
  connect, triggering the A2DP profile).
- **Mobile**: reuse the existing connect/disconnect buttons, wired to the backend.
- **Done when**: the phone connects a BT speaker through the PC; test tone audible.

### Phase 3 — Play audio to ONE speaker (PipeWire)
- **Backend**: on connect, PipeWire creates a sink; play a local audio file to it
  (`symphonia`/`rodio` or PipeWire routing). `POST /play|/pause|/volume`.
- **Mobile**: basic transport controls.
- **Done when**: a local file plays on one speaker, controlled from the phone.

### Phase 4 — Fan-out to TWO speakers ⭐ (original goal) — ✅ DONE
- **Backend**: PipeWire **combined sink** spanning both BT sinks; route playback
  to it; expose a **per-speaker latency offset**.
- **Mobile**: pick the two target speakers + a sync-offset slider.
- **Done when**: the same track plays on two classic BT speakers, tunable to an
  acceptable sync. **This realizes the original goal (without Spotify).**
- **Status**: shipped and **validated on hardware** — explicit two-speaker
  selection (cap 2), per-speaker offset (0–750 ms) applied as `module-loopback`
  branch latency over a shared null sink, single-speaker path preserved.

### Phase 5 — Spotify source ⭐ (full vision) — ✅ DONE
- **Backend**: embed/spawn **`librespot`** → PC becomes a Spotify Connect device;
  route its output into the combined sink (Spotify → two speakers).
- **Mobile**: **Spotify OAuth** (Authorization Code + PKCE); Web API to transfer
  playback to the blue2th-PC device + play/pause/skip/volume + now-playing metadata.
- **Done when**: from the phone — log in, "play on blue2th-PC", Spotify audio on two
  speakers with full transport control.
- **Status**: shipped in two slices, **validated on hardware**.
  - **5.1** — `librespot` spawned as a Connect device (`blue2th-PC`), output routed
    per the current selection. `--system-cache` is required: in plain zeroconf mode
    librespot never logs into the account, so it stays absent from
    `GET /me/player/devices` and the Web API cannot target it. The **first** run
    still needs one manual pick of `blue2th-PC` in a Spotify client to seed the
    credentials.
  - **5.2** — OAuth PKCE (no client secret anywhere), tokens held server-side with
    silent refresh, the refresh token persisted so a restart does not send the user
    back through the browser. Transport resolves the `blue2th-PC` device and
    transfers playback to it rather than driving whichever device is active. The
    redirect comes back through an Android deep link (`blue2th://spotify-callback`,
    `launchMode="singleTop"` + `onNewIntent` → `setIntent`), and now-playing is
    pushed over SSE.
- **Requires**: a Spotify **Premium** account, a registered Developer app whose
  client id is given to the backend as `BLUE2TH_SPOTIFY_CLIENT_ID` (no default —
  the server answers 503 naming the variable), and the account listed in that app's
  Development-mode allowlist.

### Phase 6 — Robustness (optional) — 🚧 IN PROGRESS
Auto-reconnect, persistence (favorite speakers, offsets), token refresh, **mDNS**
backend discovery, authenticated LAN-only control API.

- ✅ **Token refresh** — shipped with 5.2: silent refresh before expiry, refresh
  token persisted, dropped only when Spotify itself rejects the grant (never on a
  network failure, which would cost a browser round-trip for nothing).
- ✅ **6.1 — Offset persistence** — each speaker's sync offset is remembered by MAC
  in `$XDG_STATE_HOME/blue2th/offsets.json` and restored when that speaker is
  selected again, including after a restart. **Validated on hardware.** Only the
  offsets were persisted, never the selection — 6.3 below closed that half.
- ✅ **6.2 — Runtime backend configuration** — the backend address was resolved by
  `option_env!`, i.e. at compile time, so the APK only worked for whoever built it.
  `BLUE2TH_BACKEND_URL` is gone: the app holds a list of **named** backends, one
  active at a time, persisted in `SharedPreferences`, switchable from the status
  encart or the settings page (both go through `backend::activate_backend`, so they
  cannot drift apart). Each entry's name is pushed to its backend, which adopts it
  as its Spotify Connect device name — and as the name the Web API device lookup
  matches on, which is what keeps transport working after a rename. Unconfigured
  means unconfigured: no fallback address, calls fail fast instead of timing out
  against localhost. **Validated on hardware.**
- ✅ **6.3 — Selection restored when a speaker comes back** — `SpeakerTargets` now
  separates the **intent** (the addresses the user asked to play on) from the live
  selection: losing the radio prunes the latter and leaves the former alone, so
  `sync_connected` re-selects a returning speaker instead of making the user press
  `+` every time. Only an explicit deselect clears the intent, and the intent is
  persisted next to the offsets, so it survives a restart too. `restore()` reports
  whether the selection actually moved and the routing is rebuilt only then —
  `sync_connected` runs on every `/devices` poll, so re-routing unconditionally
  would tear the PipeWire graph down every couple of seconds. Reintegrating
  mid-playback can move the target sink and respawn `librespot`, so it is gated by
  a per-backend setting (default **on**), carried by `/config`. **Validated on
  hardware.**
- ✅ **6.4 — Authenticated, LAN-only control API** — `CorsLayer::permissive()` and
  the open router are gone: every route but `/health` and `POST /pair` requires a
  bearer token, and the server binds to its LAN address rather than `0.0.0.0`.
  Pairing is armed on first run (or with `--pair`) and offered two ways, chosen per
  backend: a six-character code typed into the app, or a QR whose `blue2th://` deep
  link carries url, name and code in one scan. The code is one-shot, short-lived
  and attempt-capped, and every refusal reads the same so the route cannot be used
  to enumerate. The token is persisted `0600` server-side and per backend on the
  phone; a 401 surfaces as "not paired", distinct from a backend that simply cannot
  be reached. **Validated on hardware.**
- ✅ **6.5 — Auto-reconnect** — 6.3 restored the selection the moment a known
  speaker reappeared, but something else had to bring it back first. The backend
  now dials the remembered speakers itself: the persisted playback **intent** is
  the list, and a paired-but-disconnected address is re-dialled at startup and
  then on a per-address backoff (15 s → 30 s → 60 s → 120 s, then three attempts
  at a 5-minute cap before it is given up on). It only *connects* — selection and
  routing stay with 6.3's `sync_connected`, which picks the speaker up on the next
  `/devices` poll, so the graph is never rebuilt twice. It also never **pairs**:
  the pass dials through a paired-only call, so an address that lost its bond is
  refused rather than bonded unattended. A `/disconnect` from the app **dismisses**
  the address rather than forgetting it — the intent and its tuned offset are kept,
  6.3 still restores it if it returns on its own, but the backend stops dialling
  until the user selects or connects it again: it must not fight the user. The
  policy is pure and clock-free (`Instant` comes in as a parameter), which is what
  makes the retry ladder testable at all; the BlueZ dial itself is not. Like 6.3,
  it is a per-backend setting carried by `/config`, default **on**. **Validated on
  hardware.**
- ✅ **6.6 — mDNS discovery** — a backend was identified by its URL, so a new DHCP
  lease broke every call and re-pairing created a *second* entry for the same
  machine. Identity moves to a stable id the server mints once
  (`identity.json`, separate from the token) and publishes over
  `_blue2th._tcp.local` with its name. **Search the network** in the settings page
  browses for it and either repairs a known backend's address in place — token and
  local name kept, no duplicate — or offers an unknown one for the normal pairing
  flow: discovery announces, it never authenticates, and the 6.4 code is still due.
  Both behaviours are per-app settings, on by default, and a pre-6.6 entry adopts
  the id it is matched to by URL, so it survives its *next* move too. The JNI
  surface is three synchronous calls for the multicast lock — `mdns-sd` browses in
  pure Rust, no `NsdManager`, no second `.dex`. Finding nothing stays a neutral
  state: manual entry and the QR remain the way out. **Validated on hardware.**
- ⬜ **6.7 — Background listening reliability** — Android freezes a backgrounded app,
  which drops the now-playing SSE stream the backend uses as a liveness signal.
  Handled today by presence reporting (`onStart`/`onStop`/`onTaskRemoved`) plus a
  30-minute grace period; a **foreground service** (with its permanent notification)
  is the only way to stop the freeze outright, to be paid only if the compromise
  bites.

## Cross-cutting concerns

| Topic | Plan |
|---|---|
| Security | ✅ Authenticated + LAN-only control API (6.4, bearer token + one-shot pairing code/QR); PKCE (no OAuth secret on mobile); never expose Spotify tokens |
| Network discovery | ✅ Runtime-configured named backends (6.2) → mDNS browse + stable backend id (6.6) |
| CI / tests | BlueZ/PipeWire/librespot are not CI-testable → pure logic unit-tested, hardware manual (same philosophy as the Android JNI path) |
| Sync | Imperfect on classic A2DP (no shared clock) but tunable via latency offsets — manage expectations |
| librespot | Unofficial, requires Premium, may break on Spotify updates |

## Sequencing note

Phases 0→4 already deliver the original goal (two speakers, local files) **without
Spotify**. Phase 5 layers Spotify on top. The project could have stopped after
phase 4 if Spotify integration had proved too brittle — it did not: phases 0→5 are
shipped and validated on hardware, and only the optional phase 6 remains.
