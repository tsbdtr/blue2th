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
- **Or add the address by hand**: `https://<the PC's LAN address>:4000`, the
  address the backend printed when it started. **Test** answers *Backend
  answered* when the address is right.

The backend serves its API over HTTPS, with a certificate it issued itself. The
app does not need it to be signed by anyone: it remembers the backend's public
key when it pairs, and from then on talks only to a backend holding that key. A
browser has nothing of the kind, and shows a certificate warning the first time
it opens the backend.

A backend the app knows is recognised by a stable id, not by its address, so
when the PC gets a new address from the router the app repairs the entry
itself (*Repair addresses automatically*, on by default) or asks first.

## Pair

Choose **How to pair** on the backend's entry:

- **Scan the QR** — point any camera app at the QR in the backend's banner. The
  link opens blue2th, adds or updates the backend, pairs it and makes it active.
  Nothing to type. This is the recommended way.
- **Type a code** — the six characters from the banner, then **Pair**. The code
  ignores the difference between `O` and `0` or `I` and `1` by never using them.

The code is valid for **five minutes** and works **once**; five wrong attempts
cancel it. Run `blue2th-server --pair` on the PC for a fresh one.

To pair several clients from one start — a phone and a browser, say — run
`blue2th-server --pair <n>` with `n` from 1 to 10: the banner prints one block
per code, numbered `1/n` to `n/n`. Each code works once and for five minutes,
using one leaves the others valid, and the five wrong attempts are counted once
for all of them: the fifth cancels every code.

The status card on the home screen turns green, *Backend connected*, and the
speakers can be loaded. To pair a second phone, arm a new code the same way.

Pairing trusts the key the backend presents at that moment — trust on first
use — so pair on a network you trust. The app keeps a hash of that key (its
SPKI), not of the certificate: it is not the fingerprint the backend logs for
browsers, below, and the two values never match. A new certificate on the same
key, after a change of LAN address for instance, changes nothing for the app.

The app (a release build) pairs an entry whose address starts with `http://`
over `https://`, and rewrites its address so.

## Opening the backend in a browser

`http://<the PC's LAN address>:4000` redirects to the `https://` address. The
browser then warns that the certificate is not trusted: the backend issued it
itself, and no authority vouches for it. Before accepting, compare its
fingerprint with the line the backend logs at every start:

```
TLS certificate SHA-256 fingerprint: AB:CD:…:EF
```

- **Firefox**: on the warning page, **Advanced…**, then **View Certificate**;
  the **SHA-256** line under **Fingerprints** reads exactly like the log line.
- **Chrome**: click **Not secure** in the address bar, then the certificate
  entry; the **SHA-256 Fingerprint** under **Fingerprints** carries the same
  hex digits, possibly in lower case and without the colons.

When they match, accept the risk. The exception holds for this certificate
only (for how long depends on the browser): the browser warns again once the
backend issues a new one, which it logs at start, for instance when the PC gets
a new LAN address.

## What the status colours mean

| Colour | Label | Meaning | What to do |
|---|---|---|---|
| green | Backend connected | Reachable and paired. | Nothing. |
| violet | This backend's identity changed. Tap to pair it again. | Something answers at the address, but with another key than the one the phone paired with: the backend's `tls-key.pem` was deleted or replaced, or another machine now answers there. The phone sends nothing to it, its token least of all. | If the key change is expected, tap the banner and pair again: the new pairing trusts the new key. If not, find out what answers at that address first. |
| amber | Not paired — pair this backend first | The backend answers, but the phone holds no token or the backend no longer accepts it (for instance after `auth.json` was deleted on the PC). | Arm a code with `--pair` and pair again. |
| red | Backend unreachable | No answer at all from the address. | The backend is down, the address is wrong, or the network path is blocked — see below. |
| orange | This backend is too old / too new | Both sides answer, but their protocol versions do not match. | Update the side the message names. |

*Not paired* and *unreachable* look alike and are fixed differently, which is
why the app tells them apart: the first is an authentication problem on a
backend that works, the second a network problem where authentication never
comes into play. *Identity changed* is neither: the network works and a backend
answers, but not with the key the phone knows.

The violet is provisional: a colour that stands out while testing. The
redesign's mockups give this state a screen of its own (#200).

## When it does not work

| Symptom | Likely cause | Check |
|---|---|---|
| The search finds nothing | The phone is not on the PC's network: mobile data, a guest Wi-Fi, or a VPN carrying the app's traffic away. Or the backend is not running. | Wi-Fi settings on the phone: same network, an address in the same range as the PC. A VPN with split tunnelling must list blue2th as bypassed — **reinstalling the app drops it from that list**. On the PC, `curl -k https://<lan address>:4000/health`. |
| Search finds nothing, but the address typed by hand tests fine | Multicast is filtered: the router isolates Wi-Fi clients, or the PC's firewall drops mDNS. | `sudo firewall-cmd --list-services` includes `mdns`; otherwise add it (see [`INSTALL.md`](INSTALL.md)). Typing the address is a perfectly good fallback. |
| *Backend unreachable* with the right address | TCP 4000 is blocked on the PC, or the PC listens on another interface than the one the phone reaches. | `sudo firewall-cmd --list-ports`; the address the backend printed at start; `blue2th-server --bind 0.0.0.0:4000` to listen everywhere (`BLUE2TH_BIND` is read by debug builds only). |
| *Not paired* right after a backend restart | `auth.json` was deleted or unreadable on the PC, so the backend minted a new token and forgot every phone. | Pair again with the code the banner shows, or run with `--pair` for a new one. |
| *Not paired* after updating the app, on a backend that worked | The entry was paired before the app spoke HTTPS: it holds a token but no key, and an `http://` address. A release app never sends its token over plain HTTP, nor over a connection no remembered key verified. | Pair again, by code or QR: the app pairs over `https://`, rewrites the address and remembers the key. |
| *Identity changed* (violet) right after a backend restart | `tls-key.pem` was missing — the file was deleted — and the backend generated a new key, with a new token, which the log says. | Tap the banner and pair again with the code the banner shows, or run with `--pair` for a new one. |
| The backend refuses to start over `tls-key.pem` | The key file is there but unreadable or damaged. The backend leaves it as it is rather than replace a key that might be restored. | Restore the file if you have a copy. Otherwise delete it: the next start generates a new key and a new token, and every client pairs again. |
| The browser warns about the certificate again | The backend issued a new certificate at start (it logs why), for instance after a change of LAN address. | Compare the fingerprint again, as above, then accept. |
| The pairing code is refused | Expired (five minutes), already used — by another client, when several were paired from one start — or cancelled by five wrong attempts across all armed codes. | Restart the backend with `--pair` (or `--pair <n>` for several clients) for new ones. |
| A speaker "refused to pair" | The speaker is not in pairing mode, or is already connected to another device. | Put it in pairing mode and tap it again. |
| Spotify signs in, but nothing plays | The Spotify account is not on the application's Development-mode allowlist, or `librespot` was never seeded. | Both steps under *Spotify* in [`INSTALL.md`](INSTALL.md). |

## Development: a plain-HTTP backend

A debug build of the app also pairs with a backend started with
`BLUE2TH_PLAIN_HTTP=1`, which serves plain HTTP on a loopback address (debug
builds of the backend only; see [`ARCHITECTURE.md`](ARCHITECTURE.md)). Reach it
from a phone over USB:

```bash
BLUE2TH_PLAIN_HTTP=1 cargo run -p blue2th-server -- --pair
adb reverse tcp:4000 tcp:4000
```

then add `http://127.0.0.1:4000` by hand and pair by code. A debug build keeps
the `http://` a typed address carries, and such an entry holds a token and no
key. A release build turns a typed `http://` into `https://`, and reads an
`http://` entry as *not paired*.
