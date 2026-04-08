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
      const samples = new Float32Array(e.data);
      const len = samples.length;

      for (let i = 0; i < len; i++) {
        this._buf[this._writePos] = samples[i];
        this._writePos = (this._writePos + 1) % this._size;
      }

      this._available += len;
      // Clamp to buffer size (discard oldest data on overflow).
      if (this._available > this._size) {
        const overflow = this._available - this._size;
        this._readPos = (this._readPos + overflow) % this._size;
        this._available = this._size;
      }
    };
  }

  process(_inputs, outputs) {
    const output = outputs[0];
    if (!output || output.length === 0) return true;

    const left = output[0];
    const right = output.length > 1 ? output[1] : left;
    const frames = left.length;

    for (let i = 0; i < frames; i++) {
      if (this._available >= 2) {
        left[i] = this._buf[this._readPos];
        this._readPos = (this._readPos + 1) % this._size;
        right[i] = this._buf[this._readPos];
        this._readPos = (this._readPos + 1) % this._size;
        this._available -= 2;
      } else {
        // Buffer underrun – output silence.
        left[i] = 0;
        right[i] = 0;
      }
    }

    return true;
  }
}

registerProcessor('pcm-player', PCMPlayerProcessor);
