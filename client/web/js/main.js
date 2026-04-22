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
import { AudioPlayer }   from './audio.js';

// Message-type constants (must match protocol crate).
const MSG_VIDEO_FRAME        = 0x01;
const MSG_SERVER_INFO        = 0x02;
const MSG_CURSOR_INFO        = 0x03;
const MSG_MONITOR_LIST       = 0x04;
const MSG_AUDIO_DATA         = 0x05;
const MSG_AUDIO_DEVICE_LIST  = 0x06;
const MSG_PONG               = 0x07;

// ---------------------------------------------------------------------------
// WASM loading with explicit fetch, timeout, and retry.
//
// SSL-inspecting proxies (e.g. Netskope) can cause the initial .wasm fetch
// to hang after a login redirect.  We work around this by:
//   1. Fetching the .wasm binary ourselves with `cache: 'no-cache'` so the
//      proxy cannot serve a stale / incomplete cached response.
//   2. Wrapping the fetch in an AbortController timeout.
//   3. Retrying once on failure before giving up.
// ---------------------------------------------------------------------------

/**
 * Load the WASM module with an explicit fetch and timeout.
 * @param {number} timeoutMs  Maximum time to wait for the .wasm download.
 * @returns {Promise<object>} The initialised wasm-bindgen module.
 */
async function loadWasm(timeoutMs = 15000) {
  const wasmModule = await import('../pkg/wasm_remote_client.js');

  // Build the URL to the .wasm binary relative to the JS glue module.
  const wasmUrl = new URL('../pkg/wasm_remote_client_bg.wasm', import.meta.url);

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);

  try {
    const response = await fetch(wasmUrl, {
      credentials: 'same-origin',
      signal: controller.signal,
      // Bypass potentially stale proxy cache after login redirect.
      cache: 'no-cache',
    });
    clearTimeout(timer);
    // Pass the Response directly to wasm-bindgen init() so it does not
    // issue its own (potentially cacheable) fetch.  Newer wasm-bindgen
    // versions require an options object instead of a positional arg
    // (positional usage logs a deprecation warning and may be removed).
    await wasmModule.default({ module_or_path: response });
  } catch (e) {
    clearTimeout(timer);
    throw e;
  }

  return wasmModule;
}

/**
 * Attempt to load the WASM module, retrying on failure.
 * @param {HTMLElement} statusEl  Status element for user feedback.
 * @param {number} maxAttempts    How many times to try (default 2).
 * @returns {Promise<object>}
 */
async function loadWasmWithRetry(statusEl, maxAttempts = 2) {
  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      return await loadWasm();
    } catch (e) {
      console.warn(`WASM load attempt ${attempt}/${maxAttempts} failed:`, e);
      if (attempt < maxAttempts) {
        statusEl.textContent = 'Retrying WASM load…';
        // Brief pause before retrying.
        await new Promise((r) => setTimeout(r, 500));
      } else {
        throw e;
      }
    }
  }
}

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
  const toggleLatency   = document.getElementById('toggle-latency');
  const audioSelect     = document.getElementById('audio-select');
  const layoutSelect    = document.getElementById('layout-select');
  const btnMute         = document.getElementById('btn-mute');
  const btnLogout       = document.getElementById('btn-logout');
  const remoteCursor    = document.getElementById('remote-cursor');
  const lockIndicator   = document.getElementById('lock-indicator');

  let toolbarVisible = false;
  let streaming = true;
  let pointerLocked = false;
  let remoteW = 0;
  let remoteH = 0;
  /** Last known remote cursor position (cached so we can re-show it after
   *  the browser tab regains visibility, when no new MSG_CURSOR_INFO arrives
   *  until the cursor moves on the remote machine). */
  let lastCursorX = 0;
  let lastCursorY = 0;
  let lastCursorVisible = false;
  /** Latency display visibility – off by default. */
  let latencyVisible = false;
  /** Currently active monitor index (tracks what the server is capturing). */
  let currentMonitorIndex = 0;
  /** Currently selected remote keyboard-layout KLID (default: de-DE). */
  const LAYOUT_STORAGE_KEY = 'remote_keyboard_layout';
  let currentLayoutKlid = 0x00000407;
  try {
    const stored = localStorage.getItem(LAYOUT_STORAGE_KEY);
    if (stored) {
      const v = parseInt(stored, 16);
      if (!isNaN(v)) currentLayoutKlid = v >>> 0;
    }
  } catch (_) { /* localStorage may be unavailable */ }
  // Reflect the selection in the dropdown.
  const wantedLayoutValue = '0x' + currentLayoutKlid.toString(16).padStart(8, '0').toUpperCase();
  const layoutOptions = [...layoutSelect.options];
  const matchingLayoutOption = layoutOptions.find(
    (o) => o.value.toUpperCase() === wantedLayoutValue,
  );
  if (matchingLayoutOption) {
    layoutSelect.value = matchingLayoutOption.value;
  }
  /** @type {WebSocket|null} */
  let ws = null;

  // ── Stall detection ─────────────────────────────────────────
  // SSL-inspecting proxies (e.g. Netskope) may buffer WebSocket data
  // causing frames to stop arriving.  If no video frame arrives for
  // STALL_TIMEOUT_MS while the socket is connected, we reconnect.
  const STALL_TIMEOUT_MS = 5000;
  let lastFrameTime = 0;
  let stallTimerId = 0;

  /** Send binary data over the WebSocket (if connected). */
  const send = (data) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(data);
    }
  };

  /** Reset the stall timer – call whenever a video frame is received. */
  function resetStallTimer() {
    lastFrameTime = Date.now();
    clearTimeout(stallTimerId);
    if (streaming && ws && ws.readyState === WebSocket.OPEN) {
      stallTimerId = setTimeout(onStall, STALL_TIMEOUT_MS);
    }
  }

  /** Called when no video frame arrives within STALL_TIMEOUT_MS. */
  function onStall() {
    if (!streaming || !ws) return;
    console.warn(
      `No video frame received for ${STALL_TIMEOUT_MS} ms – reconnecting (proxy stall workaround)`
    );
    // Tear down and reconnect.
    if (ws) {
      ws.close();
      ws = null;
    }
    decoder.close();
    renderer.stop();
    connect();
  }

  // ── 1. Load WASM ─────────────────────────────────────────────
  statusEl.textContent = 'Loading WASM module…';

  // Verify that the session is still valid before doing anything.
  // The token is stored in sessionStorage by the login page and sent via
  // the Authorization header – no cookies required.
  const sessionToken = sessionStorage.getItem('session_token');
  if (!sessionToken) {
    window.location.href = '/login';
    return;
  }

  try {
    const sessionRes = await fetch('/api/session', {
      headers: { 'Authorization': 'Bearer ' + sessionToken },
    });
    if (!sessionRes.ok) {
      sessionStorage.removeItem('session_token');
      window.location.href = '/login';
      return;
    }
  } catch (e) {
    sessionStorage.removeItem('session_token');
    window.location.href = '/login';
    return;
  }

  let wasm;
  try {
    wasm = await loadWasmWithRetry(statusEl);
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

  // ── 3. Set up decoder + renderer + audio ──────────────────────
  const renderer = new Renderer(canvas);
  const decoder  = new H264Decoder((frame) => renderer.drawFrame(frame));
  const latencyTracker = new wasm.LatencyTracker(120);
  const audioPlayer = new AudioPlayer();

  // Initialise audio eagerly (loads the AudioWorklet processor).
  audioPlayer.init().catch((e) => console.warn('Audio init deferred:', e));

  /** @type {InputHandler|null} */
  let inputHandler = null;

  // Periodic latency display update (only when the user has enabled it).
  setInterval(() => {
    if (!latencyVisible) return;
    if (latencyTracker.count() > 0) {
      const avg = latencyTracker.average_ms().toFixed(1);
      const min = latencyTracker.min_ms().toFixed(1);
      const max = latencyTracker.max_ms().toFixed(1);
      latencyEl.textContent = `Latency: ${avg} ms  (min ${min} / max ${max})`;
    }
  }, 500);

  // Periodic Ping → Pong RTT measurement.  Computing RTT entirely on
  // the client clock avoids any dependency on the server and browser
  // system clocks being NTP-synchronised; the previously used
  // (now - server_ts) calculation reported the clock skew between the
  // two machines (often hundreds of milliseconds) on top of the real
  // latency.  We send a ping every second and report RTT/2 as the
  /** Wall-clock microseconds, monotonic enough for RTT measurement. */
  const nowMicros = () => (performance.timeOrigin + performance.now()) * 1000;
  // approximate one-way latency.
  setInterval(() => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const nowUs = nowMicros();
    try {
      send(wasm.encode_ping(nowUs));
    } catch (e) {
      // wasm not loaded yet or send failed – ignore.
    }
  }, 1000);

  // Latency display toggle (off by default).
  toggleLatency.checked = latencyVisible;
  toggleLatency.addEventListener('change', () => {
    latencyVisible = toggleLatency.checked;
    latencyEl.classList.toggle('hidden', !latencyVisible);
    if (!latencyVisible) {
      latencyEl.textContent = '';
    }
  });

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
    // Reposition the remote cursor for the new layout.
    updateRemoteCursor(lastCursorX, lastCursorY, lastCursorVisible);
  });

  function applyScale(mode) {
    if (mode === 'fit') {
      // Stretch to fill the viewport (may distort aspect ratio).
      canvas.style.width = '100vw';
      canvas.style.height = '100vh';
    } else if (mode === 'fit-aspect') {
      // Fit inside the viewport while preserving the original aspect ratio.
      if (remoteW > 0 && remoteH > 0) {
        const vw = window.innerWidth;
        const vh = window.innerHeight;
        const scale = Math.min(vw / remoteW, vh / remoteH);
        canvas.style.width  = `${Math.floor(remoteW * scale)}px`;
        canvas.style.height = `${Math.floor(remoteH * scale)}px`;
      } else {
        canvas.style.width = '100vw';
        canvas.style.height = '100vh';
      }
    } else {
      const pct = parseInt(mode) / 100;
      canvas.style.width  = `${remoteW * pct}px`;
      canvas.style.height = `${remoteH * pct}px`;
    }
  }

  // Re-apply the current scale when the viewport size changes so that
  // the "Fit (keep aspect)" mode adapts to the new window dimensions
  // and the cursor overlay stays aligned with the canvas.
  window.addEventListener('resize', () => {
    applyScale(scaleSelect.value);
    updateRemoteCursor(lastCursorX, lastCursorY, lastCursorVisible);
  });

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
    clearTimeout(stallTimerId);
    if (ws) {
      ws.close();
      ws = null;
    }
    decoder.close();
    renderer.stop();
    audioPlayer.close();
    statusEl.textContent = 'Stream stopped';
    btnStream.textContent = '▶ Start';
  }

  function startStream(monitorIndex) {
    streaming = true;
    btnStream.textContent = '⏹ Stop';
    if (monitorIndex !== undefined) {
      currentMonitorIndex = monitorIndex;
    }
    // Re-initialise audio player for the new connection.
    audioPlayer.init().catch((e) => console.warn('Audio re-init deferred:', e));
    connect();
  }

  // ── 8b. Mute / unmute toggle ────────────────────────────────
  btnMute.addEventListener('click', () => {
    const nowMuted = audioPlayer.muted;
    audioPlayer.setMuted(!nowMuted);
    btnMute.textContent = nowMuted ? '🔊 Mute' : '🔇 Unmute';
  });

  // ── 8c. Audio device selector ───────────────────────────────
  audioSelect.addEventListener('change', () => {
    const val = audioSelect.value;
    if (val === 'off') {
      // Send 0xFF to disable audio.
      send(wasm.encode_select_audio(0xFF));
      audioPlayer.setMuted(true);
      btnMute.textContent = '🔇 Unmute';
    } else {
      const idx = parseInt(val, 10);
      if (isNaN(idx)) return;
      send(wasm.encode_select_audio(idx));
      // Auto-unmute and re-initialise audio when a device is selected.
      audioPlayer.init().then(() => {
        audioPlayer.setMuted(false);
        btnMute.textContent = '🔊 Mute';
      }).catch((e) => console.warn('Audio init on device select:', e));
    }
  });

  // ── 9. Monitor selector ──────────────────────────────────────
  monitorSelect.addEventListener('change', () => {
    const idx = parseInt(monitorSelect.value);
    // Reconnect with new monitor selection.
    stopStream();
    setTimeout(() => startStream(idx), 100);
  });

  // ── 9b. Remote keyboard-layout selector ─────────────────────
  layoutSelect.addEventListener('change', () => {
    const v = parseInt(layoutSelect.value, 16);
    if (isNaN(v)) return;
    currentLayoutKlid = v >>> 0;
    try {
      localStorage.setItem(
        LAYOUT_STORAGE_KEY,
        '0x' + currentLayoutKlid.toString(16).padStart(8, '0').toUpperCase(),
      );
    } catch (_) { /* localStorage may be unavailable */ }
    send(wasm.encode_set_keyboard_layout(currentLayoutKlid));
  });

  // ── 10. Logout ───────────────────────────────────────────────
  btnLogout.addEventListener('click', () => {
    stopStream();
    // POST to /api/logout with Bearer token, then redirect.
    fetch('/api/logout', {
      method: 'POST',
      headers: { 'Authorization': 'Bearer ' + sessionToken },
    }).finally(() => {
      sessionStorage.removeItem('session_token');
      window.location.href = '/login';
    });
  });

  // ── 11. Remote cursor rendering ──────────────────────────────
  function updateRemoteCursor(cx, cy, visible) {
    // Cache the latest known position so we can re-display the cursor
    // after the tab regains focus, even if the remote machine has not
    // sent another MSG_CURSOR_INFO update yet.
    lastCursorX = cx;
    lastCursorY = cy;
    lastCursorVisible = visible;

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

  // When the user switches back to this tab, the remote may not send a
  // fresh CursorInfo until the cursor next moves – which leaves the
  // overlay hidden while the local cursor is also hidden over the canvas.
  // Re-apply the last known cursor position so the pointer reappears
  // immediately on tab focus.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      updateRemoteCursor(lastCursorX, lastCursorY, lastCursorVisible);
    }
  });

  // ── 12. WebSocket connect ────────────────────────────────────
  function connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    // The token is passed as a query parameter because the browser
    // WebSocket API does not support custom headers.  The connection
    // is over TLS (wss://) so the URL is encrypted in transit.
    // The URL is constructed in JavaScript (not browser navigation),
    // so it does not appear in browser history.
    const wsUrl = `${proto}://${location.host}/ws?token=${encodeURIComponent(sessionToken)}`;
    statusEl.textContent = 'Connecting…';

    ws = new WebSocket(wsUrl);
    ws.binaryType = 'arraybuffer';

    ws.addEventListener('open', () => {
      statusEl.textContent = 'Connected – waiting for first frame…';
      // Always tell the server which monitor to capture.
      send(wasm.encode_select_monitor(currentMonitorIndex));
      // Set the remote keyboard layout so scancode-forwarded keystrokes
      // are interpreted correctly (default de-DE; user-selectable).
      send(wasm.encode_set_keyboard_layout(currentLayoutKlid));
      // Start the stall detector – if no video frame arrives within the
      // timeout the connection will be recycled automatically.
      resetStallTimer();
    });

    ws.addEventListener('close', () => {
      clearTimeout(stallTimerId);
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

    // Reusable DataView over the current message – avoids constructing a
    // new view per packet just to read the small header fields.
    let dv = null;
    let dvBuffer = null;

    ws.addEventListener('message', (event) => {
      const data = new Uint8Array(event.data);
      if (data.length === 0) return;

      // Read the message-type byte directly in JS to avoid the
      // wasm-bindgen `&[u8]` copy of the entire payload (which for a
      // 4K key-frame can be hundreds of KB).
      const type_ = data[0];

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

          // Restore dropdown selection to the currently active monitor so the
          // UI stays in sync after a reconnect.  If the monitor is no longer
          // available (e.g. it was disconnected), fall back to the first entry.
          const wantedValue = currentMonitorIndex.toString();
          if ([...monitorSelect.options].some(o => o.value === wantedValue)) {
            monitorSelect.value = wantedValue;
          } else if (monitorSelect.options.length > 0) {
            monitorSelect.value = monitorSelect.options[0].value;
            currentMonitorIndex = parseInt(monitorSelect.value, 10);
          }
          break;
        }

        case MSG_AUDIO_DEVICE_LIST: {
          const count = wasm.audio_device_list_count(data);
          console.log(`AudioDeviceList: ${count} device(s)`);

          // Update audio device selector.
          audioSelect.innerHTML = '';

          // "Off" option to disable audio capture.
          const offOpt = document.createElement('option');
          offOpt.value = 'off';
          offOpt.textContent = 'Off';
          audioSelect.appendChild(offOpt);

          for (let i = 0; i < count; i++) {
            const idx  = wasm.audio_device_index(data, i);
            const name = wasm.audio_device_name(data, i);
            const opt  = document.createElement('option');
            opt.value  = idx.toString();
            opt.textContent = name;
            audioSelect.appendChild(opt);
          }
          break;
        }

        case MSG_SERVER_INFO: {
          // ServerInfo layout: [type u8][width u16 LE][height u16 LE][fps u8]
          if (data.length < 6) break;
          if (dvBuffer !== data.buffer) {
            dv = new DataView(data.buffer);
            dvBuffer = data.buffer;
          }
          const w   = dv.getUint16(data.byteOffset + 1, true);
          const h   = dv.getUint16(data.byteOffset + 3, true);
          const fps = data[5];
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
          // Hot-path: parse the 10-byte header directly in JS so the
          // multi-MB H.264 payload is never copied into wasm linear
          // memory just to read a timestamp + 1-byte flag.
          // Layout: [type u8][timestamp_us u64 LE][is_keyframe u8][h264 …]
          if (data.length < 10) break;
          if (dvBuffer !== data.buffer) {
            dv = new DataView(data.buffer);
            dvBuffer = data.buffer;
          }
          // u64 little-endian → number (microseconds fit in 53 bits for
          // any realistic Unix-time value, so the f64 conversion is
          // lossless in practice).
          const tsUs   = Number(dv.getBigUint64(data.byteOffset + 1, true));
          const isKey  = data[9] !== 0;
          const h264Data = data.subarray(10);

          // Reset the stall-detection timer on every received frame.
          resetStallTimer();

          // NOTE: Latency is measured separately via Ping/Pong (see the
          // periodic pinger above and the MSG_PONG handler below) so it
          // doesn't depend on server/client wall-clock synchronisation.

          decoder.decode(h264Data, isKey, tsUs);
          break;
        }

        case MSG_PONG: {
          // RTT measurement: the server echoes back the timestamp we
          // sent in the matching Ping.  Compute the round-trip on the
          // client clock alone and report half of it as the one-way
          // latency estimate.
          // Pong layout: [type u8][client_ts_us u64 LE]; parse in JS to
          // skip the wasm-bindgen copy for this small but frequent msg.
          if (data.length < 9) break;
          if (dvBuffer !== data.buffer) {
            dv = new DataView(data.buffer);
            dvBuffer = data.buffer;
          }
          const sentUs = Number(dv.getBigUint64(data.byteOffset + 1, true));
          const nowUs  = nowMicros();
          const rttMs  = (nowUs - sentUs) / 1000;
          if (rttMs >= 0 && rttMs < 60000) {
            const oneWayMs = rttMs / 2;
            latencyTracker.record(oneWayMs);
            // Feed the latency back to the decoder so it can adapt its
            // queue-drop threshold (deeper queue on LAN, shallower on
            // slow links).  See `H264Decoder._dropThreshold`.
            decoder.setLatencyMs(oneWayMs);
          }
          break;
        }

        case MSG_CURSOR_INFO: {
          // CursorInfo layout: [type u8][x u16 LE][y u16 LE][visible u8]
          if (data.length < 6) break;
          if (dvBuffer !== data.buffer) {
            dv = new DataView(data.buffer);
            dvBuffer = data.buffer;
          }
          const cx = dv.getUint16(data.byteOffset + 1, true);
          const cy = dv.getUint16(data.byteOffset + 3, true);
          const cv = data[5] !== 0;
          updateRemoteCursor(cx, cy, cv);
          break;
        }

        case MSG_AUDIO_DATA: {
          // Skip the 1-byte message type; rest is raw f32le PCM.
          const pcmBytes = data.subarray(1);
          audioPlayer.feed(pcmBytes);
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
