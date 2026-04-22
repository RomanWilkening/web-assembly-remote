/**
 * Captures mouse and keyboard events on the canvas and converts
 * them to binary protocol messages via the WASM module.
 *
 * All coordinates are normalised to the remote desktop resolution
 * (not the CSS layout size).
 */

// --------------------------------------------------------------------------
// KeyboardEvent.code → PS/2 Set 1 hardware scancode
// --------------------------------------------------------------------------
//
// We forward the *physical* key the user pressed ("Parsec method") and
// let the keyboard layout active on the remote interpret it. Each entry
// is `[scancode, extended]`, where `extended` corresponds to the 0xE0
// prefix that Windows expects via KEYEVENTF_EXTENDEDKEY.

const SCANCODE_MAP = {
  // Top row (digits)
  Escape: [0x01, false],
  Backquote: [0x29, false],
  Digit1: [0x02, false], Digit2: [0x03, false], Digit3: [0x04, false],
  Digit4: [0x05, false], Digit5: [0x06, false], Digit6: [0x07, false],
  Digit7: [0x08, false], Digit8: [0x09, false], Digit9: [0x0A, false],
  Digit0: [0x0B, false],
  Minus: [0x0C, false], Equal: [0x0D, false], Backspace: [0x0E, false],

  // QWERTY row
  Tab: [0x0F, false],
  KeyQ: [0x10, false], KeyW: [0x11, false], KeyE: [0x12, false],
  KeyR: [0x13, false], KeyT: [0x14, false], KeyY: [0x15, false],
  KeyU: [0x16, false], KeyI: [0x17, false], KeyO: [0x18, false],
  KeyP: [0x19, false],
  BracketLeft: [0x1A, false], BracketRight: [0x1B, false],
  Enter: [0x1C, false],

  // ASDF row
  ControlLeft: [0x1D, false],
  KeyA: [0x1E, false], KeyS: [0x1F, false], KeyD: [0x20, false],
  KeyF: [0x21, false], KeyG: [0x22, false], KeyH: [0x23, false],
  KeyJ: [0x24, false], KeyK: [0x25, false], KeyL: [0x26, false],
  Semicolon: [0x27, false], Quote: [0x28, false],
  Backslash: [0x2B, false],

  // ZXCV row
  ShiftLeft: [0x2A, false],
  // ISO key on de-DE / other ISO layouts: between LShift and Y/Z
  IntlBackslash: [0x56, false],
  KeyZ: [0x2C, false], KeyX: [0x2D, false], KeyC: [0x2E, false],
  KeyV: [0x2F, false], KeyB: [0x30, false], KeyN: [0x31, false],
  KeyM: [0x32, false],
  Comma: [0x33, false], Period: [0x34, false], Slash: [0x35, false],
  ShiftRight: [0x36, false],

  // Bottom row
  AltLeft: [0x38, false],
  Space: [0x39, false],
  CapsLock: [0x3A, false],

  // Function row
  F1: [0x3B, false], F2: [0x3C, false], F3: [0x3D, false], F4: [0x3E, false],
  F5: [0x3F, false], F6: [0x40, false], F7: [0x41, false], F8: [0x42, false],
  F9: [0x43, false], F10: [0x44, false], F11: [0x57, false], F12: [0x58, false],

  // Numpad
  NumLock: [0x45, false], ScrollLock: [0x46, false],
  Numpad7: [0x47, false], Numpad8: [0x48, false], Numpad9: [0x49, false],
  NumpadSubtract: [0x4A, false],
  Numpad4: [0x4B, false], Numpad5: [0x4C, false], Numpad6: [0x4D, false],
  NumpadAdd: [0x4E, false],
  Numpad1: [0x4F, false], Numpad2: [0x50, false], Numpad3: [0x51, false],
  Numpad0: [0x52, false], NumpadDecimal: [0x53, false],

  // Extended (E0-prefixed)
  ControlRight:  [0x1D, true],
  AltRight:      [0x38, true],   // AltGr on de-DE
  MetaLeft:      [0x5B, true],
  MetaRight:     [0x5C, true],
  ContextMenu:   [0x5D, true],
  NumpadEnter:   [0x1C, true],
  NumpadDivide:  [0x35, true],
  Insert:        [0x52, true],
  Delete:        [0x53, true],
  Home:          [0x47, true],
  End:           [0x4F, true],
  PageUp:        [0x49, true],
  PageDown:      [0x51, true],
  ArrowUp:       [0x48, true],
  ArrowLeft:     [0x4B, true],
  ArrowDown:     [0x50, true],
  ArrowRight:    [0x4D, true],
  // PrintScreen actually emits "E0 2A E0 37" on press; the bare 0x37
  // (with the extended bit) works for most apps and avoids the
  // multi-byte sequence handling.
  PrintScreen:   [0x37, true],
  // Pause has its own multi-byte sequence (E1 1D 45 …); a single
  // press of the bare scancode is a reasonable approximation here.
  Pause:         [0x45, false],
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

    // Coalesce mouse-move events to one send per display refresh
    // (`requestAnimationFrame`).  This naturally tracks the encoder's
    // FPS (so we don't burn bandwidth/CPU sending intermediate samples
    // the encoder will throw away) and gives 144 Hz mice the same
    // effective rate as the local display, instead of clipping to a
    // fixed 125 Hz.  The very latest event always wins; older events
    // in the same frame are simply replaced.
    /** @private @type {{x:number,y:number}|null} */
    this._pendingMove = null;
    /** @private */
    this._moveRafId = 0;

    // Pointer lock state
    /** @private */ this._pointerLocked = false;
    /** @private */ this._virtualX = remoteWidth / 2;
    /** @private */ this._virtualY = remoteHeight / 2;

    this._bindEvents();
  }

  /** Update remote desktop dimensions (e.g. on resolution change). */
  setRemoteSize(w, h) {
    this._remoteW = w;
    this._remoteH = h;
  }

  /** Set pointer lock state. */
  setPointerLocked(locked) {
    this._pointerLocked = locked;
    if (locked) {
      // Initialize virtual cursor to center of remote screen
      this._virtualX = this._remoteW / 2;
      this._virtualY = this._remoteH / 2;
    }
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
  _handlePointerLockMove(movementX, movementY) {
    // Apply movement to virtual cursor position
    const rect = this._canvas.getBoundingClientRect();
    const sx = this._remoteW / rect.width;
    const sy = this._remoteH / rect.height;

    this._virtualX = Math.max(0, Math.min(this._remoteW, this._virtualX + movementX * sx));
    this._virtualY = Math.max(0, Math.min(this._remoteH, this._virtualY + movementY * sy));

    return {
      x: Math.round(this._virtualX),
      y: Math.round(this._virtualY),
    };
  }

  /** @private */
  _bindEvents() {
    const c = this._canvas;

    // ── Mouse ──
    c.addEventListener('mousemove', (e) => {
      let x, y;
      if (this._pointerLocked) {
        const pos = this._handlePointerLockMove(e.movementX, e.movementY);
        x = pos.x;
        y = pos.y;
      } else {
        const pos = this._toRemote(e.clientX, e.clientY);
        x = pos.x;
        y = pos.y;
      }
      // Stash the most recent position; flush once per rAF.  When the
      // mouse moves multiple times within a single display frame the
      // newer position simply overwrites the previous one — those
      // intermediate samples are intentionally discarded because the
      // encoder can only show one frame per refresh anyway.
      this._pendingMove = { x, y };
      if (this._moveRafId === 0) {
        this._moveRafId = requestAnimationFrame(() => {
          this._moveRafId = 0;
          const m = this._pendingMove;
          if (m) {
            this._pendingMove = null;
            this._send(this._wasm.encode_mouse_move(m.x, m.y));
          }
        });
      }
    });

    c.addEventListener('mousedown', (e) => {
      e.preventDefault();
      let x, y;
      if (this._pointerLocked) {
        x = Math.round(this._virtualX);
        y = Math.round(this._virtualY);
      } else {
        const pos = this._toRemote(e.clientX, e.clientY);
        x = pos.x;
        y = pos.y;
      }
      this._send(this._wasm.encode_mouse_button(e.button, true, x, y));
    });

    c.addEventListener('mouseup', (e) => {
      e.preventDefault();
      let x, y;
      if (this._pointerLocked) {
        x = Math.round(this._virtualX);
        y = Math.round(this._virtualY);
      } else {
        const pos = this._toRemote(e.clientX, e.clientY);
        x = pos.x;
        y = pos.y;
      }
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
    //
    // We forward keystrokes to the remote as **PS/2 Set 1 hardware
    // scancodes** (the "Parsec method"). The remote interprets each
    // scancode through its currently active keyboard layout, so the
    // physical key the user pressed produces the same character it
    // would on a directly attached keyboard with that layout.
    //
    // For this to feel right when the client and the remote use
    // different layouts, the user can pick the remote layout from
    // the toolbar; the client sends a `SetKeyboardLayout` message
    // and the server switches the foreground window's input
    // language accordingly.
    //
    // We remember per-physical-key (`e.code`) whether a keydown was
    // actually sent so the matching keyup releases the same scancode
    // (and isn't sent if the keydown was suppressed, e.g. for a key
    // we don't have in the map).
    /** @private @type {Map<string, {scan: number, ext: boolean}>} */
    this._pressedKeys = new Map();

    c.addEventListener('keydown', (e) => {
      // Don't intercept Ctrl+Alt+M (pointer lock toggle) or Escape (toolbar)
      if ((e.ctrlKey && e.altKey && e.code === 'KeyM') || e.key === 'Escape') {
        return;
      }
      e.preventDefault();
      // If this physical key is already marked as pressed (auto-repeat),
      // re-send the same scancode so the remote sees the repeat.
      const existing = this._pressedKeys.get(e.code);
      if (existing) {
        this._send(this._wasm.encode_key_scancode(existing.scan, existing.ext, true));
        return;
      }
      const entry = this._lookupScancode(e);
      if (!entry) return;
      this._pressedKeys.set(e.code, entry);
      this._send(this._wasm.encode_key_scancode(entry.scan, entry.ext, true));
    });

    c.addEventListener('keyup', (e) => {
      if ((e.ctrlKey && e.altKey && e.code === 'KeyM') || e.key === 'Escape') {
        return;
      }
      e.preventDefault();
      const entry = this._pressedKeys.get(e.code);
      this._pressedKeys.delete(e.code);
      if (entry) {
        this._send(this._wasm.encode_key_scancode(entry.scan, entry.ext, false));
        return;
      }
      // Fallback: no record (e.g. keyup arrived without a matching
      // keydown because the page just gained focus). Look the key up
      // and release it so the remote isn't left with a stuck key.
      const fallback = this._lookupScancode(e);
      if (fallback) {
        this._send(this._wasm.encode_key_scancode(fallback.scan, fallback.ext, false));
      }
    });

    // If the page loses focus or the canvas loses keyboard focus, release
    // every key we believe is still held.  Otherwise the remote can be
    // left with "stuck" modifiers / characters.
    const releaseAll = () => {
      for (const entry of this._pressedKeys.values()) {
        this._send(this._wasm.encode_key_scancode(entry.scan, entry.ext, false));
      }
      this._pressedKeys.clear();
    };
    window.addEventListener('blur', releaseAll);
    c.addEventListener('blur', releaseAll);
  }

  /**
   * Look up the PS/2 Set 1 scancode (and extended-key flag) for a
   * given KeyboardEvent. Uses `e.code` (physical position) only –
   * the layout-dependent translation is performed on the remote.
   * @private
   * @param {KeyboardEvent} e
   * @returns {{scan: number, ext: boolean} | null}
   */
  _lookupScancode(e) {
    const m = SCANCODE_MAP[e.code];
    if (!m) return null;
    return { scan: m[0], ext: m[1] };
  }
}
