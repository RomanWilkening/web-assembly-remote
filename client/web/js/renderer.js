/**
 * Renders decoded VideoFrame objects onto a <canvas> element.
 *
 * Uses a WebGL2 fast-path (`texImage2D(TEXTURE_2D, …, videoFrame)`) when
 * available — on Chrome/Edge this is a zero-copy upload of the decoder's
 * native YUV surface to a GPU texture, which a tiny fullscreen-quad
 * shader then samples to RGBA.  Falls back to Canvas2D `drawImage` on
 * browsers where WebGL2 isn't available or texImage2D rejects the frame.
 *
 * Two scheduling modes are supported:
 *
 *   * `lowLatency: true` (default) — paint immediately on
 *     `drawFrame()`.  The GPU command is issued straight away so the
 *     compositor can present at the very next display refresh.  When
 *     two decoded frames arrive within the same vsync interval the
 *     second simply overdraws the first; the wasted GPU draw call is
 *     vanishingly cheap for our single-quad shader and the user only
 *     ever sees the latest pixels anyway.
 *
 *   * `lowLatency: false` — coalesce through `requestAnimationFrame`
 *     so we paint at most once per display refresh.  Slightly less CPU
 *     when running well above the display refresh rate but trades up
 *     to one vsync of extra latency.  Kept as an opt-out for power
 *     profiling.
 *
 * In both modes only the latest pending frame is kept; older
 * undelivered frames are `close()`d so the GPU surface pool isn't
 * leaked.
 */

export class Renderer {
  /**
   * @param {HTMLCanvasElement} canvas
   * @param {{lowLatency?: boolean}} [opts]
   */
  constructor(canvas, opts = {}) {
    /** @private */
    this._canvas = canvas;
    /** @private @type {VideoFrame|null} */
    this._pendingFrame = null;
    /** @private */
    this._animId = 0;
    /** @private */
    this._running = false;
    /** @private */
    this._lowLatency = opts.lowLatency !== false;

    // ── Diagnostic counters ─────────────────────────────────────
    // Symmetrical with the server's encoder-reader / ws-sender
    // counters and the decoder's `stats()`.  `submitted` increments on
    // every drawFrame() call (one per VideoDecoder output);
    // `painted` increments only after a successful backend `draw()` —
    // a gap between the two means frames are being coalesced (in
    // rAF mode) or thrown away due to a draw error.
    /** @private */ this._submitted    = 0;
    /** @private */ this._painted      = 0;
    /** @private */ this._coalesced    = 0;
    /** @private */ this._drawErrors   = 0;
    /** @private @type {string|null} */ this._lastDrawError = null;

    /** @private @type {WebGL2Backend|Canvas2DBackend} */
    this._backend = WebGL2Backend.tryCreate(canvas) || new Canvas2DBackend(canvas);
    console.log(
      `Renderer backend: ${this._backend.name}` +
      (this._lowLatency ? ' (low-latency, sync paint)' : ' (rAF-coalesced)'),
    );
  }

  /**
   * Snapshot of the diagnostic counters for the periodic ticker in
   * `main.js`.  Never throws.
   * @returns {{submitted:number,painted:number,coalesced:number,errors:number,lastError:(string|null),backend:string,canvas:string}}
   */
  stats() {
    return {
      submitted: this._submitted,
      painted: this._painted,
      coalesced: this._coalesced,
      errors: this._drawErrors,
      lastError: this._lastDrawError,
      backend: this._backend ? this._backend.name : 'none',
      canvas: `${this._canvas.width}x${this._canvas.height}`,
    };
  }

  /**
   * Set the canvas resolution to match the remote desktop.
   * @param {number} width
   * @param {number} height
   */
  resize(width, height) {
    this._canvas.width = width;
    this._canvas.height = height;
    this._backend.resize(width, height);
  }

  /**
   * Submit a decoded VideoFrame for rendering.
   * Only the latest frame is kept – if the display hasn't
   * refreshed yet the previous pending frame is dropped.
   * @param {VideoFrame} frame
   */
  drawFrame(frame) {
    this._submitted++;
    // Drop the old pending frame to keep latency at minimum.
    if (this._pendingFrame) {
      this._coalesced++;
      this._pendingFrame.close();
    }
    this._pendingFrame = frame;
    this._running = true;

    if (this._lowLatency) {
      // Paint immediately so the GPU draw call hits the compositor
      // before the next vsync window starts.  This shaves up to one
      // refresh of latency vs. the rAF path.
      this._render();
      return;
    }

    // Schedule a single rAF only if one isn't already queued.  This keeps
    // the rAF callback dormant when no new frames arrive (e.g. the remote
    // desktop is idle) instead of waking up the main thread 60 times per
    // second to do nothing.
    if (this._animId === 0) {
      this._scheduleRender();
    }
  }

  /** @private */
  _scheduleRender() {
    this._animId = requestAnimationFrame(() => this._render());
  }

  /** @private */
  _render() {
    this._animId = 0;
    const frame = this._pendingFrame;
    if (frame) {
      this._pendingFrame = null;
      try {
        this._backend.draw(frame);
        this._painted++;
        // Diagnostic: log the first two paints with the backend and
        // canvas dimensions.  A silent gap between "VideoDecoder
        // produced frame #N" and "Renderer painted frame #N" means the
        // backend `draw()` is throwing or the canvas is hidden / 0×0.
        if (this._painted <= 2) {
          console.log(
            `Renderer painted frame #${this._painted}: ` +
            `backend=${this._backend.name}, ` +
            `canvas=${this._canvas.width}x${this._canvas.height}, ` +
            `frame=${frame.codedWidth}x${frame.codedHeight}`,
          );
        }
      } catch (e) {
        this._drawErrors++;
        this._lastDrawError = (e && e.message) ? e.message : String(e);
        console.warn('draw failed:', e);
        // If the WebGL2 backend dies for some reason (e.g. context lost
        // or a driver bug rejecting texImage2D from a VideoFrame on this
        // particular machine), permanently fall back to Canvas2D.
        if (this._backend.name === 'WebGL2') {
          console.warn('WebGL2 backend failed – falling back to Canvas2D');
          try { this._backend.dispose(); } catch (_) { /* ignore */ }
          this._backend = new Canvas2DBackend(this._canvas);
        }
      }
      frame.close();
    }
    // Re-arm rAF only if another frame arrived during the paint AND
    // we're in rAF-coalescing mode.  In low-latency mode `drawFrame`
    // re-enters `_render` synchronously so no scheduling is needed.
    if (!this._lowLatency && this._running && this._pendingFrame) {
      this._scheduleRender();
    }
  }

  stop() {
    this._running = false;
    if (this._animId !== 0) {
      cancelAnimationFrame(this._animId);
      this._animId = 0;
    }
    if (this._pendingFrame) {
      this._pendingFrame.close();
      this._pendingFrame = null;
    }
  }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/**
 * Canvas2D backend — universally supported, but on most platforms
 * `drawImage(VideoFrame)` is the slowest output path and may invoke a
 * software YUV→RGBA conversion in the driver.  Used as a last resort.
 */
class Canvas2DBackend {
  /** @param {HTMLCanvasElement} canvas */
  constructor(canvas) {
    /** @readonly */ this.name = 'Canvas2D';
    this._canvas = canvas;
    this._ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
  }

  resize(_w, _h) { /* canvas size already updated */ }

  /** @param {VideoFrame} frame */
  draw(frame) {
    this._ctx.drawImage(frame, 0, 0, this._canvas.width, this._canvas.height);
  }

  dispose() { /* nothing to release */ }
}

/**
 * WebGL2 backend — zero-copy `texImage2D` upload of the decoded
 * VideoFrame to a 2D texture, drawn with a single fullscreen quad.
 *
 * The textured-quad pipeline runs entirely on the GPU and avoids the
 * software YUV→RGBA path that Canvas2D `drawImage` may take.
 */
class WebGL2Backend {
  /**
   * Try to create a WebGL2 backend for `canvas`.  Returns `null` if the
   * browser doesn't support WebGL2 or the context can't be acquired.
   * @param {HTMLCanvasElement} canvas
   * @returns {WebGL2Backend|null}
   */
  static tryCreate(canvas) {
    try {
      const gl = canvas.getContext('webgl2', {
        alpha: false,
        antialias: false,
        depth: false,
        stencil: false,
        desynchronized: true,
        preserveDrawingBuffer: false,
        premultipliedAlpha: false,
      });
      if (!gl) return null;
      return new WebGL2Backend(canvas, gl);
    } catch (_) {
      return null;
    }
  }

  /**
   * @param {HTMLCanvasElement} canvas
   * @param {WebGL2RenderingContext} gl
   */
  constructor(canvas, gl) {
    /** @readonly */ this.name = 'WebGL2';
    this._canvas = canvas;
    this._gl = gl;

    // Compile the shader pair: a fullscreen quad with flipped V so the
    // top-left of the texture maps to the top-left of the canvas.
    const vsSrc = `#version 300 es
      const vec2 verts[4] = vec2[4](
        vec2(-1.0, -1.0),
        vec2( 1.0, -1.0),
        vec2(-1.0,  1.0),
        vec2( 1.0,  1.0)
      );
      const vec2 uvs[4] = vec2[4](
        vec2(0.0, 1.0),
        vec2(1.0, 1.0),
        vec2(0.0, 0.0),
        vec2(1.0, 0.0)
      );
      out vec2 vUv;
      void main() {
        vUv = uvs[gl_VertexID];
        gl_Position = vec4(verts[gl_VertexID], 0.0, 1.0);
      }
    `;
    const fsSrc = `#version 300 es
      precision mediump float;
      in vec2 vUv;
      uniform sampler2D uTex;
      out vec4 fragColor;
      void main() {
        fragColor = texture(uTex, vUv);
      }
    `;

    this._program = linkProgram(gl, compile(gl, gl.VERTEX_SHADER, vsSrc),
                                    compile(gl, gl.FRAGMENT_SHADER, fsSrc));
    gl.useProgram(this._program);
    gl.uniform1i(gl.getUniformLocation(this._program, 'uTex'), 0);

    // Empty VAO — gl_VertexID-driven triangle strip needs no buffers.
    this._vao = gl.createVertexArray();

    this._tex = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this._tex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);

    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.STENCIL_TEST);
    gl.disable(gl.BLEND);

    gl.viewport(0, 0, canvas.width, canvas.height);
  }

  resize(_w, _h) {
    this._gl.viewport(0, 0, this._canvas.width, this._canvas.height);
  }

  /** @param {VideoFrame} frame */
  draw(frame) {
    const gl = this._gl;
    gl.bindTexture(gl.TEXTURE_2D, this._tex);
    // Zero-copy on Chrome/Edge: the VideoDecoder output surface is
    // uploaded straight to a GPU texture without going through CPU.
    gl.texImage2D(
      gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame,
    );
    gl.useProgram(this._program);
    gl.bindVertexArray(this._vao);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  dispose() {
    const gl = this._gl;
    if (this._tex) gl.deleteTexture(this._tex);
    if (this._vao) gl.deleteVertexArray(this._vao);
    if (this._program) gl.deleteProgram(this._program);
    this._tex = null;
    this._vao = null;
    this._program = null;
  }
}

/**
 * @param {WebGL2RenderingContext} gl
 * @param {GLenum} type
 * @param {string} src
 * @returns {WebGLShader}
 */
function compile(gl, type, src) {
  const sh = gl.createShader(type);
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    const log = gl.getShaderInfoLog(sh);
    gl.deleteShader(sh);
    throw new Error(`Shader compile failed: ${log}`);
  }
  return sh;
}

/**
 * @param {WebGL2RenderingContext} gl
 * @param {WebGLShader} vs
 * @param {WebGLShader} fs
 * @returns {WebGLProgram}
 */
function linkProgram(gl, vs, fs) {
  const p = gl.createProgram();
  gl.attachShader(p, vs);
  gl.attachShader(p, fs);
  gl.linkProgram(p);
  // Shaders can be detached + deleted as soon as they're linked.
  gl.detachShader(p, vs); gl.deleteShader(vs);
  gl.detachShader(p, fs); gl.deleteShader(fs);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
    const log = gl.getProgramInfoLog(p);
    gl.deleteProgram(p);
    throw new Error(`Program link failed: ${log}`);
  }
  return p;
}
