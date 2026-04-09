/**
 * Renders decoded VideoFrame objects onto a <canvas> element.
 * Uses requestAnimationFrame to synchronise with the display.
 */

export class Renderer {
  /** @param {HTMLCanvasElement} canvas */
  constructor(canvas) {
    /** @private */
    this._canvas = canvas;
    /** @private */
    this._ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    /** @private @type {VideoFrame|null} */
    this._pendingFrame = null;
    /** @private */
    this._animId = 0;
    /** @private */
    this._running = false;
  }

  /**
   * Set the canvas resolution to match the remote desktop.
   * @param {number} width
   * @param {number} height
   */
  resize(width, height) {
    this._canvas.width = width;
    this._canvas.height = height;
  }

  /**
   * Submit a decoded VideoFrame for rendering.
   * Only the latest frame is kept – if the display hasn't
   * refreshed yet the previous pending frame is dropped.
   * @param {VideoFrame} frame
   */
  drawFrame(frame) {
    // Drop the old pending frame to keep latency at minimum.
    if (this._pendingFrame) {
      this._pendingFrame.close();
    }
    this._pendingFrame = frame;

    if (!this._running) {
      this._running = true;
      this._scheduleRender();
    }
  }

  /** @private */
  _scheduleRender() {
    this._animId = requestAnimationFrame(() => this._render());
  }

  /** @private */
  _render() {
    const frame = this._pendingFrame;
    if (frame) {
      this._pendingFrame = null;
      try {
        this._ctx.drawImage(frame, 0, 0, this._canvas.width, this._canvas.height);
      } catch (e) {
        console.warn('drawImage failed:', e);
      }
      frame.close();
    }
    if (this._running) {
      this._scheduleRender();
    }
  }

  stop() {
    this._running = false;
    cancelAnimationFrame(this._animId);
    if (this._pendingFrame) {
      this._pendingFrame.close();
      this._pendingFrame = null;
    }
  }
}
