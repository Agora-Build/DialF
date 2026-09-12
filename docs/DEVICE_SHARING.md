# Device sharing — use a phone attached to another machine

The phone is plugged into machine **A**. You want `adb` on machine **B**. A and B can reach
each other over a LAN or a Netbird/WireGuard overlay, and neither is on the public internet.

`dialf devices share` on A exposes A's adb server; B reaches it and then uses stock tooling —
`adb shell`, `install`, `logcat`, `push`/`pull` all behave as if the phone were plugged into B.

Both machines need `dialf` (0.3.0 or newer):

```sh
npm install -g @agora-build/dialf
# or: curl -fsSL https://dl.agora.build/dialf/install.sh | bash
```

B needs it only for the token route; the other two work with stock `adb` on B.

---

## Quick version

```sh
# A — the machine with the phone
dialf devices share --list                     # find the serial
dialf devices share --target 4B2B1C            # share it (open, expires in 1h)

# B — the machine that wants adb
export ADB_SERVER_SOCKET=tcp:<A-address>:5939
adb devices
```

That is the whole thing on a trusted network. The rest of this guide is about the cases where
"trusted" doesn't hold, and about what the share does and doesn't protect.

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

## Tools that use an adb tunnel (scrcpy, flutter run, Android Studio)

Plain `adb` — `shell`, `install`, `logcat`, `push`, `pull` — works over a share with nothing
extra. Tools that open a **tunnel** need one more step, and it is worth knowing why before you
hit it.

scrcpy, `flutter run` (the VM service) and Android Studio's debugger all create a socket with
`adb forward` or `adb reverse`. Those run *on the adb server's host* — machine A — so the
socket lands there, on A's loopback:

```sh
# on A, after `adb forward tcp:7799 …`
$ lsof -nP -iTCP:7799 -sTCP:LISTEN
adb    127.0.0.1:7799          ← loopback on A, not reachable from B
```

The tool on B then waits on *B's* localhost and nothing ever connects. scrcpy reports it as:

```
/…/scrcpy-server: 1 file pushed, 0 skipped.
[server] INFO: Device: [samsung] samsung SM-G977U (Android 12)
ERROR: Server connection failed
```

Note that the push succeeded — the share is fine; only the tunnel is in the wrong place.

### Fix 1 — `--forward-port` (no SSH needed)

Have the share proxy the tunnel port alongside the adb port:

```sh
# on A
dialf devices share --target R3CM40KGDVY --forward-port 27183

# on B
export ADB_SERVER_SOCKET=tcp:<A-address>:5939
scrcpy --tunnel-host=<A-address> --tunnel-port=27183
```

`--tunnel-port` pins the port scrcpy asks `adb forward` to open (27183 is its default) and
implies `--force-adb-forward`; `--tunnel-host` points scrcpy at A instead of its own
localhost. Repeat `--forward-port` for several.

On a **token** share, a forwarded port can't ask for the token — the tool opens it with a
plain socket. It is gated on the peer's address instead: only a machine that already
authenticated on the adb port may use it. That is weaker than the handshake (addresses can be
spoofed, and a NAT makes several machines look like one), so treat a forward port as the
looser half of a token share.

`--forward-port` needs a network bind; it is refused on a loopback share, where `adb forward`
already owns `127.0.0.1:<port>` and Fix 2 applies instead.

### Fix 2 — SSH tunnel the port

If you would rather not open another port, tunnel it:

```sh
# on B, leave running
ssh -L 27183:127.0.0.1:27183 user@A

# on B, another shell
export ADB_SERVER_SOCKET=tcp:<A-address>:5939
scrcpy --tunnel-port=27183
```

Here `--tunnel-host` stays at its default (B's localhost), which SSH maps to A's loopback.

### Fix 3 — skip the share for that tool

Put the phone on adb-over-WiFi and let B own it directly:

```sh
# on A
adb -s R3CM40KGDVY tcpip 5555

# on B
unset ADB_SERVER_SOCKET
adb connect <phone-ip>:5555
scrcpy                       # no flags: the tunnel is local again
```

Needs B to reach the phone's own address. This takes dialf out of the picture for that tool.

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

A `--forward-port` is the one component gated by address rather than by the token — see
[Tools that use an adb tunnel](#tools-that-use-an-adb-tunnel-scrcpy-flutter-run-android-studio).

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
  forward_ports: [27183]                 # tunnel ports for scrcpy etc (optional)
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
