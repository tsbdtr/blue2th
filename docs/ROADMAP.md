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
2. **Phone role** — **remote** for the new audio path. The existing Android BT
   JNI/A2DP code is **kept as legacy** (see below), not deleted.
3. **Control transport** — **REST first**; **SSE/WebSocket** for the live scan
   stream (phase 1).
4. **Spotify** — `librespot` (PC = Connect endpoint) + Web API control with
   **OAuth Authorization Code + PKCE** (no client secret on mobile). Amazon Music
   is **out of scope** (no public playback API).

## Legacy: existing Android Bluetooth code

The current on-phone Bluetooth stack (scan, pair, multi-profile connect/disconnect
via JNI + reflection on hidden A2DP/HFP APIs) is **retained**. The phone no longer
needs it for the PC-backend path, but it is the foundation for a possible future
**on-phone LE Audio** feature if/when Android third-party LE Audio support
improves. Keep it building and tested; gate the new path behind the backend.

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
  offsets are persisted, never the selection: at startup no speaker is connected,
  so a restored selection would be dropped immediately by `retain_connected` —
  restoring it belongs with auto-reconnect below.
- ⬜ **Auto-reconnect** — reconnect the remembered speakers on startup; this is what
  would make restoring the *selection* meaningful.
- ⬜ **Runtime backend address + mDNS discovery** — `BLUE2TH_BACKEND_URL` is read by
  `option_env!`, i.e. at **compile time**, so the PC's LAN address is baked into the
  APK. This is the blocker for distributing a binary: today sharing the app means
  sharing the repo so each user rebuilds with their own address. Runtime
  configuration first, mDNS after.
- ⬜ **Authenticated, LAN-only control API** — the router still runs
  `CorsLayer::permissive()` with no authentication: anyone on the network can drive
  the backend, Spotify transport included. Becomes necessary as soon as discovery
  exists.
- ⬜ **Background listening reliability** — Android freezes a backgrounded app, which
  drops the now-playing SSE stream the backend uses as a liveness signal. Handled
  today by presence reporting (`onStart`/`onStop`/`onTaskRemoved`) plus a 30-minute
  grace period; a **foreground service** (with its permanent notification) is the
  only way to stop the freeze outright, to be paid only if the compromise bites.

## Cross-cutting concerns

| Topic | Plan |
|---|---|
| Security | Authenticated + LAN-only control API; PKCE (no OAuth secret on mobile); never expose Spotify tokens |
| Network discovery | Manual IP first → mDNS in phase 6 |
| CI / tests | BlueZ/PipeWire/librespot are not CI-testable → pure logic unit-tested, hardware manual (same philosophy as the Android JNI path) |
| Sync | Imperfect on classic A2DP (no shared clock) but tunable via latency offsets — manage expectations |
| librespot | Unofficial, requires Premium, may break on Spotify updates |

## Sequencing note

Phases 0→4 already deliver the original goal (two speakers, local files) **without
Spotify**. Phase 5 layers Spotify on top. The project could have stopped after
phase 4 if Spotify integration had proved too brittle — it did not: phases 0→5 are
shipped and validated on hardware, and only the optional phase 6 remains.
