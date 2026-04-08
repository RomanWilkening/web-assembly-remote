/**
 * Application entry-point.
 *
 * 1. Loads the WASM module.
 * 2. Opens a WebSocket to the server.
 * 3. Wires up the H.264 decoder, canvas renderer, and input handler.
 * 4. Controls: fullscreen, pointer lock, monitor select, start/stop, logout.
 */

import { H264Decoder } from './decoder.js';
import { Renderer }     from './renderer.js';
import { InputHandler }  from './input.js';

// Message-type constants (must match protocol crate).
const MSG_VIDEO_FRAME  = 0x01;
const MSG_SERVER_INFO  = 0x02;
const MSG_CURSOR_INFO  = 0x03;
const MSG_MONITOR_LIST = 0x04;

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

async function main() {
  const statusEl        = document.getElementById('status');
  const latencyEl       = document.getElementById('latency');
  const canvas          = document.getElementById('screen');
  const toolbar         = document.getElementById('toolbar');
  const monitorSelect   = document.getElementById('monitor-select');
  const scaleSelect     = document.getElementById('scale-select');
  const btnFullscreen   = document.getElementById('btn-fullscreen');
  const btnPointerLock  = document.getElementById('btn-pointerlock');
  const btnStream       = document.getElementById('btn-stream');
  const btnLogout       = document.getElementById('btn-logout');
  const remoteCursor    = document.getElementById('remote-cursor');
  const lockIndicator   = document.getElementById('lock-indicator');

  let toolbarVisible = false;
  let streaming = true;
  let pointerLocked = false;
  let remoteW = 0;
  let remoteH = 0;
  /** @type {WebSocket|null} */
  let ws = null;

  // ── 1. Load WASM ─────────────────────────────────────────────
  statusEl.textContent = 'Loading WASM module…';

  // Verify that the session is still valid before doing anything.
  // When a proxy strips the session cookie from sub-resource requests the
  // module scripts load fine (they are served without auth), but the
  // WebSocket will be rejected with 401.  Checking here gives a clean
  // redirect to /login rather than a cryptic "Connection error".
  try {
    const sessionRes = await fetch('/api/session', { credentials: 'same-origin' });
    if (!sessionRes.ok) {
      window.location.href = '/login';
      return;
    }
  } catch (e) {
    window.location.href = '/login';
    return;
  }

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

  // ── 3. Set up decoder + renderer ─────────────────────────────
  const renderer = new Renderer(canvas);
  const decoder  = new H264Decoder((frame) => renderer.drawFrame(frame));
  const latencyTracker = new wasm.LatencyTracker(120);

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

  // ── 4. Toolbar toggle with Escape ────────────────────────────
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      toolbarVisible = !toolbarVisible;
      toolbar.classList.toggle('hidden', !toolbarVisible);
      e.preventDefault();
    }
  });

  // ── 5. Fullscreen toggle ─────────────────────────────────────
  btnFullscreen.addEventListener('click', () => {
    if (!document.fullscreenElement) {
      document.documentElement.requestFullscreen().catch(() => {});
    } else {
      document.exitFullscreen().catch(() => {});
    }
  });

  document.addEventListener('fullscreenchange', () => {
    btnFullscreen.textContent = document.fullscreenElement
      ? '⛶ Exit Fullscreen'
      : '⛶ Fullscreen';
  });

  // F11 fullscreen shortcut (handled by toolbar or browser)
  document.addEventListener('keydown', (e) => {
    if (e.key === 'F11') {
      e.preventDefault();
      btnFullscreen.click();
    }
  });

  // ── 6. Pointer Lock (Mouse Lock-In) ──────────────────────────
  function togglePointerLock() {
    if (document.pointerLockElement === canvas) {
      document.exitPointerLock();
    } else {
      canvas.requestPointerLock();
    }
  }

  btnPointerLock.addEventListener('click', togglePointerLock);

  // Ctrl+Alt+M shortcut for pointer lock toggle
  document.addEventListener('keydown', (e) => {
    if (e.ctrlKey && e.altKey && e.code === 'KeyM') {
      e.preventDefault();
      togglePointerLock();
    }
  });

  document.addEventListener('pointerlockchange', () => {
    pointerLocked = document.pointerLockElement === canvas;
    lockIndicator.classList.toggle('hidden', !pointerLocked);
    btnPointerLock.textContent = pointerLocked
      ? '🔓 Unlock Mouse'
      : '🔓 Lock Mouse';

    // Update input handler with pointer lock state
    if (inputHandler) {
      inputHandler.setPointerLocked(pointerLocked);
    }
  });

  // ── 7. Scale selector ────────────────────────────────────────
  scaleSelect.addEventListener('change', () => {
    applyScale(scaleSelect.value);
  });

  function applyScale(mode) {
    if (mode === 'fit') {
      canvas.style.width = '100vw';
      canvas.style.height = '100vh';
    } else {
      const pct = parseInt(mode) / 100;
      canvas.style.width  = `${remoteW * pct}px`;
      canvas.style.height = `${remoteH * pct}px`;
    }
  }

  // ── 8. Stream start/stop ─────────────────────────────────────
  btnStream.addEventListener('click', () => {
    if (streaming) {
      stopStream();
    } else {
      startStream();
    }
  });

  function stopStream() {
    streaming = false;
    if (ws) {
      ws.close();
      ws = null;
    }
    decoder.close();
    renderer.stop();
    statusEl.textContent = 'Stream stopped';
    btnStream.textContent = '▶ Start';
  }

  function startStream(monitorIndex) {
    streaming = true;
    btnStream.textContent = '⏹ Stop';
    connect(monitorIndex);
  }

  // ── 9. Monitor selector ──────────────────────────────────────
  monitorSelect.addEventListener('change', () => {
    const idx = parseInt(monitorSelect.value);
    // Reconnect with new monitor selection.
    stopStream();
    setTimeout(() => startStream(idx), 100);
  });

  // ── 10. Logout ───────────────────────────────────────────────
  btnLogout.addEventListener('click', () => {
    stopStream();
    // POST to /api/logout then redirect
    fetch('/api/logout', { method: 'POST' }).finally(() => {
      window.location.href = '/login';
    });
  });

  // ── 11. Remote cursor rendering ──────────────────────────────
  function updateRemoteCursor(cx, cy, visible) {
    if (!visible || remoteW === 0 || remoteH === 0) {
      remoteCursor.classList.add('hidden');
      return;
    }
    remoteCursor.classList.remove('hidden');

    // Convert remote coordinates to viewport position
    const rect = canvas.getBoundingClientRect();
    const sx = rect.width / remoteW;
    const sy = rect.height / remoteH;
    const px = rect.left + cx * sx;
    const py = rect.top + cy * sy;

    remoteCursor.style.transform = `translate(${px}px, ${py}px)`;
  }

  // ── 12. WebSocket connect ────────────────────────────────────
  function connect(monitorIndex) {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const wsUrl = `${proto}://${location.host}/ws`;
    statusEl.textContent = `Connecting to ${wsUrl}…`;

    ws = new WebSocket(wsUrl);
    ws.binaryType = 'arraybuffer';

    const send = (data) => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(data);
      }
    };

    ws.addEventListener('open', () => {
      statusEl.textContent = 'Connected – waiting for first frame…';
      // If a specific monitor was requested, send SelectMonitor
      // Otherwise, send ClientReady.
      if (monitorIndex !== undefined) {
        send(wasm.encode_select_monitor(monitorIndex));
      } else {
        send(wasm.encode_client_ready());
      }
    });

    ws.addEventListener('close', () => {
      if (streaming) {
        statusEl.textContent = 'Disconnected';
        decoder.close();
        renderer.stop();
      }
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
        case MSG_MONITOR_LIST: {
          const count = wasm.monitor_list_count(data);
          console.log(`MonitorList: ${count} monitor(s)`);

          // Update monitor selector
          monitorSelect.innerHTML = '';
          for (let i = 0; i < count; i++) {
            const idx    = wasm.monitor_info_index(data, i);
            const mw     = wasm.monitor_info_width(data, i);
            const mh     = wasm.monitor_info_height(data, i);
            const prim   = wasm.monitor_info_primary(data, i);
            const opt    = document.createElement('option');
            opt.value    = idx.toString();
            opt.textContent = `Monitor ${idx}${prim ? ' (Primary)' : ''} – ${mw}×${mh}`;
            monitorSelect.appendChild(opt);
          }
          break;
        }

        case MSG_SERVER_INFO: {
          const w   = wasm.server_info_width(data);
          const h   = wasm.server_info_height(data);
          const fps = wasm.server_info_fps(data);
          console.log(`ServerInfo: ${w}×${h} @ ${fps} fps`);
          remoteW = w;
          remoteH = h;
          renderer.resize(w, h);
          decoder.setRemoteSize(w, h);

          if (!inputHandler) {
            inputHandler = new InputHandler(canvas, wasm, send, w, h);
          } else {
            inputHandler.setRemoteSize(w, h);
          }
          inputHandler.setPointerLocked(pointerLocked);

          statusEl.textContent = `${w}×${h} @ ${fps} fps`;
          applyScale(scaleSelect.value);
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

        case MSG_CURSOR_INFO: {
          const cx = wasm.cursor_info_x(data);
          const cy = wasm.cursor_info_y(data);
          const cv = wasm.cursor_info_visible(data);
          updateRemoteCursor(cx, cy, cv);
          break;
        }

        default:
          console.warn('Unknown message type:', type_);
      }
    });
  }

  // ── 13. Initial connection ───────────────────────────────────
  connect();
}

main().catch(console.error);
