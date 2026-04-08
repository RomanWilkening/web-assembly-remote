/**
 * Low-latency audio player for raw f32le interleaved stereo PCM
 * received from the remote desktop server.
 *
 * Uses an AudioWorklet with a ring buffer for smooth playback and
 * a GainNode to implement mute/unmute.  Starts muted by default.
 */

export class AudioPlayer {
  constructor() {
    /** @private @type {AudioContext|null} */
    this._ctx = null;
    /** @private @type {GainNode|null} */
    this._gain = null;
    /** @private @type {AudioWorkletNode|null} */
    this._worklet = null;
    /** @private */
    this._muted = true;
    /** @private */
    this._ready = false;
    /** @private @type {Promise<void>|null} */
    this._initPromise = null;
  }

  /**
   * Initialise the audio graph.  Safe to call multiple times –
   * subsequent calls are no-ops.
   */
  async init() {
    if (this._initPromise) return this._initPromise;
    this._initPromise = this._doInit();
    return this._initPromise;
  }

  /** @private */
  async _doInit() {
    try {
      this._ctx = new AudioContext({ sampleRate: 48000 });

      // GainNode for mute control.
      this._gain = this._ctx.createGain();
      this._gain.gain.value = 0; // start muted
      this._gain.connect(this._ctx.destination);

      // Load the AudioWorklet processor.
      await this._ctx.audioWorklet.addModule('js/pcm-worklet.js');

      this._worklet = new AudioWorkletNode(this._ctx, 'pcm-player', {
        outputChannelCount: [2],
      });
      this._worklet.connect(this._gain);

      this._ready = true;
      console.log('AudioPlayer initialised (48 kHz stereo, muted)');
    } catch (e) {
      console.error('AudioPlayer init failed:', e);
      this._ready = false;
    }
  }

  /**
   * Feed raw f32le interleaved stereo PCM data to the player.
   * @param {Uint8Array} pcmBytes – raw bytes (f32le)
   */
  feed(pcmBytes) {
    if (!this._ready || !this._worklet) return;

    // Resume the context if it was suspended (autoplay policy).
    if (this._ctx && this._ctx.state === 'suspended') {
      this._ctx.resume().catch(() => {});
    }

    // Copy the bytes into an ArrayBuffer and transfer to the worklet.
    const copy = pcmBytes.buffer.slice(
      pcmBytes.byteOffset,
      pcmBytes.byteOffset + pcmBytes.byteLength,
    );
    this._worklet.port.postMessage(copy, [copy]);
  }

  /**
   * Set the muted state.
   * @param {boolean} muted
   */
  setMuted(muted) {
    this._muted = muted;
    if (this._gain) {
      this._gain.gain.value = muted ? 0 : 1;
    }
    // Resuming on unmute satisfies browser autoplay policy
    // (the click on the unmute button is the user gesture).
    if (!muted && this._ctx && this._ctx.state === 'suspended') {
      this._ctx.resume().catch(() => {});
    }
  }

  /** @returns {boolean} */
  get muted() {
    return this._muted;
  }

  /** Stop playback and release resources. */
  close() {
    if (this._worklet) {
      this._worklet.disconnect();
      this._worklet = null;
    }
    if (this._ctx) {
      this._ctx.close().catch(() => {});
      this._ctx = null;
    }
    this._ready = false;
    this._initPromise = null;
  }
}
