# Pairing the phone

The phone finds the backend on the network, then pairs with it once. Pairing
hands the phone a token the backend recognises; from then on the phone controls
the backend without asking again. This page assumes the backend is installed and
running with a pairing banner on screen — see [`INSTALL.md`](INSTALL.md).

## Install the app

Each [release](https://github.com/tsbdtr/blue2th/releases) ships
`blue2th-<version>.apk` for Android 7.0 or later, arm64. Install it by hand
(sideloading must be allowed for the app you open it with). Updates install over
the previous version and keep the paired backends.

## Find the backend

Open the settings (the gear at the top right), section **Backends**.

- **Search the network.** The app listens for backends for a few seconds and
  lists what it hears: *New* for one it does not know, *Already known* or
  *Up to date* for one it does. Tap a new one to add it. This needs both devices
  on the same Wi-Fi network; see the table below when nothing shows up.
- **Or add the address by hand**: `http://<the PC's LAN address>:4000`, the
  address the backend printed when it started. **Test** answers *Backend
  answered* when the address is right.

A backend the app knows is recognised by a stable id, not by its address, so
when the PC gets a new address from the router the app repairs the entry
itself (*Repair addresses automatically*, on by default) or asks first.

## Pair

Choose **How to pair** on the backend's entry:

- **Scan the QR** — point any camera app at the QR in the backend's banner. The
  link opens blue2th, adds or updates the backend, pairs it and makes it active.
  Nothing to type.
- **Type a code** — the six characters from the banner, then **Pair**. The code
  ignores the difference between `O` and `0` or `I` and `1` by never using them.

The code is valid for **five minutes** and works **once**; five wrong attempts
cancel it. Run `blue2th-server --pair` on the PC for a fresh one.

The status card on the home screen turns green, *Backend connected*, and the
speakers can be loaded. To pair a second phone, arm a new code the same way.

## What the status colours mean

| Colour | Label | Meaning | What to do |
|---|---|---|---|
| green | Backend connected | Reachable and paired. | Nothing. |
| amber | Not paired — pair this backend first | The backend answers, but the phone holds no token or the backend no longer accepts it (for instance after `auth.json` was deleted on the PC). | Arm a code with `--pair` and pair again. |
| red | Backend unreachable | No answer at all from the address. | The backend is down, the address is wrong, or the network path is blocked — see below. |
| orange | This backend is too old / too new | Both sides answer, but their protocol versions do not match. | Update the side the message names. |

*Not paired* and *unreachable* look alike and are fixed differently, which is
why the app tells them apart: the first is an authentication problem on a
backend that works, the second a network problem where authentication never
comes into play.

## When it does not work

| Symptom | Likely cause | Check |
|---|---|---|
| The search finds nothing | The phone is not on the PC's network: mobile data, a guest Wi-Fi, or a VPN carrying the app's traffic away. Or the backend is not running. | Wi-Fi settings on the phone: same network, an address in the same range as the PC. A VPN with split tunnelling must list blue2th as bypassed — **reinstalling the app drops it from that list**. On the PC, `curl http://<lan address>:4000/health`. |
| Search finds nothing, but the address typed by hand tests fine | Multicast is filtered: the router isolates Wi-Fi clients, or the PC's firewall drops mDNS. | `sudo firewall-cmd --list-services` includes `mdns`; otherwise add it (see [`INSTALL.md`](INSTALL.md)). Typing the address is a perfectly good fallback. |
| *Backend unreachable* with the right address | TCP 4000 is blocked on the PC, or the PC listens on another interface than the one the phone reaches. | `sudo firewall-cmd --list-ports`; the address the backend printed at start; `BLUE2TH_BIND=0.0.0.0:4000` to listen everywhere. |
| *Not paired* right after a backend restart | `auth.json` was deleted or unreadable on the PC, so the backend minted a new token and forgot every phone. | Run with `--pair` and pair again. |
| The pairing code is refused | Expired (five minutes), already used, or cancelled by five wrong attempts. | Restart the backend with `--pair` for a new one. |
| A speaker "refused to pair" | The speaker is not in pairing mode, or is already connected to another device. | Put it in pairing mode and tap it again. |
| Spotify signs in, but nothing plays | The Spotify account is not on the application's Development-mode allowlist, or `librespot` was never seeded. | Both steps under *Spotify* in [`INSTALL.md`](INSTALL.md). |
