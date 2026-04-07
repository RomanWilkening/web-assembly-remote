/**
 * Application entry-point.
 *
 * 1. Loads the WASM module.
 * 2. Opens a WebSocket to the server.
 * 3. Wires up the H.264 decoder, canvas renderer, and input handler.
 */

import { H264Decoder } from './decoder.js';
import { Renderer }     from './renderer.js';
import { InputHandler }  from './input.js';

// Message-type constants (must match protocol crate).
const MSG_VIDEO_FRAME = 0x01;
const MSG_SERVER_INFO = 0x02;

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

async function main() {
  const statusEl  = document.getElementById('status');
  const latencyEl = document.getElementById('latency');
  const canvas    = document.getElementById('screen');

  // ── 1. Load WASM ─────────────────────────────────────────────
  statusEl.textContent = 'Loading WASM module…';

  let wasm;
  try {
    wasm = await import('../pkg/wasm_remote_client.js');
    await wasm.default();       // initialise the WASM instance
  } catch (e) {
    statusEl.textContent = `WASM load failed: ${e}`;
    console.error(e);
    return;
  }

  // ── 2. Check WebCodecs support ───────────────────────────────
  if (typeof VideoDecoder === 'undefined') {
    statusEl.textContent =
      'Your browser does not support WebCodecs (VideoDecoder). ' +
      'Please use a recent version of Chrome / Edge.';
    return;
  }

  // ── 3. Connect WebSocket ─────────────────────────────────────
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const wsUrl = `${proto}://${location.host}/ws`;
  statusEl.textContent = `Connecting to ${wsUrl}…`;

  const ws = new WebSocket(wsUrl);
  ws.binaryType = 'arraybuffer';

  // Utility: send binary data.
  const send = (data) => {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(data);
    }
  };

  // ── 4. Set up decoder + renderer ─────────────────────────────
  const renderer = new Renderer(canvas);
  const decoder  = new H264Decoder((frame) => renderer.drawFrame(frame));

  const latencyTracker = new wasm.LatencyTracker(120); // ~2 s window at 60 fps

  /** @type {InputHandler|null} */
  let inputHandler = null;

  // Periodic latency display update.
  setInterval(() => {
    if (latencyTracker.count() > 0) {
      const avg = latencyTracker.average_ms().toFixed(1);
      const min = latencyTracker.min_ms().toFixed(1);
      const max = latencyTracker.max_ms().toFixed(1);
      latencyEl.textContent = `Latency: ${avg} ms  (min ${min} / max ${max})`;
    }
  }, 500);

  // ── 5. WebSocket event handlers ──────────────────────────────

  ws.addEventListener('open', () => {
    statusEl.textContent = 'Connected – waiting for first frame…';
    send(wasm.encode_client_ready());
  });

  ws.addEventListener('close', () => {
    statusEl.textContent = 'Disconnected';
    decoder.close();
    renderer.stop();
  });

  ws.addEventListener('error', (e) => {
    statusEl.textContent = 'Connection error';
    console.error('WebSocket error:', e);
  });

  ws.addEventListener('message', (event) => {
    const data = new Uint8Array(event.data);
    if (data.length === 0) return;

    const type_ = wasm.message_type(data);

    switch (type_) {
      case MSG_SERVER_INFO: {
        const w   = wasm.server_info_width(data);
        const h   = wasm.server_info_height(data);
        const fps = wasm.server_info_fps(data);
        console.log(`ServerInfo: ${w}×${h} @ ${fps} fps`);
        renderer.resize(w, h);

        if (!inputHandler) {
          inputHandler = new InputHandler(canvas, wasm, send, w, h);
        } else {
          inputHandler.setRemoteSize(w, h);
        }

        statusEl.textContent = `${w}×${h} @ ${fps} fps`;
        // Focus canvas so keyboard events work.
        canvas.focus();
        break;
      }

      case MSG_VIDEO_FRAME: {
        const tsUs      = wasm.video_frame_timestamp(data);
        const isKey     = wasm.video_frame_is_keyframe(data);
        const offset    = wasm.video_frame_data_offset();
        const h264Data  = data.subarray(offset);

        // Measure one-way latency (approximate, requires synchronised clocks).
        const nowUs = performance.now() * 1000 + performance.timeOrigin * 1000;
        const latencyMs = (nowUs - tsUs) / 1000;
        if (latencyMs > 0 && latencyMs < 60000) {
          latencyTracker.record(latencyMs);
        }

        decoder.decode(h264Data, isKey, tsUs);
        break;
      }

      default:
        console.warn('Unknown message type:', type_);
    }
  });
}

main().catch(console.error);
