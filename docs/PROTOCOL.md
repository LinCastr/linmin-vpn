# Radmin VPN GUI ⇄ service protocol (reverse engineered)

Goal: talk to `RvControlSvc.exe` directly from linmin-vpn so we never open the
bundled Wine GUI (`RvRvpnGui.exe`), not even to join or leave a network.

Everything here was recovered by proxying the real pipe and decoding the frames
(see "Tools" below). It is **incomplete** and only covers what has been observed;
treat tag/command numbers as confirmed only where a column says so. All work was
done against the user's own install, own networks and own passwords.

## Transport

- Named pipe **`\\.\pipe\RadminVpnGuiChannel`**, created by the service.
  `GetNamedPipeInfo` reports message-type, 200-byte in/out buffers. The GUI opens
  it and uses it as a **byte stream** with its own framing (setting the client to
  message read-mode fails and is not needed).
- The service also uses an event object `RadminVPN_{...}_evt` and a GUI-side
  app mutex `RadminVPN_{A3234D4F-...}_app`. The IPC class hierarchy is
  `FamRT::CNamedPipe` / `FamRT::CPipeClient` (RTTI in both binaries).
- **Framing:** stream of frames. Each frame = `u32 length (little-endian)` then
  `length` bytes of body. A frame may span several pipe reads; reassemble by
  length. One frame = one message.

## Message body = TLV attributes

The body is a flat list of attributes; some attributes nest another list.

Each attribute header is **8 bytes**:

| bytes | field | notes |
|---|---|---|
| 4 | value length | **big-endian** u32, length of the value only |
| 1 | type | see table |
| 1 | flags | string encoding/misc; often 0 |
| 2 | tag | **big-endian** u16, the attribute id |

Then `value length` bytes of value.

| type | meaning | value |
|---|---|---|
| 0x00 | container | nested attribute list |
| 0x01 | u32 | big-endian |
| 0x02 | u64 | big-endian |
| 0x0b | bool | big-endian u32, 0/1 |
| 0x03 | string | UTF-16**BE**; often ends with `\x01`; `flags` is an encoding selector |
| 0x08 | bytes | raw (used by the password blob) |
| 0x09 | 16 bytes | IPv6 address |
| 0x0d | 16 bytes | GUID / 128-bit id |

Decoder: `tools/rvdecode.py` (reads a `pipespy` hex log, prints the tree).

## Direction conventions

- **GUI → service (command):** usually first attr `034a` (request id, u32) then
  `0300` (command code, u32), then a container with the args. `034a = 0xffffffff`
  means "no reply expected" (used by the periodic visible-nodes ping).
- **Service → GUI (reply):** `0300` (echoes the command) + `0310` (result, 0 = ok)
  + args + `034a` (echoes the request id).
- **Service → GUI (event, unsolicited):** `0301` (event/message type, u32) +
  `03d6` (a monotonically increasing sequence counter) + payload.

Command codes (`0300`) and event types (`0301`) are **separate number spaces**.
They line up with the service's RTTI classes `ControlSvc::msg::cmd::C*`
(commands) and `ControlSvc::msg::C*` (messages/events) — dump with
`strings RvControlSvc.exe | grep -oE '\.\?AV.*msg@ControlSvc'`.

## Commands observed (GUI → service, tag `0300`)

| code | name (from RTTI) | args container | fields seen |
|---|---|---|---|
| 2 | CreateNetwork | `132b` | `0308` bool, `0306` name (str), `030d` password blob |
| 3 | JoinNetwork | `131b` | `033f` bool (GUI "Gaming network" tab), `0306` name, `030d` password blob — omitted when joining without password (sends a join request an admin must approve) |
| 4 | SwitchPower | — | `0311` bool (0 = off, 1 = on) |
| 5 | LeaveNetwork | — | `0309` network GUID (16) |
| 10 | ApproveJoin | — | `0309` network GUID, `01e1` node id (u64) — confirmed statically: `CApproveJoin` ctor passes 10 to the base command |
| 14 | UpdateNetwork (properties) | `132a` | `0309` GUID, `0306` name, `0308`/`0323`/`031c` bool |
| 15 | DeleteNetwork | — | `0309` network GUID |
| 18 | ChangeAlias (rename self) | — | `0304` new name (str) |
| 20 | UpdateVisibleNodes | `1344` | `01e1` node id, `0376` u32 — sent every ~3 s |
| 28 | LanguageChanged | — | `025a` u32 ×2 (lang ids, e.g. 1033, 1046) |

Still unmapped (RTTI names them; code numbers unknown): KickNode,
GrantAdmin, RevokeAdmin, ChangeAlias-of-node, ChatCommand/OpenChat, EnableNetwork,
EnumPublicNetworks, SetSettings. Each command's code is the immediate the
service pushes before calling the base-command constructor in the class ctor
(find it by the vtable write `mov [esi], <vtable>` of the RTTI class).

## Events / replies observed (service → GUI, tag `0301`)

| type | meaning | key fields |
|---|---|---|
| 34 | full state after connect | networks + nodes tree (see below) |
| 7 | NetworksChanged | `131f` { `1329` network, `131e` node } |
| 5 | connect/handshake progress | `0312` state u32, `0313` bool, `01c9` public ip |
| 20 | connected-to-slave? | `0322` u64 |
| 23 | node connected in/out | `1266` channel { endpoints, rtt } |
| 28 | latency tick (~10 s) | `01e1` node id, `034d` u32 (ms?) |
| 21,24,26,33,35,36,… | per-node detail updates | version `01ed`, rtt `03a0`, etc. |

Network properties (`DlgNetworkProperties`): `0308` is "Allow sending connect
requests without password" (joiners without password wait for approval; the
password still works), `0323` is "Block joining for new members".

Roles (`030a`): on a network container it is this machine's role; inside a
membership ref (`1324`) it is that member's role. `0` Applicant (waiting for
approval), `1` Member, `2` Administrator. The service sends the full state only
on connect; a client that connects before the service is switched on gets an
empty one, so the app reconnects to resync.

## Attribute tags seen

Scalars: `0300` cmd code · `0301` event type · `0303` node caps (u64) ·
`0304` alias/computer name · `0305` VPN IPv4 (u32) **and** IPv6 (16 bytes) ·
`0306` network name · `0308`/`031c`/`0323` network bool props · `0309` network
GUID (16) · `030a` node/network kind (1,2) · `030d` **password blob** (see below) ·
`0310` result (0 = ok) · `0311` power bool · `0312` connect state · `033f` bool ·
`034a` request id · `034d` u32 (rtt?) · `0376` visible-nodes param · `037e` online
bool · `0381`/`0383` u64 timestamps (FILETIME-like) · `03a0` u32 (rtt) · `03a3`
u32 (member count) · `03d6` sequence counter · `03e0` u32 (=3, direct conn?) ·
`01c1` channel id (u64) · `01c2` remote ip (str) · `01c3` remote port (u32) ·
`01c9` public ip (str) · `01e1` node id (u64) · `01e9` "local → remote" endpoints
(str) · `01ed` version (str, "2.1.1") · `01f1` rtt ms (u32, 75).

Containers: `1266` channel · `127d` endpoint · `1315` created-network ·
`131b` join-args · `131e` node · `131f` networks-changed · `1321` networks-root ·
`1322`/`1323` network · `1324` node-in-network · `1327`/`1328` adapter ·
`132a` update-network-args · `132b` create-network-args · `133c` settings ·
`1344` visible-nodes-args · `135d`/`135e` rtt table.

## Password blob (`030d`) — SOLVED

The password sent with CreateNetwork / JoinNetwork uses a random One-Time Pad mask over UTF-16LE plaintext:

1. Let $P$ be the null-terminated UTF-16LE representation of the password:
   $$P = \text{UTF-16LE}(\text{password} + \text{"\0"})$$
   The byte length of $P$ is $N = 2 \times (\text{len(password)} + 1)$.
2. Generate a random nonce $R$ of length $N$ bytes (e.g. from `/dev/urandom`).
3. Compute the masked ciphertext $M$:
   $$M = P \oplus R$$
4. The final wire blob for attribute `030d` is the concatenation of the masked bytes and the nonce:
   $$\text{blob} = M \mathbin{\Vert} R$$
   Total length is $2N = 4 \times (\text{len(password)} + 1)$ bytes.

The service (`RvControlSvc.exe`) splits the blob into two equal halves of $N$ bytes and computes $(M \oplus R) = P$ to recover the plaintext UTF-16LE password. The network name is **not** involved in key derivation. The apparent 4 bytes per character was an artifact of two halves of 2-byte UTF-16LE words.

Verified live against the official Radmin service: network creation with this encoding succeeds with result `0x310 = 0`.

## Tools (reverse engineering only — not part of the app)

- `bridge/pipespy.c` → `pipespy.exe` (build: `zig cc -target x86-windows-gnu`).
  Listens on `\\.\pipe\RadminVpnGuiChannex`, connects to the real
  `...Channel`, relays both ways, hex-logs every frame.
- Patch a copy of the GUI to talk to the spy: replace the UTF-16LE string
  `RadminVpnGuiChannel` → `RadminVpnGuiChannex` in a copy of `RvRvpnGui.exe`
  (one occurrence) and run that copy while the spy is up.
- Run both inside the launcher's Wine env (same `WINEPREFIX`, drop `LD_PRELOAD`).
- `tools/rvdecode.py` decodes a spy log into the attribute tree.

## Full-state tree (event 34) — confirmed live

Verified against a live capture (`src/pipe.rs` + `rvrelay.exe`, see below):

- Top of the event is the **self node** container (`1320`): `01e1` id, `0304`
  alias, `0311` power bool, `0312` connect state, `0305` own IPv4 (u32) then the
  same tag again as IPv6 (16 bytes), `01c9` public ip. It nests the networks
  root `1321`.
- Inside `1321`:
  - **Node detail** records `1322` — one per known node: `01e1` id, `0304`
    alias, `01c9` public ip, `01ed` version, `0305` IPv4 (u32) + IPv6, `03a0`
    rtt, `1266` channel (present when connected).
  - **Network** `1323`: `0309` GUID, `030a` kind (2 = network), `03a3` member
    count, `0306` name, `0381`/`0383` timestamps, then **membership refs**
    `1324`, each carrying only `01e1` node id + `030a` kind (1 = node). The rich
    data is matched in from the `1322` records by node id.
- There is **no plain online flag** (`037e` was not seen); treat a node as online
  when it has a `1266` channel or a measured rtt.

## Transport bridge for Linux

`bridge/rvrelay.c` → `rvrelay.exe` (build: `zig cc -target x86-windows-gnu`).
Runs inside the service's own Wine prefix, connects to
`\\.\pipe\RadminVpnGuiChannel` as the GUI would, and relays raw bytes between
the pipe and its own stdin/stdout. No logging, no TCP port. `src/pipe.rs` spawns
it with `wine rvrelay.exe` and pumps frames. The real GUI must not run at the
same time (one GUI client per pipe).

## Next steps

1. Confirm command `10` and the remaining right-click actions against captures if needed.
2. Polish window floating / size hints in Hyprland.
