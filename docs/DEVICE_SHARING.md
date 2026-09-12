# Device sharing — use a phone attached to another machine

The phone is plugged into machine **A**. You want `adb` on machine **B**. A and B can reach
each other over a LAN or a Netbird/WireGuard overlay, and neither is on the public internet.

`dialf devices share` on A exposes A's adb server; B reaches it and then uses stock tooling —
`adb shell`, `install`, `logcat`, `push`/`pull` all behave as if the phone were plugged into B.

> **Status: not released yet.** Until a release is cut, both A and B need a local build:
> `cd server && cargo build --release --bin dialf`.

---

## Which route to take

The share listens on `0.0.0.0:5939` by default and expires after an hour. What you choose is
how B proves it's allowed in.

| | Route 1 — open | Route 2 — token | Route 3 — SSH tunnel |
|---|---|---|---|
| On A | `share --target …` | `share --target … --token` | `share --bind 127.0.0.1:5939` |
| On B | stock `adb -H A` | `dialf devices connect` | `ssh -L`, stock `adb` |
| Who can use the phone | anyone who can reach the port | whoever holds the token | whoever can SSH to A |
| Encrypted | only if your overlay does it | no | yes |
| dialf on B | not needed | needed | not needed |

**On a Netbird/WireGuard mesh, Route 1 is reasonable**: the overlay already decides who can
reach the port and encrypts the traffic. **On a plain LAN it is not** — anyone on the network
can drive the phone.

**Route 2** adds a secret on top, for when the network itself isn't the boundary you want.
**Route 3** is the strongest and needs nothing installed on B, if you have SSH to A.

All three enforce the same device scope and the same blocked-command policy — no token does
not mean no policy.

---

## Route 2 — token over LAN / Netbird

### On A (phone attached)

**1. Find the phone's serial**

```sh
dialf devices share --list
# 192.168.100.179:33415   device   Pixel 9 Pro
```

That serial is what `--target` takes. A USB phone shows a short serial (`4B2B1C`); a phone on
adb-over-WiFi shows `ip:port`.

**2. Pick the address to bind**

This is the decision that matters most.

- **Netbird/WireGuard IP** (e.g. `100.117.207.150`) — **preferred**. Only overlay peers can
  reach the port; machines on the plain LAN cannot reach it at all. WireGuard also encrypts
  the traffic, which covers the one thing the token does not do.
- **LAN IP** (e.g. `192.168.1.x`) — reachable by anything on the LAN. The token still gates
  who may *use* it, but the session is plaintext, so someone on the wire could sniff or
  hijack it after the handshake.
- **`0.0.0.0`** — both of the above at once. Avoid unless you mean it.

Find your Netbird address with `netbird status`, or `ifconfig` and look for the `100.x.x.x`
address on the `wt0`/`utun*` interface.

**3. Start the share, asking for a token**

```sh
dialf devices share --target 192.168.100.179:33415 --bind 100.117.207.150:5939 --token
```

It prints the token **once**:

```
  token: dvs_oGuYrg9PePnP
  (shown once, not saved — a restarted share issues a new one)
```

Copy it now. It is generated per share and held only in memory — there is no config field and
no file to read it back from, so losing it means restarting the share for a new one.

Share several phones by repeating `--target`, or expose everything with `--all`.

**4. Check it**

```sh
dialf devices share --status
```

### On B (the machine that wants adb)

**1. Open the connection** — leave this running, like an SSH tunnel:

```sh
dialf devices connect 100.117.207.150:5939 --token dvs_oGuYrg9PePnP
```

It holds `127.0.0.1:5038` locally. The token can also come from `$DIALF_SHARE_TOKEN` instead
of the flag.

**2. In another shell, use adb normally**

```sh
export ADB_SERVER_SOCKET=tcp:127.0.0.1:5038

adb devices
adb shell
adb install app.apk
adb logcat
adb pull /sdcard/file.txt
```

Without the env var, pass `-H 127.0.0.1 -P 5038` to each command.

---

## Route 1 — open share on a trusted network

```sh
# on A
dialf devices share --target 192.168.100.179:33415
```

Then from B, with nothing installed:

```sh
export ADB_SERVER_SOCKET=tcp:100.117.207.150:5939
adb devices
```

A prints an unmissable warning when it starts an open share, because on the wrong network this
hands the phone to anyone who can reach the port.

## Route 3 — SSH tunnel (nothing on B, encrypted)

Bind the share to loopback so only A itself can reach it:

```sh
# on A
dialf devices share --target 192.168.100.179:33415 --bind 127.0.0.1:5939
```

Then from B:

```sh
ssh -L 5939:127.0.0.1:5939 A
# in another shell on B:
export ADB_SERVER_SOCKET=tcp:127.0.0.1:5939
adb devices
```

SSH is what keeps B out until B is authenticated, and it encrypts the session — which neither
of the other routes does on its own.

---

## Expiry

Every share stops itself after `--expire-after` seconds — **one hour by default**. A share is a
door held open; this is what closes it when you forget.

```sh
dialf devices share --target … --expire-after 900   # 15 minutes
dialf devices share --target … --expire-after 0     # never (warns)
```

Both the start output and `--status` say when it closes. Expiry drops connections that are
already open, not just the listener — otherwise a held `adb shell` would outlive the deadline,
which is the thing expiry exists to prevent.

---

## Why `dialf devices connect` exists

`adb` cannot authenticate. `adb -H host -P port` opens a socket and immediately starts speaking
the adb protocol — there is no hook for a handshake. So when the share issues a token,
something on B has to perform that handshake on adb's behalf. That is the entire job of
`dialf devices connect`: it holds a local port, authenticates once per connection, and splices.

This is why it is needed **only for Route 2**. A share started without `--token` has no
handshake, so pointing the shim at one is refused with advice to use `adb` directly.

---

## What protects you

| Layer | What it stops |
|---|---|
| Netbird bind | Anyone not on your overlay — they cannot reach the port at all |
| WireGuard | Sniffing or hijacking the session in transit |
| `--token` | Anyone who can reach the port but does not hold the secret |
| `--target` | Reaching a phone you did not share, even with a valid token |
| adb gate | `adb kill-server` from B stopping A's adb server |
| Expiry | A share you forgot to stop |

Three properties worth stating plainly, because they are easy to assume wrongly:

**A share with no token is open to whoever can reach it.** The default bind is
`0.0.0.0:5939`, so on a plain LAN that is everyone on the LAN. The network is the boundary;
pick the bind accordingly, or use `--token`.

**The token authenticates; it does not encrypt.** After the handshake the adb session is
plaintext. On Netbird/WireGuard that is fine — the transport is already encrypted. On a plain
LAN it is not: use Route 3, or bind to an overlay address.

**No token does not mean no policy.** Device scope and the blocked-command list are enforced
on an open share exactly as on a token-protected one.

---

## Making it persistent on A

By default sharing is off and must be started each time, so an exposed port does not outlive
the session that wanted it. To have it come back with the daemon:

```yaml
adb_share:
  enabled: true
  bind: 100.117.207.150:5939
  targets: ["192.168.100.179:33415"]     # or: all: true
  expire_after: 3600                     # seconds; <= 0 never expires
```

There is no `token:` field, by design — a token is minted per share and printed once, so a
config-started share is always an open one. If you want a token, start the share from the CLI.

---

## Sharing more than one phone

```sh
dialf devices share --target 4B2B1C --target 9F1E2D
dialf devices share --all
```

With several devices shared, B must say which one: `adb -s 4B2B1C shell`. A bare `adb shell`
is refused rather than picking one at random — otherwise `--target` would be a coin flip over
which phone you reached.

---

## Stopping

```sh
dialf devices share --stop      # closes the port, drops in-flight connections
```

On B, Ctrl+C the `dialf devices connect` process.

---

## Gotchas

- **Version skew.** The adb *client* kills and restarts a server whose version does not match.
  Through a share that attempt is blocked (you will see `host:kill is blocked`), so nothing
  breaks on A — but B may get confusing errors instead of a clean failure. Keep platform-tools
  roughly aligned across A and B.
- **Sharing never guesses.** No `--target` and no `--all` is an error that lists what is
  attached. A host that gains a second phone will not start sharing it silently.
- **`--list` needs A's adb server running.** If it is not, dialf starts it for you; if `adb`
  is not installed on A at all, `--status` reports the upstream as unreachable.
- **A machine cannot reach its own overlay IP.** Netbird/Tailscale route `100.x` addresses
  off-box and do not hairpin, so A cannot test its own share at its Netbird address — only B
  can. From A, use loopback or A's LAN address; otherwise adb hangs until it times out.
- **The shared port carries full device access.** Anyone who gets through every layer above can
  install apps, read `/sdcard`, and open a shell on a phone holding a live SIM. Treat the token
  like an SSH key.
