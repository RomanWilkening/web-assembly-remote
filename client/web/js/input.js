/**
 * Captures mouse and keyboard events on the canvas and converts
 * them to binary protocol messages via the WASM module.
 *
 * All coordinates are normalised to the remote desktop resolution
 * (not the CSS layout size).
 */

// --------------------------------------------------------------------------
// JavaScript key-code → Windows Virtual-Key code mapping
// --------------------------------------------------------------------------

const VK_MAP = {
  // Letters
  KeyA: 0x41, KeyB: 0x42, KeyC: 0x43, KeyD: 0x44, KeyE: 0x45,
  KeyF: 0x46, KeyG: 0x47, KeyH: 0x48, KeyI: 0x49, KeyJ: 0x4a,
  KeyK: 0x4b, KeyL: 0x4c, KeyM: 0x4d, KeyN: 0x4e, KeyO: 0x4f,
  KeyP: 0x50, KeyQ: 0x51, KeyR: 0x52, KeyS: 0x53, KeyT: 0x54,
  KeyU: 0x55, KeyV: 0x56, KeyW: 0x57, KeyX: 0x58, KeyY: 0x59,
  KeyZ: 0x5a,
  // Digits
  Digit0: 0x30, Digit1: 0x31, Digit2: 0x32, Digit3: 0x33, Digit4: 0x34,
  Digit5: 0x35, Digit6: 0x36, Digit7: 0x37, Digit8: 0x38, Digit9: 0x39,
  // F-keys
  F1: 0x70, F2: 0x71, F3: 0x72, F4: 0x73, F5: 0x74, F6: 0x75,
  F7: 0x76, F8: 0x77, F9: 0x78, F10: 0x79, F11: 0x7a, F12: 0x7b,
  // Modifiers
  ShiftLeft: 0xa0, ShiftRight: 0xa1,
  ControlLeft: 0xa2, ControlRight: 0xa3,
  AltLeft: 0xa4, AltRight: 0xa5,
  MetaLeft: 0x5b, MetaRight: 0x5c,
  // Whitespace / editing
  Space: 0x20, Enter: 0x0d, Backspace: 0x08, Tab: 0x09, Escape: 0x1b,
  Delete: 0x2e, Insert: 0x2d,
  // Navigation
  ArrowUp: 0x26, ArrowDown: 0x28, ArrowLeft: 0x25, ArrowRight: 0x27,
  Home: 0x24, End: 0x23, PageUp: 0x21, PageDown: 0x22,
  // Punctuation (US layout, approximate)
  Minus: 0xbd, Equal: 0xbb,
  BracketLeft: 0xdb, BracketRight: 0xdd,
  Semicolon: 0xba, Quote: 0xde,
  Backquote: 0xc0, Backslash: 0xdc,
  Comma: 0xbc, Period: 0xbe, Slash: 0xbf,
  CapsLock: 0x14, NumLock: 0x90, ScrollLock: 0x91,
  PrintScreen: 0x2c, Pause: 0x13,
  // Numpad
  Numpad0: 0x60, Numpad1: 0x61, Numpad2: 0x62, Numpad3: 0x63,
  Numpad4: 0x64, Numpad5: 0x65, Numpad6: 0x66, Numpad7: 0x67,
  Numpad8: 0x68, Numpad9: 0x69,
  NumpadMultiply: 0x6a, NumpadAdd: 0x6b, NumpadSubtract: 0x6d,
  NumpadDecimal: 0x6e, NumpadDivide: 0x6f, NumpadEnter: 0x0d,
};

export class InputHandler {
  /**
   * @param {HTMLCanvasElement} canvas
   * @param {object} wasm – the WASM module exports
   * @param {function(Uint8Array): void} send – function that sends binary data over WebSocket
   * @param {number} remoteWidth
   * @param {number} remoteHeight
   */
  constructor(canvas, wasm, send, remoteWidth, remoteHeight) {
    /** @private */ this._canvas = canvas;
    /** @private */ this._wasm = wasm;
    /** @private */ this._send = send;
    /** @private */ this._remoteW = remoteWidth;
    /** @private */ this._remoteH = remoteHeight;

    // Throttle mouse-move to at most once per frame (~16 ms).
    /** @private */ this._lastMoveTime = 0;
    /** @private */ this._moveThrottleMs = 8; // ~125 Hz max

    this._bindEvents();
  }

  /** Update remote desktop dimensions (e.g. on resolution change). */
  setRemoteSize(w, h) {
    this._remoteW = w;
    this._remoteH = h;
  }

  // ---------- internal ----------

  /** @private */
  _toRemote(clientX, clientY) {
    const rect = this._canvas.getBoundingClientRect();
    const sx = this._remoteW / rect.width;
    const sy = this._remoteH / rect.height;
    return {
      x: Math.round((clientX - rect.left) * sx),
      y: Math.round((clientY - rect.top) * sy),
    };
  }

  /** @private */
  _bindEvents() {
    const c = this._canvas;

    // ── Mouse ──
    c.addEventListener('mousemove', (e) => {
      const now = performance.now();
      if (now - this._lastMoveTime < this._moveThrottleMs) return;
      this._lastMoveTime = now;

      const { x, y } = this._toRemote(e.clientX, e.clientY);
      this._send(this._wasm.encode_mouse_move(x, y));
    });

    c.addEventListener('mousedown', (e) => {
      e.preventDefault();
      const { x, y } = this._toRemote(e.clientX, e.clientY);
      this._send(this._wasm.encode_mouse_button(e.button, true, x, y));
    });

    c.addEventListener('mouseup', (e) => {
      e.preventDefault();
      const { x, y } = this._toRemote(e.clientX, e.clientY);
      this._send(this._wasm.encode_mouse_button(e.button, false, x, y));
    });

    c.addEventListener('wheel', (e) => {
      e.preventDefault();
      const dx = Math.round(e.deltaX);
      const dy = Math.round(-e.deltaY); // invert to match Windows WHEEL_DELTA
      this._send(this._wasm.encode_mouse_scroll(dx, dy));
    }, { passive: false });

    c.addEventListener('contextmenu', (e) => e.preventDefault());

    // ── Keyboard ──
    c.addEventListener('keydown', (e) => {
      e.preventDefault();
      const vk = VK_MAP[e.code];
      if (vk !== undefined) {
        this._send(this._wasm.encode_key_event(vk, true));
      }
    });

    c.addEventListener('keyup', (e) => {
      e.preventDefault();
      const vk = VK_MAP[e.code];
      if (vk !== undefined) {
        this._send(this._wasm.encode_key_event(vk, false));
      }
    });
  }
}
