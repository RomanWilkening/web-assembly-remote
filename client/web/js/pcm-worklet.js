/**
 * AudioWorklet processor that plays interleaved stereo f32le PCM
 * received via its MessagePort.
 *
 * Data arrives as ArrayBuffer containing interleaved stereo samples
 * (left, right, left, right, …) at 48 kHz.
 */

class PCMPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();

    // Ring buffer – ~1 second of stereo audio at 48 kHz.
    this._size = 48000 * 2;
    this._buf = new Float32Array(this._size);
    this._writePos = 0;
    this._readPos = 0;
    this._available = 0;

    this.port.onmessage = (e) => {
      const buffer = e.data;
      const samples = new Float32Array(buffer);
      const len = samples.length;
      // Hand the underlying ArrayBuffer back to the main thread so it
      // can be recycled instead of GC'd (see AudioPlayer._bufferPool).
      // We do this *before* the early-return so even empty chunks are
      // recycled rather than dropped on the floor.
      try {
        this.port.postMessage(buffer, [buffer]);
      } catch (_) { /* ignore – worst case the main thread allocates */ }
      if (len === 0) return;

      // Run-length write: at most two contiguous segments (before and
      // after the wrap-around).  `Float32Array.set` is a single typed
      // memcpy — orders of magnitude faster than a per-sample modulo
      // loop when called 50× per second with ~1920 samples each.
      const size = this._size;
      const wp = this._writePos;
      const first = Math.min(len, size - wp);
      this._buf.set(samples.subarray(0, first), wp);
      if (first < len) {
        this._buf.set(samples.subarray(first), 0);
        this._writePos = len - first;
      } else {
        this._writePos = (wp + first) % size;
      }

      this._available += len;
      // Clamp to buffer size (discard oldest data on overflow).
      if (this._available > size) {
        const overflow = this._available - size;
        this._readPos = (this._readPos + overflow) % size;
        this._available = size;
      }
    };
  }

  process(_inputs, outputs) {
    const output = outputs[0];
    if (!output || output.length === 0) return true;

    const left = output[0];
    const right = output.length > 1 ? output[1] : null;
    const frames = left.length;
    const need = frames * 2; // interleaved stereo

    if (this._available >= need) {
      // Fast path: enough buffered data — copy with two contiguous
      // typed-array reads (before / after ring-buffer wrap).
      const size = this._size;
      let rp = this._readPos;
      const first = Math.min(need, size - rp);
      const seg1 = this._buf.subarray(rp, rp + first);
      let seg2 = null;
      if (first < need) {
        seg2 = this._buf.subarray(0, need - first);
      }
      // De-interleave into the output planes.
      // Walk seg1 then (optional) seg2 in 2-sample steps.
      let oi = 0;
      for (let i = 0; i < seg1.length; i += 2) {
        left[oi] = seg1[i];
        if (right) right[oi] = seg1[i + 1];
        oi++;
      }
      if (seg2) {
        for (let i = 0; i < seg2.length; i += 2) {
          left[oi] = seg2[i];
          if (right) right[oi] = seg2[i + 1];
          oi++;
        }
      }
      rp += need;
      if (rp >= size) rp -= size;
      this._readPos = rp;
      this._available -= need;
    } else {
      // Underrun: emit silence (typed-array fill is one memset).
      left.fill(0);
      if (right) right.fill(0);
    }

    return true;
  }
}

registerProcessor('pcm-player', PCMPlayerProcessor);
