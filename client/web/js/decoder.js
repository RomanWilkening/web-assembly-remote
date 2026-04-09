/**
 * WebCodecs-based H.264 decoder for ultra-low-latency video.
 *
 * Receives Annex-B access units and passes them directly to the
 * browser's hardware-accelerated VideoDecoder in Annex-B mode
 * (no avcC description). This is the most compatible approach
 * and avoids complex Annex-B → AVC conversion.
 */

/**
 * @callback OnFrameCallback
 * @param {VideoFrame} frame
 */

export class H264Decoder {
  /** @param {OnFrameCallback} onFrame */
  constructor(onFrame) {
    /** @private */
    this._onFrame = onFrame;
    /** @private @type {VideoDecoder|null} */
    this._decoder = null;
    /** @private */
    this._configured = false;
    /** @private */
    this._codedWidth = 0;
    /** @private */
    this._codedHeight = 0;
    /** @private */
    this._useSoftware = false;
  }

  /**
   * Set the remote desktop dimensions for the decoder configuration.
   * Must be called before the first decode() call (typically from
   * ServerInfo).
   * @param {number} width
   * @param {number} height
   */
  setRemoteSize(width, height) {
    this._codedWidth = width;
    this._codedHeight = height;
  }

  /**
   * Configure the decoder once the first key-frame arrives.
   * SPS is extracted from the Annex-B data to build the codec
   * string. No avcC description is provided – Chrome will decode
   * the raw Annex-B data directly.
   * @param {Uint8Array} annexB – complete key-frame access unit
   */
  _configureFromKeyFrame(annexB) {
    const nalUnits = parseAnnexB(annexB);

    let sps = null;
    for (const nal of nalUnits) {
      if (nal.length > 3 && (nal[0] & 0x1f) === 7) {
        sps = nal;
        break;
      }
    }

    if (!sps) {
      console.warn('Key-frame missing SPS – cannot configure decoder');
      return;
    }

    const profile = sps[1];
    const compat = sps[2];
    const level = sps[3];
    const codec = `avc1.${hex(profile)}${hex(compat)}${hex(level)}`;

    // Close any previous decoder instance.
    if (this._decoder && this._decoder.state !== 'closed') {
      try { this._decoder.close(); } catch (_) { /* ignore */ }
    }

    this._decoder = new VideoDecoder({
      output: (frame) => this._onFrame(frame),
      error: (e) => {
        console.error('VideoDecoder error:', e);
        // Mark as unconfigured so the next key-frame triggers
        // re-initialisation instead of feeding a closed decoder.
        this._configured = false;
        // If hardware decoding failed, try software next time.
        if (!this._useSoftware) {
          console.warn('Retrying with software decoding on next key-frame');
          this._useSoftware = true;
        }
      },
    });

    /** @type {VideoDecoderConfig} */
    const config = {
      codec,
      optimizeForLatency: true,
    };

    // Provide explicit dimensions if known (improves compatibility).
    if (this._codedWidth > 0 && this._codedHeight > 0) {
      config.codedWidth = this._codedWidth;
      config.codedHeight = this._codedHeight;
    }

    // After a hardware-decode failure, fall back to software.
    if (this._useSoftware) {
      config.hardwareAcceleration = 'prefer-software';
    }

    this._decoder.configure(config);
    this._configured = true;

    const accel = this._useSoftware ? ' (software)' : '';
    console.log(`Decoder configured: ${codec}${accel}`);
  }

  /**
   * Decode one H.264 access unit (Annex-B).
   * The raw Annex-B data (with start codes and inline SPS/PPS)
   * is passed directly to WebCodecs.
   * @param {Uint8Array} annexB
   * @param {boolean} isKeyFrame
   * @param {number} timestampUs – microsecond timestamp from server
   */
  decode(annexB, isKeyFrame, timestampUs) {
    // If the decoder was closed (e.g. after an error), reset and wait
    // for the next key-frame to reconfigure.
    if (this._decoder && this._decoder.state === 'closed') {
      this._configured = false;
    }

    if (!this._configured) {
      if (!isKeyFrame) return; // need a key-frame first
      this._configureFromKeyFrame(annexB);
      if (!this._configured) return;
    }

    // When frames arrive in bursts (common with SSL-inspecting proxies
    // like Netskope that buffer data), the decoder queue can grow
    // unbounded. Drop non-key delta frames to prevent overload;
    // always accept key-frames so the decoder can resynchronise.
    if (!isKeyFrame && this._decoder.decodeQueueSize > 3) {
      return;
    }

    // Pass raw Annex-B data directly – no conversion needed.
    // Chrome decodes Annex-B natively when no avcC description
    // was provided during configure().
    const chunk = new EncodedVideoChunk({
      type: isKeyFrame ? 'key' : 'delta',
      timestamp: timestampUs, // µs
      data: annexB,
    });

    try {
      this._decoder.decode(chunk);
    } catch (e) {
      console.error('decode() threw:', e);
      this._configured = false;
    }
  }

  close() {
    if (this._decoder && this._decoder.state !== 'closed') {
      this._decoder.close();
    }
  }
}

// ---------------------------------------------------------------------------
// Annex-B parsing utilities
// ---------------------------------------------------------------------------

/**
 * Split an Annex-B byte-stream into individual NAL unit payloads
 * (without start-code prefix).
 * @param {Uint8Array} data
 * @returns {Uint8Array[]}
 */
function parseAnnexB(data) {
  const nals = [];
  let i = 0;

  while (i < data.length) {
    // Detect 3- or 4-byte start codes
    if (
      i + 2 < data.length &&
      data[i] === 0 && data[i + 1] === 0
    ) {
      let scLen;
      if (data[i + 2] === 1) {
        scLen = 3;
      } else if (
        i + 3 < data.length &&
        data[i + 2] === 0 && data[i + 3] === 1
      ) {
        scLen = 4;
      } else {
        i++;
        continue;
      }

      const nalStart = i + scLen;
      // Find next start code
      let nalEnd = data.length;
      for (let j = nalStart + 1; j + 2 < data.length; j++) {
        if (
          data[j] === 0 && data[j + 1] === 0 &&
          (data[j + 2] === 1 ||
            (j + 3 < data.length && data[j + 2] === 0 && data[j + 3] === 1))
        ) {
          nalEnd = j;
          break;
        }
      }

      nals.push(data.subarray(nalStart, nalEnd));
      i = nalEnd;
    } else {
      i++;
    }
  }

  return nals;
}

/** @param {number} n @returns {string} */
function hex(n) {
  return n.toString(16).padStart(2, '0');
}
