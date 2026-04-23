# mcp-rustdesk

Let an LLM **see and control a remote machine over the RustDesk protocol**.

This MCP server exposes the live RustDesk peer connection (E2EE NaCl
+ AV1/H264/VP9 codecs + NAT traversal via your own hbbs) as a set of
tools any MCP-compatible client (Claude Code, Claude Desktop, Cursor,
…) can drive: connect, screenshot, click, scroll, type, key, plus an
optional Gemini Live "watcher" that streams the frames to Gemini in
parallel for continuous narration.

> First public MCP that wraps RustDesk. Inspired by the existing
> [VNC](https://github.com/hrrrsn/mcp-vnc) and computer-use MCPs, but
> with the security and codec advantages of RustDesk's protocol.

Runs on **Linux, macOS and Windows** (daemon + MCP server). The
daemon ships a cross-platform loopback IPC (TCP 127.0.0.1 + token
auth) so the same code path works on every host.

## Why RustDesk and not VNC?

| | VNC MCPs | mcp-rustdesk | Anthropic Computer Use |
|---|---|---|---|
| Stream  | RFB framebuffer | AV1/H264/VP9 decoded live | local OS screenshot |
| Encryption | none unless you tunnel it | E2EE NaCl, native | local only |
| NAT traversal | tunnel-yourself (SSH, WireGuard…) | hbbs hole-punch built-in | n/a |
| Bandwidth | raw or zlib | modern codecs (~10× better) | n/a |
| MCP returns | PNG on demand | PNG on demand | PNG on demand |

The MCP layer always returns one PNG per `screenshot` call — no
multimodal LLM API today (April 2026) ingests a real video stream as
input. The Gemini Live watcher (optional) bridges that gap by feeding
frames continuously to Gemini and exposing its textual observations
back through MCP.

## Architecture

```
LLM client (Claude Code, etc.)
    │ stdio MCP
    ▼
mcp-server/server.py            ← Python, FastMCP
    │ TCP 127.0.0.1 + token JSON-RPC
    ▼
rustdesk-headless               ← Rust, built from RustDesk + our patch
    │ RustDesk protocol (E2EE)
    ▼
remote peer (any RustDesk client, any OS)
```

At startup the daemon binds `127.0.0.1:0` (OS picks a free port),
generates a 32-byte random auth token, and writes a user-only
rendezvous file (`{host, port, token, pid}`) the Python server reads
to locate and authenticate to it. No Unix sockets, no named pipes —
one code path on every OS.

A local HTTP viewer (`rustdesk_viewer_start`) lets a human watch the
exact frames the LLM sees, in real time.

## Repository layout

```
.
├── headless/                    Rust source for the headless RustDesk binary
│   ├── headless.rs              gets copied into rustdesk/src/bin/
│   ├── lib.rs.patch             visibility patch on the rustdesk crate
│   ├── apply.sh                 Linux / macOS: clone, patch, build
│   └── apply.ps1                Windows:       clone, patch, build
└── mcp-server/                  Python MCP server
    ├── server.py
    ├── requirements.txt
    └── pyproject.toml
```

## Install

### Option A — pre-built release (quickest)

Download the archive for your OS from the
[latest release](https://github.com/meumeu-dev/mcp-rustdesk/releases):

- `rustdesk-headless-linux-x64.tar.gz`
- `rustdesk-headless-macos-arm64.tar.gz` / `-macos-x64.tar.gz`
- `rustdesk-headless-windows-x64.zip`

Extract the binary anywhere on your `$PATH` (or note its absolute path).

### Option B — build from source

First build pulls ~700 crates plus libvpx/libyuv/opus/aom via vcpkg —
count 30–60 min.

**Linux / macOS:**

```sh
git clone https://github.com/meumeu-dev/mcp-rustdesk.git
cd mcp-rustdesk
./headless/apply.sh                    # clones rustdesk + patches + cargo build
```

Outputs `rustdesk/target/release/rustdesk-headless`.

**Windows (PowerShell):**

```powershell
git clone https://github.com/meumeu-dev/mcp-rustdesk.git
cd mcp-rustdesk
.\headless\apply.ps1
```

Outputs `rustdesk\target\release\rustdesk-headless.exe`. Requires Visual
Studio 2022 Build Tools, Rust (`rustup default stable-msvc`), and
`choco install cmake nasm yasm`.

### Set up the Python MCP server

```sh
python3 -m venv mcp-server/.venv
mcp-server/.venv/bin/pip install -r mcp-server/requirements.txt
```

On Windows:

```powershell
python -m venv mcp-server\.venv
mcp-server\.venv\Scripts\pip install -r mcp-server\requirements.txt
```

### Register the MCP with your client

Claude Code (Linux / macOS):

```sh
claude mcp add rustdesk -s user \
  -e GEMINI_API_KEY="..." \
  -e RUSTDESK_VIEWER_PORT=8765 \
  -- "$PWD/mcp-server/.venv/bin/python" "$PWD/mcp-server/server.py"
```

Or as plain JSON in your client's MCP config:

```json
{
  "mcpServers": {
    "rustdesk": {
      "command": "/abs/path/to/mcp-server/.venv/bin/python",
      "args": ["/abs/path/to/mcp-server/server.py"],
      "env": {
        "GEMINI_API_KEY": "AIza...",
        "RUSTDESK_VIEWER_PORT": "8765"
      }
    }
  }
}
```

### Start the daemon

```sh
./rustdesk/target/release/rustdesk-headless &
```

Windows:

```powershell
Start-Process -FilePath .\rustdesk\target\release\rustdesk-headless.exe
```

The daemon binds `127.0.0.1:<random port>` and writes a rendezvous
file containing the port and auth token. The Python MCP server reads
it to connect — no manual port / token plumbing needed.

Rendezvous file location (auto-detected by both daemon and client):

| OS | Path |
|---|---|
| Linux (systemd) | `$XDG_RUNTIME_DIR/rustdesk-headless/daemon.json` |
| Linux (fallback) / macOS | `$TMPDIR/rustdesk-headless-<uid>/daemon.json` |
| Windows | `%LOCALAPPDATA%\RustDesk-headless\daemon.json` |

Run the daemon as the same user that runs the MCP server (file
permissions are user-only).

If you self-host RustDesk (recommended), make sure
`~/.config/RustDesk-headless/RustDesk-headless2.toml` has your
`custom-rendezvous-server` and `key`, OR pass them per-call to
`rustdesk_connect`. Alternatively, copy your existing `~/.config/rustdesk/`
to `~/.config/RustDesk-headless/` once at setup.

### Environment variables

Consumed by the Python server:

| Var | Default | Purpose |
|---|---|---|
| `RUSTDESK_HEADLESS_RENDEZVOUS` | platform default (see above) | override the rendezvous file path |
| `GEMINI_API_KEY` | (unset) | required only if you call `rustdesk_watch_start` |
| `RUSTDESK_VIEWER_PORT` | (unset) | auto-start the local HTTP viewer on that port |
| `RUSTDESK_VIEWER_FPS` | 5 | refresh rate of the HTTP viewer |

Consumed by the daemon:

| Var | Default | Purpose |
|---|---|---|
| `RUSTDESK_APPNAME` | `RustDesk-headless` | suffix that isolates config / IPC from a classic RustDesk install. Any other name is accepted; `RustDesk` is **refused** to avoid clobbering a classic client. |
| `RUSTDESK_HEADLESS_RENDEZVOUS` | platform default | override the rendezvous file path |

## MCP tools

| Tool | Args | Returns |
|---|---|---|
| `rustdesk_connect` | `peer_id`, `password`, `key?`, `rendezvous_server?` | session ack |
| `rustdesk_disconnect` | — | "disconnected" |
| `rustdesk_status` | — | `{connected, width, height, peer_id}` |
| `rustdesk_screenshot` | — | PNG image |
| `rustdesk_move_mouse` | `x`, `y` | ack |
| `rustdesk_click` | `x`, `y`, `button="left"`, `double=false` | ack |
| `rustdesk_scroll` | `x`, `y`, `dy` | ack |
| `rustdesk_type` | `text` | ack |
| `rustdesk_key` | `key`, `modifiers=[]` | ack |
| `rustdesk_watch_start` | `prompt`, `fps=1`, `model="gemini-2.0-flash-live-001"` | ack |
| `rustdesk_watch_stop` | — | ack |
| `rustdesk_watch_observations` | `drain=true` | `{observations: [...], frames_sent, sessions_opened}` |
| `rustdesk_viewer_start` | `port=8765`, `fps=5` | URL |
| `rustdesk_viewer_stop` | — | ack |

## Security

- Daemon binds `127.0.0.1:<random>` only; the first frame on every
  connection must be an `auth` call with the shared token from the
  rendezvous file. Mismatched / missing tokens are rejected before any
  RPC is processed. The token comparison is constant-time.
- Rendezvous file is written user-only (`0600` on Unix; on Windows it
  lives under `%LOCALAPPDATA%` which is user-ACL'd by default).
- The HTTP viewer binds to `127.0.0.1` only — tunnel via SSH if you
  want remote access.
- Passwords pass through the loopback connection, then the RustDesk
  protocol's E2EE — they are never logged.
- The daemon never executes shell commands on either end.
- Screenshots go in MCP responses; if your LLM provider logs prompts,
  every frame the LLM sees is logged with them. Treat the remote peer
  accordingly.
- The daemon refuses to start with `RUSTDESK_APPNAME=RustDesk` so it
  can never collide with an installed classic RustDesk client's config
  dir, named pipe, or IPC socket.

## Status

MVP. Tested on Linux x86_64 hosts driving Linux and Windows peers
through a self-hosted hbbs/hbbr. macOS and Windows host builds are
produced by CI (see `.github/workflows/build.yml`); tested via CI,
community testing welcome. Patches against RustDesk 1.4.x.

## Caveats

- **Peer-side input permissions** must be granted on the remote
  RustDesk client (Authorizations panel: keyboard, mouse, clipboard).
  Without them the daemon connects, frames stream, but `_click` /
  `_type` / `_key` calls are silently dropped by the peer. A missing
  permission looks identical to a wrong key mapping — verify by
  clicking the corresponding toggle in the remote RustDesk UI.
- Multi-display peers: only display 0 is exposed for now.
- The Gemini Live watcher's video sessions cap at ~2 minutes; the
  watcher reconnects transparently but you may see brief gaps in
  observations.
- **macOS:** first run will ask for *Accessibility* and *Screen
  Recording* permissions (System Settings → Privacy & Security). These
  are granted per-binary path, separate from any RustDesk.app grant.
- **Windows:** SmartScreen may block the first run of an unsigned
  binary — "More info" → "Run anyway". If your organization runs
  AppLocker, whitelist the binary path.
- The daemon stores its identity under a separate config dir
  (`RustDesk-headless/`) so it never interferes with a classic RustDesk
  install. The trade-off: you need to copy or re-enter your custom
  rendezvous server and key once.

## License

MIT for our patches and the Python server. The compiled
`rustdesk-headless` binary, being a derivative work of RustDesk, is
distributed under AGPL-3.0. See `LICENSE`.
