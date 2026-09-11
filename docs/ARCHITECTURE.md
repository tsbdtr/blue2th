# blue2th architecture

How the system is built and why. This is the design record: what each part
owns, the decisions that shaped it and the reasons behind them. What is done or
still to do lives in the [issue tracker](https://github.com/tsbdtr/blue2th/issues)
and its milestones; how a release is made is in [`RELEASING.md`](RELEASING.md).

## The problem, and the shape of the answer

The goal is to play the same music on **two classic Bluetooth speakers at
once**, controlled from a phone. Android does not allow it: a third-party app
cannot route A2DP audio to two sinks, because the Bluetooth stack and the A2DP
source role belong to the OS, and dual-A2DP is an OEM feature (Samsung's, for
instance), not a platform one.

So the audio engine lives on a **Linux PC**, where BlueZ and PipeWire give full
user-space control of both the radio and the audio graph, and the phone is a
**remote**. Spotify follows the same logic: the phone cannot forward Spotify's
audio (DRM), so the PC becomes a **Spotify Connect** endpoint through
`librespot`, the phone drives playback through the Spotify Web API, and the
audio goes from Spotify's servers to the PC and from there to both speakers.
The phone never carries audio.

```
 blue2th-frontend (Dioxus / Android)        Spotify Web API
   remote UI, Spotify OAuth   ─────────────▶  play / transfer / volume
        │  HTTP + SSE                              │
        ▼                                          ▼
 blue2th-server (Rust / Axum, Linux PC)      Spotify servers
   - bluer     → BlueZ (pair, connect A2DP)        │ direct stream
   - librespot → Spotify Connect endpoint  ◀───────┘
   - PipeWire combined sink → fan-out
        ├──────────────┐
        ▼              ▼
   🔊 Speaker 1    🔊 Speaker 2
```

The speakers pair with the **PC**, not the phone. The PC owns the audio source,
`librespot` or a local test tone; the phone owns nothing but the user's
intent.

## Three layers, one workspace

| Crate | Runs on | Owns |
|---|---|---|
| `blue2th-frontend` | the phone (Dioxus 0.7, Android) | The remote UI, the list of known backends and their tokens, backend discovery, the Spotify sign-in redirect. No Bluetooth of its own. |
| `blue2th-server` | the PC (Axum / Tokio) | Everything with a side effect: BlueZ through `bluer`, the PipeWire graph, the `librespot` child process, the Spotify tokens, the persisted state. |
| `blue2th-proto` | both | The serde types both sides speak, and the protocol version. Target-agnostic by rule: it is compiled into an Android app and a Linux binary. |

A single cargo workspace holds the three, with one version for the product:
the app, the backend and the contract are built from one tag and released
together, so a server-only fix still moves the app's version. The number
designates the release, not the crate that changed.

Decisions taken at the start and still standing:

- **REST for control, SSE for what streams**: the Bluetooth scan and the
  now-playing feed. No WebSocket; nothing needed one.
- **OAuth Authorization Code with PKCE** for Spotify. No client secret exists
  anywhere; the phone signs in, the PC holds the tokens.
- **Amazon Music is out of scope**: it has no public playback API.
- **`librespot` stays a subprocess, never a crate.** It is GPL-3.0; the process
  boundary is what lets blue2th be `MIT OR Apache-2.0`. The rule and its
  consequences are in `CLAUDE.md`.

### The phone owns no Bluetooth

The first blue2th was an on-phone app with its own Bluetooth stack: scan, pair
and multi-profile connect through JNI and reflection on hidden A2DP and HFP
APIs. It was kept through the transition to the backend, behind a flag, in case
it became the base of an on-phone LE Audio feature, then deleted once the
backend path was proven on hardware: 2579 lines that compiled on every build
and that nobody was going to revive. Android still exposes no third-party LE
Audio; should it ever, that would be a new stack against a new API, not this
one. What remains of JNI on the phone is two small seams: a multicast lock for
mDNS, and the lifecycle hooks that report presence.

## The control API

The backend listens on the PC's LAN address, port 4000 (`BLUE2TH_BIND` overrides
it), and serves:

| Area | Routes |
|---|---|
| Liveness and pairing | `GET /health`, `POST /pair` |
| Bluetooth | `GET /adapters`, `GET /devices`, `GET /scan` (SSE), `POST /devices/{addr}/connect` and `/disconnect` |
| Playback targets | `POST /devices/{addr}/select` and `/deselect`, `POST /devices/{addr}/offset`, `GET /targets` |
| Local transport | `POST /play`, `/pause`, `/stop`, `/volume`; `GET /playback` |
| Spotify | `/spotify/start`, `/stop`, `/status`; `/spotify/auth/url`, `/auth/callback`, `/auth/status`; `/spotify/play`, `/pause`, `/next`, `/previous`, `/volume`; `GET /spotify/now-playing` (SSE) |
| Client | `POST /client/presence`, `GET` and `POST /config` |

### Authentication and pairing

Every route requires `Authorization: Bearer <token>`, with two exceptions that
exist for a reason each. `GET /health` is open so that a phone holding a wrong
or missing token reads the backend as *not paired* rather than *offline* — the
two states are fixed differently, and an open probe is what tells them apart.
`POST /pair` is open because it is how a phone gets a token in the first place.

Pairing is therefore the one door, and everything rests on the code behind it
being short-lived, one-shot and rate-limited: a six-character code with
unlimited attempts is not a secret. The code is armed on a first run, whenever
the token store is missing or unreadable (a fresh token has just invalidated
every phone, so someone must be able to pair again), and otherwise only on
`--pair`. It lives five minutes, works once, and five failed attempts cancel
it. Every refusal reads the same, so the route cannot be used to enumerate. The
code reaches the phone typed by hand, or as a QR carrying a `blue2th://pair`
deep link with the address, the name and the code in one scan.

The token is persisted `0600` on the PC and per backend on the phone. A `401`
surfaces in the app as *not paired*, distinct from a backend that cannot be
reached at all.

### Identity and discovery

A backend is identified by a stable id it mints once (`identity.json`), not by
its address: a new DHCP lease used to break every call and re-pairing created a
second entry for the same machine. The id and the backend's name are published
over mDNS as `_blue2th._tcp.local`. The phone browses for it, in pure Rust
(`mdns-sd`), holding a Wi-Fi multicast lock through JNI because the Wi-Fi driver
otherwise filters multicast frames to save power. A known backend found at a new
address is repaired in place, token and local name kept; an unknown one is
offered for the normal pairing flow. Discovery announces, it never
authenticates.

### Protocol version

`blue2th-proto` carries a protocol version, compiled into both sides and
compared on every contact, since the phone and the PC are updated by hand at
different times and a gap between them is the normal state. The app names the
side to update; the comparison never trusts what a payload says about itself.

## The audio path

### Speakers, selection, and the combined sink

The backend scans, pairs, trusts and connects speakers through `bluer`; a
connected A2DP speaker appears in PipeWire as a sink. Playing to two of them is
a **combined sink**: a shared null sink that the source plays into, and one
`module-loopback` branch per selected speaker from that null sink to the
speaker's sink. Every non-empty selection goes through it, one speaker
included; a separate single-speaker path once existed and was dropped because
two paths meant two sets of defects.

Classic A2DP gives two speakers no shared clock, so they drift apart by a fixed
amount that depends on the speaker. Each branch carries a **latency offset**
the user tunes from the phone, 0 to 750 ms, applied as that loopback's
latency on top of a base buffer every branch gets. Offsets are remembered by
speaker address and restored when that speaker is selected again, across
restarts.

The selection is capped at two speakers. It separates the **intent** — the
addresses the user asked to play on — from the live selection: losing a
speaker's radio prunes the latter and leaves the former alone, so a returning
speaker is re-selected without the user pressing anything, and only an explicit
deselect clears the intent. Routing is rebuilt only when the selection actually
moved, because the reconciliation runs on every `/devices` poll and tearing the
PipeWire graph down every couple of seconds would cut the audio.

### Auto-reconnect

The backend also dials remembered speakers itself: the persisted intent is the
list, and a paired-but-disconnected address is re-dialled at startup, then on a
per-address backoff (15 s, 30 s, 60 s, 120 s, then three attempts at a
five-minute cap before it is given up on). The policy is pure and clock-free —
the time comes in as a parameter — which is what makes the retry ladder
testable; the BlueZ dial itself is not. It only *connects*: selection and
routing stay with the reconciliation above, so the graph is never rebuilt
twice. It never *pairs*: an address that lost its bond is refused rather than
bonded unattended. A disconnect from the app **dismisses** the address rather
than forgetting it — intent and offset are kept, and the backend stops dialling
until the user asks again. It must not fight the user.

### Spotify

`librespot` runs as a child process named after the backend, with its output
sent to the combined sink, in Spotify Connect mode. Two details are not
obvious:

- **`--system-cache` is required.** In plain zeroconf mode `librespot` only
  advertises itself and is never logged into the account, so it does not appear
  in the Web API's device list and playback cannot be transferred to it. The
  first run needs one manual pick of the device in a Spotify client to seed the
  credentials; the cache logs it in by itself from then on.
- **Autoplay is off explicitly.** Spotify refuses to hand `librespot` the
  autoplay context it asks for at the end of a queue, so the feature only logs
  errors; off, the behaviour is the same and the log says what happened.

The phone signs in with PKCE and hands the code to the backend through the
`blue2th://spotify-callback` deep link; the backend exchanges it, holds the
tokens, refreshes them silently before expiry, and persists the refresh token
so a restart does not send the user back through the browser. The refresh
token is dropped only when Spotify itself rejects it, never on a network
failure. Transport resolves the backend's device by name and transfers
playback to it rather than driving whichever device is active, which is also
what keeps transport working after the backend is renamed. Now-playing is
pushed to the phone over SSE.

The backend owns the Connect level too (#58). `librespot` applies that level
before PipeWire sees a sample, so it is a separate control from the local
`/volume`: the poll reads it from `/me/player` into now-playing's
`volume_percent`, and `POST /spotify/volume` writes it through the Web API,
targeting the backend's own device. Every `librespot` start is
`--initial-volume 100`, so a respawn — a re-routing, a rename, a fresh
`/spotify/start` — snaps the level back to full scale; the start marks the
respawn, and the policy in `spotify_volume.rs` writes the last chosen level
back at the next poll, keeping the mark until the write succeeds. Without a
mark, a level seen changing is a choice made in some client and is adopted, not
fought. The `spotify_volume_lock` setting pins the level to 100 instead:
`POST /spotify/volume` is refused with a 409 that names the lock, and the poll
re-asserts 100 whenever it sees anything else. The app's own bar for that level
is #120's.

Running the backend under the name the app gave it matters twice: it is the
Spotify Connect device name, and the name the Web API lookup matches on.

The child's lifetime is bound to the server's twice (#122): the child carries a
parent-death signal (`PR_SET_PDEATHSIG`, SIGTERM) so it dies with the server
however the server goes, and the server itself handles SIGTERM/SIGINT by
stopping the Spotify source cleanly and leaving the PipeWire graph in place.

### Presence and the watchdog

Playback deliberately keeps going while the app sits in the background, so the
backend needs some other way to notice that nobody is there: a swipe-away, a
crash or a dropped network would otherwise leave the PC streaming to nobody.
The app already holds the now-playing SSE stream open for as long as it runs,
so that connection is the heartbeat, with no extra traffic.

Losing it is ambiguous on its own, because Android freezes a backgrounded app,
which drops the connection while the user is deliberately listening. So the app
reports what it is doing through `POST /client/presence`, from the activity's
lifecycle hooks (`onStart`, `onStop`, and `onTaskRemoved` in a small service,
the only reliable signal for a swipe out of recents), and the grace period
follows: short in the foreground, where only a crash can cut the feed; long —
thirty minutes — in the background, where the freeze is expected; and a *gone*
report pauses at once. Any report other than *gone* also restarts the idle
clock, because it is positive evidence the app is alive: otherwise a *foreground*
report after a long background idle would apply the shorter grace to time
already spent under the longer one and pause at the next tick. A foreground
service with its permanent notification would stop the freeze outright; it is
the price to pay only if the compromise bites.

## The app

The app holds a list of **named backends**, one active at a time, persisted in
`SharedPreferences`, each with its address, its token, its pairing method and
the backend's stable id. Every switch goes through one function, so the status
card and the settings page cannot drift apart. Unconfigured means unconfigured:
with no backend, calls fail fast instead of timing out against localhost.

Each entry's name is pushed to its backend, which adopts it. Three per-backend
settings travel over `/config`: whether a returning speaker is reintegrated
mid-playback (it can move the target sink and respawn `librespot`), whether the
backend dials remembered speakers, and the backend's name.

Two things the app is not: a Bluetooth controller, and a test subject. Dioxus
components are not test-runnable here, so what is rendered is specified in
writing and checked on the phone.

## Testing philosophy

Pure logic is unit-tested; the hardware boundary — BlueZ, PipeWire, `librespot`,
a real speaker, the Android lifecycle — is validated by hand and kept out of CI.
The code is shaped for that: policies take the clock as a parameter, selection
models take the live state as a slice, the audio module hides PipeWire behind
one interface. A test whose name is the claim beats a comment that states a
fact, and an empty value is guarded explicitly wherever a prefix or a substring
is compared; both rules, and the defects that taught them, are in `CLAUDE.md`.

## Known limits

- **Synchronisation is tunable, not perfect.** Classic A2DP has no shared clock;
  the per-speaker offset compensates. Automatic calibration is a tracked idea,
  not a feature.
- **`librespot` is unofficial**, requires a Premium account, and can break when
  Spotify changes its protocol. The Spotify application must list the account
  on its Development-mode allowlist, or playback fails with no visible error.
- **The PipeWire graph is driven through `pactl`**, a prototype choice; the
  native API is tracked in the v0.2.0 milestone.
- **Background listening on Android** rests on presence reports and a grace
  period rather than a foreground service.
