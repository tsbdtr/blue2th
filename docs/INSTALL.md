# Installing the backend

The backend is `blue2th-server`, a single binary that runs on a Linux PC, as the
user logged into the desktop session. It drives the PC's Bluetooth through BlueZ
and its audio through PipeWire, and it is what the phone talks to. This page
gets it running and reachable; [`PAIRING.md`](PAIRING.md) then connects the
phone to it.

## What the PC needs

- **PipeWire with its PulseAudio compatibility layer.** The backend drives
  PipeWire through `pactl`, so `pipewire-pulse` must be running in your session
  and `pactl` must be installed. Fedora: `pipewire pipewire-pulseaudio
  pulseaudio-utils`. Debian and Ubuntu: `pipewire pipewire-pulse
  pulseaudio-utils`. Check with `pactl info`: the server name line must say
  *PulseAudio (on PipeWire …)*.
- **BlueZ**, with `bluetoothd` running, and a Bluetooth adapter. Check with
  `bluetoothctl show`.
- **A desktop session.** PipeWire is a per-user service, so the backend runs as
  that user — not as root, not as a system service. A `systemd --user` unit is
  fine (example below).
- For Spotify, [`librespot`](https://github.com/librespot-org/librespot) on the
  `PATH`, built with its PulseAudio backend, and a Spotify **Premium** account:
  ```bash
  cargo install librespot --features pulseaudio-backend
  ```

The speakers do not need to be paired with the PC in advance: put a speaker in
pairing mode and tap it in the app; the backend pairs and connects it.

## Getting the binary

Each [release](https://github.com/tsbdtr/blue2th/releases) ships
`blue2th-server-<version>-x86_64-linux-gnu.tar.gz`: the binary and the two licence
texts. Built on Ubuntu 24.04, it runs on any distribution with glibc 2.39 or
newer and the shared libraries above (`libasound`, `libdbus`).

```bash
tar xzf blue2th-server-<version>-x86_64-linux-gnu.tar.gz
install -m 755 blue2th-server-<version>-x86_64-linux-gnu/blue2th-server ~/.local/bin/
```

[`RELEASING.md`](RELEASING.md) says how to verify what you downloaded.

To build it instead, install a stable Rust toolchain and the development
packages the build links against (`pkg-config`, `libdbus-1-dev`,
`libasound2-dev` on Debian; `dbus-devel`, `alsa-lib-devel` on Fedora), then:

```bash
cargo build --release -p blue2th-server
# → target/release/blue2th-server
```

## First run

```bash
blue2th-server --pair
```

The backend binds the PC's LAN address on port 4000, announces itself on the
network (`_blue2th._tcp` over mDNS), and prints a **pairing banner**: a
six-character code and the same thing as a QR code, valid for five minutes and
usable once. Leave it running and go to [`PAIRING.md`](PAIRING.md).

On later starts, a backend that has already been paired arms no code and says
so. `--pair` arms a new one whenever you want to pair another phone.

### Where it keeps its state

Under `$XDG_STATE_HOME/blue2th/` (`~/.local/state/blue2th/` by default):
`auth.json` (the token the phone holds), `offsets.json` (each speaker's latency
offset, by address), `name.json` (the name the app gave this backend),
`identity.json` (the stable id the phone recognises the backend by, whatever its
address). Spotify's refresh token lives in `spotify-token.json` next to them, and
`librespot`'s credential cache under `~/.cache/blue2th/librespot/`.

Deleting `auth.json` forgets every paired phone: the next start arms a pairing
code again.

### Environment

| Variable | Effect |
|---|---|
| `BLUE2TH_BIND` | Bind address, `host:port`. Overrides the automatic LAN choice — useful when the PC has several interfaces (the backend takes the first routable one) or to listen on `0.0.0.0:4000`. |
| `BLUE2TH_SPOTIFY_CLIENT_ID` | The Spotify application's client id (below). Spotify features stay off without it, and the backend says which variable is missing. |
| `BLUE2TH_SPOTIFY_REDIRECT_URI` | The OAuth redirect URI. Defaults to `blue2th://spotify-callback`, which the app registers; leave it alone unless you rebuilt the app with another scheme. |

## Network

The phone must reach the PC on **TCP 4000**, and network search needs **mDNS**
(UDP 5353, multicast). Both devices on the same Wi-Fi network, with no client
isolation between them. On Fedora with firewalld:

```bash
sudo firewall-cmd --add-port=4000/tcp --add-service=mdns --permanent
sudo firewall-cmd --reload
```

From the PC itself, `curl http://<lan address>:4000/health` must answer a JSON
status. If it does and the phone still cannot reach it, the path between them
is the problem, not the backend — see the troubleshooting table in
[`PAIRING.md`](PAIRING.md).

## Spotify

The PC becomes a Spotify Connect device; the phone controls it through the
Spotify Web API. Three things to set up once.

**1. A Spotify application.** On the
[developer dashboard](https://developer.spotify.com/dashboard), create an app
with `blue2th://spotify-callback` as its redirect URI and *Web API* enabled. Give
its client id to the backend:

```bash
export BLUE2TH_SPOTIFY_CLIENT_ID=<client id>
```

There is no client secret anywhere: the app signs in with PKCE.

**2. The allowlist.** A new Spotify application is in *Development mode*, where
only the accounts listed under *User Management* may use it. Add the Spotify
account you will sign in with. **Skipping this is the one failure with no
visible error**: sign-in works, playback does not.

**3. Seed `librespot` once.** On the very first run, open any Spotify client,
pick the PC in the Connect device list and play something. That signs `librespot`
in and caches the credentials; from then on it signs in by itself when the
backend starts it. Without this step the device exists but stays invisible to
the Web API, and the app cannot transfer playback to it.

## Running it as a service

A `systemd --user` unit keeps the backend up across logins. Example, in
`~/.config/systemd/user/blue2th-server.service`:

```ini
[Unit]
Description=blue2th backend
After=pipewire-pulse.service

[Service]
ExecStart=%h/.local/bin/blue2th-server
Environment=BLUE2TH_SPOTIFY_CLIENT_ID=<client id>
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now blue2th-server
journalctl --user -u blue2th-server -f
```

Pair before enabling the unit, or run `blue2th-server --pair` from a terminal
once while the unit is stopped: the banner goes to the journal otherwise, where
`journalctl` shows it just as well.

## Updating

Replace the binary and restart. The state directory is kept, so the phone stays
paired and the offsets survive. The app and the backend check each other's
protocol version on every contact: an *incompatible* status on the phone means
one side is behind — the message says which.
