/**
 * Codec-aware low-latency video decoder built on the WebCodecs
 * `VideoDecoder` API.
 *
 * Supports three input bitstream formats, selected by `setCodec()`
 * before the first key frame arrives (`ServerInfo.codec` from the
 * server's wire protocol):
 *
 *   * `0` – H.264 / AVC (Annex-B byte-stream, AUD-delimited)
 *   * `1` – H.265 / HEVC (Annex-B byte-stream, AUD-delimited)
 *   * `2` – AV1 (Low-Overhead Bitstream Format, raw OBU sequence)
 *
 * For all three formats the encoded chunks are passed through to the
 * browser decoder verbatim — Chrome decodes Annex-B H.264/HEVC natively
 * and accepts AV1 OBUs without an additional `description` payload, so
 * we never have to do AVCC/HVCC/AV1C conversion in JavaScript.
 *
 * The codec-string for `VideoDecoder.configure()` is derived from the
 * first key frame:
 *
 *   * H.264: parsed from SPS (profile_idc / constraint_set_flags /
 *     level_idc) for an exact `avc1.PPCCLL` string.
 *   * HEVC and AV1: a permissive default is used because parsing the
 *     full VPS/SPS or the AV1 sequence header in JS would be a
 *     significant amount of bit-stream code for a feature that, in
 *     practice, just needs to advertise "Main profile up to 4K".
 *     Production deployments that need a tighter codec string can wire
 *     it up via a future `--client-codec-override` server flag (TODO).
 */

// Wire-protocol codec IDs (must match `CodecKind::protocol_id` on the server).
export const CODEC_H264 = 0;
export const CODEC_HEVC = 1;
export const CODEC_AV1  = 2;

/**
 * @callback OnFrameCallback
 * @param {VideoFrame} frame
 */

export class VideoStreamDecoder {
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
    /** @private @type {number|null} – most recent measured one-way latency (ms) */
    this._latencyMs = null;
    /** @private @type {number} – CODEC_* constant */
    this._codecId = CODEC_H264;

    // ── Diagnostic counters ─────────────────────────────────────
    // Match the server-side `encoder-reader` / `ws-sender` counters so
    // an operator can correlate frame flow end-to-end.  Each number is
    // monotonic over the lifetime of this decoder instance; the
    // periodic ticker in `main.js` reads them via `stats()`.
    /** @private */ this._decodeIn        = 0; // chunks submitted to VideoDecoder
    /** @private */ this._decodeOut       = 0; // VideoFrames produced (output cb)
    /** @private */ this._droppedQueueFull = 0; // delta frames skipped due to deep queue
    /** @private */ this._droppedNoConfig  = 0; // delta frames dropped while waiting for IDR
    /** @private */ this._errors           = 0; // VideoDecoder errors observed
    /** @private @type {string|null} */ this._lastError = null;
  }

  /**
   * Snapshot of the diagnostic counters; called by the periodic ticker
   * in `main.js`.  Never throws.
   * @returns {{in:number,out:number,dropped:number,droppedNoCfg:number,errors:number,lastError:(string|null),queueSize:number,configured:boolean,state:string}}
   */
  stats() {
    return {
      in: this._decodeIn,
      out: this._decodeOut,
      dropped: this._droppedQueueFull,
      droppedNoCfg: this._droppedNoConfig,
      errors: this._errors,
      lastError: this._lastError,
      queueSize: this._decoder ? this._decoder.decodeQueueSize : 0,
      configured: this._configured,
      state: this._decoder ? this._decoder.state : 'none',
    };
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
   * Tell the decoder which codec the encoder is producing.  Reset of
   * the configuration happens automatically on the next key frame.
   * @param {number} codecId – one of CODEC_H264 / CODEC_HEVC / CODEC_AV1
   */
  setCodec(codecId) {
    if (codecId !== CODEC_H264 && codecId !== CODEC_HEVC && codecId !== CODEC_AV1) {
      console.warn(`Unknown codec ID ${codecId} – falling back to H.264`);
      codecId = CODEC_H264;
    }
    if (codecId !== this._codecId) {
      this._codecId = codecId;
      // Force re-configuration on the next key frame.
      if (this._decoder && this._decoder.state !== 'closed') {
        try { this._decoder.close(); } catch (_) { /* ignore */ }
      }
      this._configured = false;
      console.log(`Decoder codec set to ${codecName(codecId)}`);
    }
  }

  /**
   * Configure the decoder once the first key frame arrives.  The codec
   * string is derived per-codec; see the file header comment for the
   * rationale behind the HEVC / AV1 defaults.
   * @param {Uint8Array} keyFrame – complete key-frame access unit
   */
  _configureFromKeyFrame(keyFrame) {
    let codec;
    switch (this._codecId) {
      case CODEC_HEVC:
        // hev1.<profile>.<profile_compatibility>.L<level×30>.<constraints>
        // Profile 1 = Main, profile compatibility 6 = Main, level 5.1
        // is encoded as `153` (HEVC convention: level × 30, so 5.1 → 153);
        // covers up to 4K@60 which is also the upper bound of what
        // desktop HW HEVC encoders emit.
        codec = 'hev1.1.6.L153.B0';
        break;
      case CODEC_AV1:
        // av01.<profile>.<level><tier>.<bit_depth>
        // Profile 0 = Main, level 4.0 is encoded as the two-digit `08`
        // (AV1 convention: 5.2 → 13, 4.0 → 8 → padded "08"), tier `M`
        // = Main, 8-bit colour depth.  Covers 4K@30 / 1080p@120 —
        // desktop streaming below 4K@60 fits comfortably; AMF and
        // SVT-AV1 emit Main profile by default.
        codec = 'av01.0.08M.08';
        break;
      case CODEC_H264:
      default: {
        const c = h264CodecStringFromKeyFrame(keyFrame);
        if (!c) {
          console.warn('H.264 key frame missing SPS – cannot configure decoder');
          return;
        }
        codec = c;
        break;
      }
    }

    // Close any previous decoder instance.
    if (this._decoder && this._decoder.state !== 'closed') {
      try { this._decoder.close(); } catch (_) { /* ignore */ }
    }

    this._decoder = new VideoDecoder({
      output: (frame) => {
        this._decodeOut++;
        // Diagnostic: log the first two decoded frames so an operator
        // can correlate "encoder-reader: emitted frame #N" / "ws-sender:
        // shipped frame #N" / "MSG_VIDEO_FRAME #N received" with
        // "VideoDecoder produced frame #N".  A silent gap here means
        // WebCodecs accepted the chunks but produced no output (codec
        // mismatch, hardware fallback, etc.).
        if (this._decodeOut <= 2) {
          console.log(
            `VideoDecoder produced frame #${this._decodeOut}: ` +
            `${frame.codedWidth}x${frame.codedHeight}, ts=${frame.timestamp}us`,
          );
        }
        this._onFrame(frame);
      },
      error: (e) => {
        this._errors++;
        this._lastError = (e && e.message) ? e.message : String(e);
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
   * Decode one access unit.  The data is passed verbatim to WebCodecs
   * — no Annex-B → AVCC / HVCC / AV1C conversion is performed.
   * @param {Uint8Array} data – complete codec access unit
   * @param {boolean} isKeyFrame
   * @param {number} timestampUs – microsecond timestamp from server
   */
  decode(data, isKeyFrame, timestampUs) {
    // If the decoder was closed (e.g. after an error), reset and wait
    // for the next key-frame to reconfigure.
    if (this._decoder && this._decoder.state === 'closed') {
      this._configured = false;
    }

    if (!this._configured) {
      if (!isKeyFrame) {
        this._droppedNoConfig++;
        return; // need a key-frame first
      }
      this._configureFromKeyFrame(data);
      if (!this._configured) return;
    }

    // When frames arrive in bursts (common with SSL-inspecting proxies
    // like Netskope that buffer data), the decoder queue can grow
    // unbounded.  Drop non-key delta frames to prevent overload; always
    // accept key-frames so the decoder can resynchronise.
    //
    // The threshold adapts to the current network RTT: a healthy 10 ms
    // link can absorb a small queue without adding visible lag, whereas
    // a 200 ms link should drop aggressively.  Caller updates RTT via
    // `setLatencyMs()`; falls back to a conservative magic number of 3
    // if no RTT samples have been recorded yet.
    if (!isKeyFrame && this._decoder.decodeQueueSize > this._dropThreshold()) {
      this._droppedQueueFull++;
      return;
    }

    const chunk = new EncodedVideoChunk({
      type: isKeyFrame ? 'key' : 'delta',
      timestamp: timestampUs, // µs
      data,
    });

    try {
      this._decoder.decode(chunk);
      this._decodeIn++;
    } catch (e) {
      this._errors++;
      this._lastError = (e && e.message) ? e.message : String(e);
      console.error('decode() threw:', e);
      this._configured = false;
    }
  }

  /**
   * Update the most recent measured one-way latency (ms).  Used to
   * adapt the decoder-queue drop threshold so a fast link allows a
   * slightly deeper queue (smoother) and a slow link drops earlier
   * (lower latency).  Call from the Pong handler.
   * @param {number} oneWayMs
   */
  setLatencyMs(oneWayMs) {
    if (Number.isFinite(oneWayMs) && oneWayMs >= 0) {
      this._latencyMs = oneWayMs;
    }
  }

  /**
   * Compute the adaptive drop threshold from the most recent latency
   * sample.  Bands chosen empirically:
   *   * `< 20 ms` (LAN): allow up to 5 queued frames so brief jitter
   *     never causes a drop — at this latency 5 frames ≈ 80 ms of
   *     buffer which is invisible to the user.
   *   * `20–60 ms` (typical WAN): 3 frames ≈ one half-second of buffer
   *     at 60 fps; balances smoothness against motion-to-photon lag.
   *   * `≥ 60 ms` (slow / congested): drop earlier (2 frames) because
   *     each queued frame becomes a *visible* lag spike.
   * The numbers are heuristics — tweak if profiling on a specific
   * link suggests otherwise.
   * @private
   */
  _dropThreshold() {
    const ms = this._latencyMs;
    if (ms == null) return 3;        // no measurement yet
    if (ms < 20)  return 5;          // LAN
    if (ms < 60)  return 3;          // typical WAN
    return 2;                         // slow / congested
  }

  close() {
    if (this._decoder && this._decoder.state !== 'closed') {
      this._decoder.close();
    }
  }
}

/**
 * Backwards-compatible alias for callers that still import the old
 * H.264-only class name.
 * @deprecated Use `VideoStreamDecoder` and `setCodec()`.
 */
export const H264Decoder = VideoStreamDecoder;

// ---------------------------------------------------------------------------
// H.264 codec-string derivation
// ---------------------------------------------------------------------------

/**
 * Build an `avc1.PPCCLL` codec string from the SPS NAL unit found in
 * an Annex-B key-frame access unit, or `null` if no SPS is present.
 * @param {Uint8Array} keyFrame
 * @returns {string|null}
 */
function h264CodecStringFromKeyFrame(keyFrame) {
  const nals = parseAnnexB(keyFrame);
  for (const nal of nals) {
    if (nal.length > 3 && (nal[0] & 0x1f) === 7) {
      // SPS: byte 0 is NAL header, bytes 1..3 are profile_idc /
      // constraint flags / level_idc.
      return `avc1.${hex(nal[1])}${hex(nal[2])}${hex(nal[3])}`;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Annex-B parsing utilities (used by H.264 SPS extraction)
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

/** @param {number} id @returns {string} */
function codecName(id) {
  switch (id) {
    case CODEC_H264: return 'H.264';
    case CODEC_HEVC: return 'HEVC';
    case CODEC_AV1:  return 'AV1';
    default:         return `Unknown(${id})`;
  }
}
