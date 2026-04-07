/**
 * WebCodecs-based H.264 decoder for ultra-low-latency video.
 *
 * Receives Annex-B access units and converts them to AVC format
 * for the browser's hardware-accelerated VideoDecoder.
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
    this._sps = null;
    /** @private */
    this._pps = null;
  }

  /**
   * Configure the decoder once the first key-frame arrives.
   * SPS + PPS are extracted from the Annex-B data to build the
   * avcC description required by WebCodecs.
   * @param {Uint8Array} annexB – complete key-frame access unit
   */
  _configureFromKeyFrame(annexB) {
    const nalUnits = parseAnnexB(annexB);

    for (const nal of nalUnits) {
      const type_ = nal[0] & 0x1f;
      if (type_ === 7) this._sps = nal; // SPS
      if (type_ === 8) this._pps = nal; // PPS
    }

    if (!this._sps || !this._pps) {
      console.warn('Key-frame missing SPS/PPS – cannot configure decoder');
      return;
    }

    const description = buildAvcC(this._sps, this._pps);
    const profile = this._sps[1];
    const compat = this._sps[2];
    const level = this._sps[3];
    const codec = `avc1.${hex(profile)}${hex(compat)}${hex(level)}`;

    this._decoder = new VideoDecoder({
      output: (frame) => this._onFrame(frame),
      error: (e) => console.error('VideoDecoder error:', e),
    });

    this._decoder.configure({
      codec,
      description,
      optimizeForLatency: true,
    });

    this._configured = true;
    console.log(`Decoder configured: ${codec}`);
  }

  /**
   * Decode one H.264 access unit (Annex-B).
   * @param {Uint8Array} annexB
   * @param {boolean} isKeyFrame
   * @param {number} timestampUs – microsecond timestamp from server
   */
  decode(annexB, isKeyFrame, timestampUs) {
    if (!this._configured) {
      if (!isKeyFrame) return; // need a key-frame first
      this._configureFromKeyFrame(annexB);
      if (!this._configured) return;
    }

    // Convert Annex-B → AVC (length-prefixed NAL units)
    const avcData = annexBToAvc(annexB);

    const chunk = new EncodedVideoChunk({
      type: isKeyFrame ? 'key' : 'delta',
      timestamp: timestampUs, // µs
      data: avcData,
    });

    this._decoder.decode(chunk);
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

/**
 * Convert Annex-B data to AVC format (4-byte big-endian length prefix
 * per NAL unit, start codes removed).
 * @param {Uint8Array} annexB
 * @returns {Uint8Array}
 */
function annexBToAvc(annexB) {
  const nals = parseAnnexB(annexB);

  let totalSize = 0;
  for (const nal of nals) {
    // Skip AUD NAL units (type 9) – decoder doesn't need them
    if ((nal[0] & 0x1f) === 9) continue;
    totalSize += 4 + nal.length;
  }

  const out = new Uint8Array(totalSize);
  let offset = 0;
  for (const nal of nals) {
    if ((nal[0] & 0x1f) === 9) continue;
    // 4-byte big-endian length
    out[offset]     = (nal.length >>> 24) & 0xff;
    out[offset + 1] = (nal.length >>> 16) & 0xff;
    out[offset + 2] = (nal.length >>> 8) & 0xff;
    out[offset + 3] =  nal.length & 0xff;
    out.set(nal, offset + 4);
    offset += 4 + nal.length;
  }

  return out;
}

/**
 * Build an avcC (AVC Decoder Configuration Record) from SPS and PPS.
 * @param {Uint8Array} sps
 * @param {Uint8Array} pps
 * @returns {Uint8Array}
 */
function buildAvcC(sps, pps) {
  const len = 6 + 2 + sps.length + 1 + 2 + pps.length;
  const buf = new Uint8Array(len);
  let i = 0;
  buf[i++] = 1;           // configurationVersion
  buf[i++] = sps[1];      // AVCProfileIndication
  buf[i++] = sps[2];      // profile_compatibility
  buf[i++] = sps[3];      // AVCLevelIndication
  buf[i++] = 0xff;        // lengthSizeMinusOne = 3  →  4-byte lengths
  buf[i++] = 0xe1;        // numOfSequenceParameterSets = 1
  buf[i++] = (sps.length >> 8) & 0xff;
  buf[i++] = sps.length & 0xff;
  buf.set(sps, i); i += sps.length;
  buf[i++] = 1;           // numOfPictureParameterSets
  buf[i++] = (pps.length >> 8) & 0xff;
  buf[i++] = pps.length & 0xff;
  buf.set(pps, i);
  return buf;
}

/** @param {number} n @returns {string} */
function hex(n) {
  return n.toString(16).padStart(2, '0');
}
