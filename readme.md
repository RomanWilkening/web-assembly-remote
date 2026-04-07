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

### 2. Run the server

```powershell
cd server
.\target\debug\wasm-remote-server.exe --encoder h264_amf --fps 60 --quality 20
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

### 3. Open in browser

Navigate to `http://localhost:9090`.  Click the canvas to focus it for keyboard input.

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
│   └── src/
│       ├── main.rs        CLI entry point
│       ├── server.rs      HTTP + WebSocket (axum)
│       ├── capture.rs     Screen capture (scrap / DXGI)
│       ├── encoder.rs     FFmpeg AMF pipeline + H.264 parser
│       └── input.rs       Mouse / keyboard simulation (SendInput)
├── client/            WASM protocol module + web UI
│   ├── src/lib.rs         Protocol encode/decode + latency tracker
│   └── web/
│       ├── index.html
│       ├── style.css
│       └── js/
│           ├── main.js       App bootstrap, WebSocket orchestration
│           ├── decoder.js    WebCodecs H.264 decoder (Annex-B → AVC)
│           ├── renderer.js   Canvas renderer (newest-frame policy)
│           └── input.js      Keyboard/mouse → VK code mapping
├── build.ps1          Windows build script (PowerShell)
├── build.sh           Linux/macOS build script (CPU fallback)
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
