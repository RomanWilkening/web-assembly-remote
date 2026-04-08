WebAssembly Remote Desktop

Low-latency remote desktop streaming from a Windows 11 machine to any modern browser.
The server captures the screen, hardware-encodes it with **AMD AMF** (RX 6800 XT), and
streams H.264 over WebSocket.  The client is a **WebAssembly** app that decodes with the
browser's WebCodecs API (also hardware-accelerated) and renders on a `<canvas>`.

## Architecture

```
┌─────────────────── Windows 11 Server ───────────────────┐
│                                                         │
│  Screen Capture ──► FFmpeg (h264_amf) ──► WebSocket ──► │──┐
│  (DXGI)              ultra-low-latency     (axum)       │  │
│                                                         │  │  HTTPS / WSS
│  Input Simulator ◄── WebSocket ◄────────────────────────│◄─┤  (Apache
│  (SendInput API)                                        │  │   Reverse
└─────────────────────────────────────────────────────────┘  │   Proxy)
                                                             │
┌─────────────────── Browser (Client) ────────────────────┐  │
│                                                         │  │
│  WebSocket ──► WASM Protocol Parser ──► WebCodecs ──►   │◄─┘
│                                          (H.264 HW      │
│  Input Handler (keyboard/mouse) ──► WASM Encoder ──►    │
│                                       WebSocket         │
│                                                         │
│  Canvas Renderer  ◄── VideoFrame                        │
└─────────────────────────────────────────────────────────┘
```

## Latency Optimisations

| Layer | Technique |
|-------|-----------|
| Capture | DXGI Desktop Duplication (GPU-resident, ~1 ms) |
| Encoding | AMD AMF `ultralowlatency` preset, no B-frames, CQP rate control |
| Framing | Minimal binary protocol (10-byte header per frame) |
| Transport | WebSocket binary, TCP_NODELAY, channel buffer of 2 frames |
| Decoding | WebCodecs `optimizeForLatency: true` (browser hardware decoder) |
| Rendering | `desynchronized` canvas, newest-frame-only policy |
| Input | Throttled at ~125 Hz, sent immediately over WebSocket |

## Prerequisites

- **Windows 11** with AMD RX 6800 XT (or any GPU supported by AMF)
- [Rust](https://rustup.rs/) toolchain (stable)
- [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/) (`cargo install wasm-pack`)
- [FFmpeg](https://ffmpeg.org/) in PATH, built with `--enable-amf` (most Windows builds include this)
- A modern browser with WebCodecs support (Chrome 94+, Edge 94+)

## Quick Start

### 1. Build

```powershell
# PowerShell (Windows)
.\build.ps1

# or for a release build:
.\build.ps1 -Release
```

### 2. Configure

Copy and edit the configuration file:

```powershell
copy config.toml.example config.toml
# Edit config.toml to set your username and password
```

**config.toml** format:

```toml
[auth]
username = "admin"
password = "your-secure-password"
```

### 3. Run the server

```powershell
cd server
.\target\debug\wasm-remote-server.exe --config ..\config.toml --encoder h264_amf --fps 60 --quality 20
```

CLI flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--host` | `0.0.0.0` | Bind address |
| `--port` | `9090` | Bind port |
| `--fps` | `60` | Target frame rate |
| `--quality` | `20` | H.264 QP value (lower = better quality, higher bitrate) |
| `--encoder` | `h264_amf` | FFmpeg encoder name (`h264_amf`, `libx264`, …) |
| `--static-dir` | `./static` | Path to client web files |
| `--config` | `config.toml` | Path to TOML configuration file |

### 4. Open in browser

Navigate to `http://localhost:9090`. You will be prompted to log in with the credentials from `config.toml`.

## Features

### Authentication
- Login page with credentials from `config.toml`
- Session-based authentication (HttpOnly cookies)
- All routes protected, including WebSocket

### UI Controls (press Escape to toggle toolbar)
- **Fullscreen**: Toggle with toolbar button or F11
- **Start/Stop Stream**: Connect/disconnect the video stream
- **Scale**: Choose from Fit, 100%, 75%, 50%
- **Monitor Selector**: Switch between monitors on multi-monitor setups
- **Mouse Lock**: Lock the mouse cursor to the canvas (toggle with Ctrl+Alt+M)
- **Logout**: End the session

### Multi-Monitor Support
- Server enumerates all connected monitors
- Client shows a dropdown to select which monitor to view
- Switching monitors reconnects the stream

### Pointer Lock (Mouse Lock-In)
- Press **Ctrl+Alt+M** or click the toolbar button to lock/unlock the mouse
- When locked, the mouse cursor is confined to the remote desktop area
- A visual indicator shows when pointer lock is active

### Remote Cursor
- Server captures the cursor position from the remote machine
- A cursor overlay is rendered in the browser, tracking the remote cursor
- Works even when the local cursor is hidden

## Apache Reverse Proxy

Example config to expose the server via HTTPS:

```apache
<VirtualHost *:443>
    ServerName remote.example.com

    SSLEngine on
    SSLCertificateFile    /path/to/fullchain.pem
    SSLCertificateKeyFile /path/to/privkey.pem

    # Enable WebSocket proxying
    RewriteEngine On
    RewriteCond %{HTTP:Upgrade} websocket [NC]
    RewriteCond %{HTTP:Connection} upgrade [NC]
    RewriteRule ^/ws$ ws://127.0.0.1:9090/ws [P,L]

    ProxyPreserveHost On
    ProxyPass        /ws ws://127.0.0.1:9090/ws
    ProxyPassReverse /ws ws://127.0.0.1:9090/ws

    ProxyPass        / http://127.0.0.1:9090/
    ProxyPassReverse / http://127.0.0.1:9090/

    # Low-latency headers
    Header set Cache-Control "no-store"
</VirtualHost>
```

Required Apache modules:

```bash
a2enmod proxy proxy_http proxy_wstunnel rewrite ssl headers
```

## Project Structure

```
├── protocol/          Shared binary protocol (Rust crate)
│   └── src/lib.rs
├── server/            Desktop capture + encoding server (Windows)
│   ├── static_auth/       Login page HTML (embedded at compile time)
│   └── src/
│       ├── main.rs        CLI entry point
│       ├── server.rs      HTTP + WebSocket (axum) + auth middleware
│       ├── config.rs      TOML configuration loader
│       ├── auth.rs        Session-based authentication
│       ├── capture.rs     Screen capture (scrap / DXGI) + multi-monitor
│       ├── cursor.rs      Cursor position capture (Win32 GetCursorInfo)
│       ├── encoder.rs     FFmpeg AMF pipeline + H.264 parser
│       └── input.rs       Mouse / keyboard simulation (SendInput)
├── client/            WASM protocol module + web UI
│   ├── src/lib.rs         Protocol encode/decode + latency tracker
│   └── web/
│       ├── index.html     Main UI with toolbar, cursor overlay
│       ├── style.css      Styles for toolbar, cursor, lock indicator
│       └── js/
│           ├── main.js       App bootstrap, WebSocket, toolbar controls
│           ├── decoder.js    WebCodecs H.264 decoder (Annex-B mode)
│           ├── renderer.js   Canvas renderer (newest-frame policy)
│           └── input.js      Keyboard/mouse → VK code + pointer lock
├── config.toml.example    Sample configuration file
├── build.ps1              Windows build script (PowerShell)
├── build.sh               Linux/macOS build script (CPU fallback)
└── readme.md
```

## Development (Linux / CPU fallback)

For testing on Linux without an AMD GPU, use the `libx264` software encoder:

```bash
chmod +x build.sh
./build.sh
cd server
./target/debug/wasm-remote-server --encoder libx264
```

## Tuning Tips

- **Lower QP** (e.g. `--quality 15`) = sharper image, higher bandwidth
- **Higher QP** (e.g. `--quality 28`) = lower bandwidth, more compression artifacts
- **Reduce FPS** (e.g. `--fps 30`) to halve bandwidth for non-gaming use
- For gaming, keep `--fps 60` and QP around 18–22
