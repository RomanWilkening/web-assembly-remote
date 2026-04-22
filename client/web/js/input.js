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
      const now = performance.now();
      if (now - this._lastMoveTime < this._moveThrottleMs) return;
      this._lastMoveTime = now;

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
      this._send(this._wasm.encode_mouse_move(x, y));
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
    // We deliver keystrokes to the remote in two different ways depending
    // on what the user pressed:
    //
    //   • For plain typing (printable characters with no Ctrl/Meta/plain-Alt
    //     modifier, including AltGr-produced characters like '@' on a
    //     German layout), we send the actual Unicode character via
    //     `encode_key_unicode`.  The server injects it with
    //     SendInput+KEYEVENTF_UNICODE, which bypasses the keyboard layout
    //     active on the remote.  This guarantees that what the user typed
    //     locally is what arrives on the remote, regardless of the layout
    //     mismatch between client (e.g. de-DE QWERTZ) and remote (e.g.
    //     en-US QWERTY).
    //
    //   • For everything else – modifier keys, shortcuts (Ctrl+C, Alt+F4,
    //     Win+L, …), navigation (arrows, Home/End/PgUp/PgDn), F-keys,
    //     Enter/Tab/Backspace/Esc/Delete – we send the Windows Virtual-Key
    //     code derived from `e.code` so that shortcuts behave consistently.
    //
    // We remember per-physical-key (`e.code`) which path was used on
    // keydown so the matching keyup releases the same way.
    /** @private @type {Map<string, {kind: 'unicode', codepoint: number} | {kind: 'vk', vk: number}>} */
    this._pressedKeys = new Map();

    c.addEventListener('keydown', (e) => {
      // Don't intercept Ctrl+Alt+M (pointer lock toggle) or Escape (toolbar)
      if ((e.ctrlKey && e.altKey && e.code === 'KeyM') || e.key === 'Escape') {
        return;
      }
      // Ignore dead keys – the browser will fire a follow-up event with
      // the composed character (e.g. '´' + 'a' → 'á') which we forward
      // as a Unicode injection.
      if (e.key === 'Dead') {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      // If this physical key is already marked as pressed (auto-repeat),
      // re-send using the previously chosen path so it stays consistent.
      const existing = this._pressedKeys.get(e.code);
      if (existing) {
        this._sendKeyDown(existing);
        return;
      }
      const entry = this._chooseKeyEncoding(e);
      if (!entry) return;
      this._pressedKeys.set(e.code, entry);
      this._sendKeyDown(entry);
    });

    c.addEventListener('keyup', (e) => {
      if ((e.ctrlKey && e.altKey && e.code === 'KeyM') || e.key === 'Escape') {
        return;
      }
      if (e.key === 'Dead') {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      const entry = this._pressedKeys.get(e.code);
      this._pressedKeys.delete(e.code);
      if (entry) {
        this._sendKeyUp(entry);
        return;
      }
      // Fallback: no record (e.g. keyup arrived without a matching
      // keydown because the page just gained focus).  Use the same
      // selection logic as keydown so we still release the right key.
      const fallback = this._chooseKeyEncoding(e);
      if (fallback) this._sendKeyUp(fallback);
    });

    // If the page loses focus or the canvas loses keyboard focus, release
    // every key we believe is still held.  Otherwise the remote can be
    // left with "stuck" modifiers / characters.
    const releaseAll = () => {
      for (const entry of this._pressedKeys.values()) {
        this._sendKeyUp(entry);
      }
      this._pressedKeys.clear();
    };
    window.addEventListener('blur', releaseAll);
    c.addEventListener('blur', releaseAll);
  }

  /**
   * Decide whether a key event should be sent as a Unicode character
   * (so the remote sees the exact character regardless of its layout)
   * or as a Windows Virtual-Key code (so shortcuts and special keys
   * keep working).
   * @private
   * @param {KeyboardEvent} e
   * @returns {{kind: 'unicode', codepoint: number} | {kind: 'vk', vk: number} | null}
   */
  _chooseKeyEncoding(e) {
    // Pure modifier keys: always send as VK so the remote knows they
    // are held while subsequent keys are pressed.
    const isModifier =
      e.code === 'ShiftLeft' || e.code === 'ShiftRight' ||
      e.code === 'ControlLeft' || e.code === 'ControlRight' ||
      e.code === 'AltLeft' || e.code === 'AltRight' ||
      e.code === 'MetaLeft' || e.code === 'MetaRight';

    // Determine whether this is a plain printable character that should
    // be Unicode-injected.  `e.key` is a single Unicode character for
    // printable keys, and a longer name (e.g. 'Enter', 'ArrowLeft') for
    // non-printable keys.
    //
    // We also need to handle AltGr (which on Windows is reported as
    // Ctrl+Alt) – AltGr-produced characters like '@' on a German
    // keyboard should be sent as Unicode, not interpreted as Ctrl+Alt+Q.
    const isAltGr = e.ctrlKey && e.altKey;
    const ctrlOrMetaShortcut = (e.ctrlKey && !isAltGr) || e.metaKey;
    const altShortcut = e.altKey && !isAltGr; // e.g. Alt+F4

    // Use a Unicode codepoint when:
    //   • the key produced exactly one Unicode scalar value, AND
    //   • it is not a modifier key on its own, AND
    //   • there is no Ctrl/Meta/plain-Alt shortcut active (AltGr is OK).
    if (
      !isModifier &&
      !ctrlOrMetaShortcut &&
      !altShortcut &&
      typeof e.key === 'string' &&
      e.key.length > 0
    ) {
      // `e.key` may be a single BMP character (length 1) or a
      // surrogate pair representing a supplementary-plane codepoint
      // (length 2).  `String.codePointAt(0)` handles both correctly.
      const cp = e.key.codePointAt(0);
      const charLen = cp > 0xFFFF ? 2 : 1;
      if (e.key.length === charLen) {
        return { kind: 'unicode', codepoint: cp };
      }
      // Multi-character names like 'Enter', 'Tab', 'ArrowUp' fall
      // through to the VK path below.
    }

    const vk = VK_MAP[e.code];
    if (vk === undefined) return null;
    return { kind: 'vk', vk };
  }

  /** @private */
  _sendKeyDown(entry) {
    if (entry.kind === 'unicode') {
      this._send(this._wasm.encode_key_unicode(entry.codepoint, true));
    } else {
      this._send(this._wasm.encode_key_event(entry.vk, true));
    }
  }

  /** @private */
  _sendKeyUp(entry) {
    if (entry.kind === 'unicode') {
      this._send(this._wasm.encode_key_unicode(entry.codepoint, false));
    } else {
      this._send(this._wasm.encode_key_event(entry.vk, false));
    }
  }
}
