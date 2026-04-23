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
| Capture | DXGI Desktop Duplication (GPU-resident, ~1 ms); 500 µs polling sleep when idle |
| Encoding | AMF / NVENC / x264 / x265 / SVT-AV1 with `ultralowlatency` / `zerolatency` presets, no B-frames |
| Rate control | CQP/CRF by default (constant quality) or CBR with a 1-frame VBV buffer (`--bitrate-kbps`) |
| Slicing | Optional intra-frame slicing (`--slices N`) lets the decoder start work before the whole frame has arrived |
| Framing | Minimal binary protocol (10-byte header per frame) |
| Transport | WebSocket binary, TCP_NODELAY, send-side newest-wins frame coalescing (delta frames are dropped under congestion; key frames never are) |
| Decoding | WebCodecs `optimizeForLatency: true` (browser hardware decoder) for H.264 / HEVC / AV1 |
| Rendering | WebGL2 zero-copy `texImage2D(VideoFrame)` upload, `desynchronized` canvas, immediate paint (no rAF gate) by default |
| Input | Throttled at ~125 Hz, sent immediately over WebSocket |

## Quality Knobs

| Layer | Technique |
|-------|-----------|
| Codec | H.264 (universal), HEVC (better compression at the same QP), AV1 (best compression but newer HW only) |
| Chroma | 4:2:0 (default, universal HW decode) or 4:4:4 (`--chroma 444`, sharper text/UI; HW-decoder support varies) |
| Colour | BT.709 + full PC range tags emitted to FFmpeg for software encoders (`libx264` / `libx265` / `libsvtav1`). HW encoders (AMF / NVENC / QSV / VAAPI) write their own SPS VUI fields and the global `-color_*` flags are deliberately omitted to avoid an AMF-specific reconfigure stall observed on Windows 11 with AMD drivers. |

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
| `--quality` | `20` | Codec QP/CRF value (lower = better quality, higher bitrate). Ignored when `--bitrate-kbps` is set. |
| `--encoder` | `h264_amf` | FFmpeg encoder name (`h264_amf` / `hevc_amf` / `av1_amf` for AMD, `h264_nvenc` / `hevc_nvenc` / `av1_nvenc` for NVIDIA, `libx264` / `libx265` / `libsvtav1` for CPU fallback). |
| `--codec` | auto | Override codec family for splitting + browser decoder (`h264`, `hevc`, `av1`). Auto-detected from `--encoder` when omitted. |
| `--chroma` | `420` | Chroma sub-sampling: `420` (universal HW decode, no explicit pixel-format conversion — FFmpeg auto-negotiates the BGRA → encoder-native path) or `444` (sharper text via `-pix_fmt yuv444p`, less HW decoder support). |
| `--slices` | `1` | Number of slices per encoded frame. Higher values reduce decode latency at the cost of slightly worse compression. Honoured by H.264/HEVC encoders; ignored by AV1. The flag is only forwarded to FFmpeg when N > 1, so the default keeps the historical command line that older `h264_amf` builds were tuned against. |
| `--bitrate-kbps` | _unset_ | Switch from constant-quality (CQP/CRF) to CBR with a 1-frame VBV buffer. Useful on bandwidth-limited links where stable glass-to-glass latency matters more than constant visual quality. |
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
- **Switch to HEVC or AV1** (`--encoder hevc_amf` / `--encoder av1_amf`) for ~30–50 % less bandwidth at the same visual quality, provided the browser has HW decoder support
- **`--chroma 444`** preserves full chroma resolution — visibly sharper text/UI on subpixel-rendered fonts, but only available with codecs/profiles your decoder supports
- **`--slices 4`** can shave a frame of decode latency at 4K by letting the decoder start work on the first slice before the rest of the frame arrives
- **`--bitrate-kbps 25000`** switches the encoder to CBR with a tight 1-frame VBV buffer — preferable on links where bandwidth is the bottleneck, because constant quality (CQP) can produce multi-MB key frames that take a full RTT to deliver
