//! Codec-specific access-unit splitters.
//!
//! Implementations buffer the encoder output stream and return one
//! [`EncodedFrame`] per complete access unit (one per displayed picture).
//! Extracted into their own module from the original `encoder.rs` so each
//! splitter can be unit-tested (and fuzz-tested) in isolation.

use super::EncodedFrame;

/// Codec-agnostic access-unit splitter trait.
pub trait FrameSplitter: Send {
    /// Append `data` to the internal buffer and append any complete
    /// access units detected to `out`.  Implementations must reserve
    /// `EncodedFrame::HEADER_LEN` bytes at the start of each emitted
    /// frame's `data` so the WebSocket sender can fill in the wire
    /// header in place.
    ///
    /// `out` is owned by the caller so the same `Vec` can be reused
    /// across the encoder-reader's hot loop.
    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>);

    /// Number of bytes currently held in the splitter's internal buffer.
    /// Used purely for diagnostic logging.
    fn buffered_bytes(&self) -> usize;

    /// Reset all internal state.  Called after an encoder restart so a
    /// fresh splitter does not see stale partial bytes from the previous
    /// FFmpeg process.
    fn reset(&mut self);
}

// ── H.264 splitter ─────────────────────────────────────────────────

/// Splits an H.264 Annex-B byte-stream into access units by detecting
/// AUD NAL units (`nal_unit_type == 9`).
///
/// Uses an index-based read cursor (`read_pos`) rather than draining the
/// front of `buf` after every emitted access unit — `Vec::drain(..n)`
/// memmoves the residual bytes (typically a partial next frame) to the
/// vector's start, and at 60 fps with 100–500 KB IDRs that backlog of
/// memcpy contends for the same memory bandwidth as the encoder-reader
/// thread that is concurrently draining FFmpeg's stdout.  Compacting is
/// instead performed lazily, only when the consumed prefix exceeds half
/// of the buffer length, so the worst-case compaction work is bounded
/// by the live (still-needed) bytes.
pub struct H264Splitter {
    buf: Vec<u8>,
    /// First index in `buf` that has not yet been emitted as part of an
    /// access unit.  All bytes in `buf[..read_pos]` are dead.
    read_pos: usize,
}

impl Default for H264Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Splitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    /// Compact `buf` if more than half of its content is dead bytes.
    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    /// Find the byte offset of the next AUD start-code at or after `from`.
    fn find_aud(&self, from: usize) -> Option<usize> {
        let d = &self.buf;
        let mut i = from;
        while i + 3 < d.len() {
            let rel = memchr::memchr(0, &d[i..d.len().saturating_sub(3)])?;
            i += rel;
            if i + 3 >= d.len() {
                return None;
            }
            if d[i + 1] == 0 {
                // 4-byte start-code: 00 00 00 01 <nal type 9>
                if i + 4 < d.len()
                    && d[i + 2] == 0
                    && d[i + 3] == 1
                    && (d[i + 4] & 0x1F) == 9
                {
                    return Some(i);
                }
                // 3-byte start-code: 00 00 01 <nal type 9>
                if d[i + 2] == 1 && (d[i + 3] & 0x1F) == 9 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    /// Returns true if the access-unit data contains an IDR slice (type 5).
    fn contains_idr(data: &[u8]) -> bool {
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                let nal_idx = if data[i + 2] == 1 {
                    i + 3
                } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if nal_idx < data.len() && (data[nal_idx] & 0x1F) == 5 {
                    return true;
                }
                i = nal_idx;
            } else {
                i += 1;
            }
        }
        false
    }
}

impl FrameSplitter for H264Splitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);

        let mut search = self.read_pos;
        let mut prev_aud: Option<usize> = None;

        while search + 3 < self.buf.len() {
            if let Some(aud_pos) = self.find_aud(search) {
                if let Some(start) = prev_aud {
                    let au_slice = &self.buf[start..aud_pos];
                    if !au_slice.is_empty() {
                        let is_key = Self::contains_idr(au_slice);
                        let mut data = Vec::with_capacity(
                            EncodedFrame::HEADER_LEN + au_slice.len(),
                        );
                        data.resize(EncodedFrame::HEADER_LEN, 0);
                        data.extend_from_slice(au_slice);
                        out.push(EncodedFrame { data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        if let Some(start) = prev_aud {
            self.read_pos = start;
            self.compact();
        }
    }
}

// ── HEVC splitter ──────────────────────────────────────────────────

/// Splits an HEVC Annex-B byte-stream into access units by detecting
/// AUD NAL units (`nal_unit_type == 35`).
pub struct HevcSplitter {
    buf: Vec<u8>,
    read_pos: usize,
}

impl Default for HevcSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcSplitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    fn hevc_nal_type(b0: u8) -> u8 {
        (b0 >> 1) & 0x3F
    }

    fn find_aud(&self, from: usize) -> Option<usize> {
        let d = &self.buf;
        let mut i = from;
        while i + 3 < d.len() {
            let rel = memchr::memchr(0, &d[i..d.len().saturating_sub(3)])?;
            i += rel;
            if i + 3 >= d.len() {
                return None;
            }
            if d[i + 1] == 0 {
                if i + 4 < d.len()
                    && d[i + 2] == 0
                    && d[i + 3] == 1
                    && Self::hevc_nal_type(d[i + 4]) == 35
                {
                    return Some(i);
                }
                if d[i + 2] == 1 && Self::hevc_nal_type(d[i + 3]) == 35 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    fn contains_keyframe(data: &[u8]) -> bool {
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 {
                let nal_idx = if data[i + 2] == 1 {
                    i + 3
                } else if i + 4 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if nal_idx < data.len() {
                    let t = Self::hevc_nal_type(data[nal_idx]);
                    if (16..=23).contains(&t) {
                        return true;
                    }
                }
                i = nal_idx;
            } else {
                i += 1;
            }
        }
        false
    }
}

impl FrameSplitter for HevcSplitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);

        let mut search = self.read_pos;
        let mut prev_aud: Option<usize> = None;

        while search + 3 < self.buf.len() {
            if let Some(aud_pos) = self.find_aud(search) {
                if let Some(start) = prev_aud {
                    let au_slice = &self.buf[start..aud_pos];
                    if !au_slice.is_empty() {
                        let is_key = Self::contains_keyframe(au_slice);
                        let mut data = Vec::with_capacity(
                            EncodedFrame::HEADER_LEN + au_slice.len(),
                        );
                        data.resize(EncodedFrame::HEADER_LEN, 0);
                        data.extend_from_slice(au_slice);
                        out.push(EncodedFrame { data, is_keyframe: is_key });
                    }
                }
                prev_aud = Some(aud_pos);
                search = aud_pos + 5;
            } else {
                break;
            }
        }

        if let Some(start) = prev_aud {
            self.read_pos = start;
            self.compact();
        }
    }
}

// ── AV1 splitter (Low-Overhead Bitstream Format) ───────────────────

/// AV1 OBU types we care about.  Defined by the AV1 spec, section 6.2.1.
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
#[allow(dead_code)]
const OBU_FRAME_HEADER: u8 = 3;
#[allow(dead_code)]
const OBU_FRAME: u8 = 6;

/// Splits an AV1 Low-Overhead Bitstream Format stream into access
/// units by detecting Temporal Delimiter OBUs (type 2).
pub struct Av1Splitter {
    buf: Vec<u8>,
    read_pos: usize,
}

impl Default for Av1Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Av1Splitter {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(256 * 1024), read_pos: 0 }
    }

    fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }
        let live = self.buf.len() - self.read_pos;
        if self.read_pos > live {
            self.buf.copy_within(self.read_pos.., 0);
            self.buf.truncate(live);
            self.read_pos = 0;
        }
    }

    /// Parse a LEB128 (max 8 bytes per AV1 spec §4.10.5).
    /// Returns `(value, byte_count)` or `None` if truncated/invalid.
    pub(crate) fn read_leb128(buf: &[u8]) -> Option<(u64, usize)> {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        for (i, &b) in buf.iter().enumerate().take(8) {
            value |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Some((value, i + 1));
            }
            shift += 7;
        }
        None
    }

    fn extract_units(&mut self, out: &mut Vec<EncodedFrame>) {
        let mut td_starts: Vec<usize> = Vec::new();
        let mut keyframe_units: Vec<bool> = Vec::new();
        let mut current_is_key = false;

        let mut pos = self.read_pos;
        let mut last_complete_end = self.read_pos;

        while pos < self.buf.len() {
            let header = self.buf[pos];
            let obu_type = (header >> 3) & 0x0F;
            let ext_flag = (header & 0x04) != 0;
            let size_flag = (header & 0x02) != 0;

            let mut p = pos + 1;
            if ext_flag {
                if p >= self.buf.len() {
                    break;
                }
                p += 1;
            }

            let payload_len = if size_flag {
                if p >= self.buf.len() {
                    break;
                }
                let (len, n) = match Self::read_leb128(&self.buf[p..]) {
                    Some(v) => v,
                    None => break,
                };
                p += n;
                len as usize
            } else {
                self.buf.len().saturating_sub(p)
            };

            let payload_end = p.checked_add(payload_len);
            let payload_end = match payload_end {
                Some(v) if v <= self.buf.len() => v,
                _ => break,
            };

            if obu_type == OBU_SEQUENCE_HEADER {
                current_is_key = true;
            }

            if obu_type == OBU_TEMPORAL_DELIMITER {
                td_starts.push(pos);
                keyframe_units.push(false);
                current_is_key = false;
            } else if let Some(last) = keyframe_units.last_mut() {
                if current_is_key {
                    *last = true;
                }
            }

            pos = payload_end;
            last_complete_end = pos;
        }

        if td_starts.len() >= 2 {
            for w in 0..td_starts.len() - 1 {
                let start = td_starts[w];
                let end = td_starts[w + 1];
                let au_slice = &self.buf[start..end];
                if !au_slice.is_empty() {
                    let mut data = Vec::with_capacity(
                        EncodedFrame::HEADER_LEN + au_slice.len(),
                    );
                    data.resize(EncodedFrame::HEADER_LEN, 0);
                    data.extend_from_slice(au_slice);
                    out.push(EncodedFrame {
                        data,
                        is_keyframe: keyframe_units[w],
                    });
                }
            }
            self.read_pos = *td_starts.last().unwrap();
            self.compact();
        } else if last_complete_end > self.read_pos && td_starts.is_empty() {
            self.read_pos = last_complete_end;
            self.compact();
        }
    }
}

impl FrameSplitter for Av1Splitter {
    fn buffered_bytes(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<EncodedFrame>) {
        self.buf.extend_from_slice(data);
        self.extract_units(out);
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn aud_au(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09]; // AUD
        v.extend_from_slice(payload);
        v
    }

    fn push_collect(s: &mut dyn FrameSplitter, data: &[u8]) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        s.push(data, &mut out);
        out
    }

    #[test]
    fn h264_detects_two_access_units_split_on_aud() {
        let mut det = H264Splitter::new();
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]); // type 1 (P/B)
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb])); // type 5 (IDR)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].is_keyframe);
        assert!(frames[1].is_keyframe);
        for f in &frames {
            assert!(f.data.len() >= EncodedFrame::HEADER_LEN);
            assert!(f.data[..EncodedFrame::HEADER_LEN].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn h264_handles_streaming_chunks_without_losing_data() {
        let mut det = H264Splitter::new();
        let mut bytes = aud_au(&[0x00, 0x00, 0x01, 0x41, 0xaa]);
        bytes.extend_from_slice(&aud_au(&[0x00, 0x00, 0x01, 0x65, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09]);

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(!got[0].is_keyframe);
        assert!(got[1].is_keyframe);
    }

    #[test]
    fn h264_no_aud_means_no_frames_emitted() {
        let mut det = H264Splitter::new();
        let frames = push_collect(&mut det, &[0x00, 0x00, 0x00, 0x01, 0x65, 0xff, 0xff]);
        assert!(frames.is_empty());
    }

    #[test]
    fn h264_lazy_compaction_keeps_buffer_bounded() {
        let mut det = H264Splitter::new();
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x09]; // AUD
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]); // NAL type 1
        au.extend(std::iter::repeat_n(0xaau8, 4096));    // big payload
        let trailing_aud = [0x00, 0x00, 0x00, 0x01, 0x09];

        push_collect(&mut det, &au);

        for _ in 0..100 {
            let mut chunk = trailing_aud.to_vec();
            chunk.extend_from_slice(&au[5..]);
            let frames = push_collect(&mut det, &chunk);
            assert_eq!(frames.len(), 1);
        }

        assert!(
            det.buffered_bytes() < 16 * 1024,
            "buffer grew unboundedly: {} bytes",
            det.buffered_bytes()
        );
    }

    fn hevc_aud_au(payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x46, 0x01];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn hevc_detects_two_access_units() {
        let mut det = HevcSplitter::new();
        let mut bytes = hevc_aud_au(&[0x00, 0x00, 0x01, 0x02, 0x01, 0xaa]);
        bytes.extend_from_slice(&hevc_aud_au(&[0x00, 0x00, 0x01, 0x26, 0x01, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x46, 0x01]);

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].is_keyframe);
        assert!(frames[1].is_keyframe);
    }

    #[test]
    fn hevc_streamed_one_byte_at_a_time() {
        let mut det = HevcSplitter::new();
        let mut bytes = hevc_aud_au(&[0x00, 0x00, 0x01, 0x02, 0x01, 0xaa]);
        bytes.extend_from_slice(&hevc_aud_au(&[0x00, 0x00, 0x01, 0x26, 0x01, 0xbb]));
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x46, 0x01]);

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(!got[0].is_keyframe);
        assert!(got[1].is_keyframe);
    }

    fn av1_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let header = (obu_type & 0x0F) << 3 | 0x02;
        let mut v = vec![header];
        assert!(payload.len() < 128, "test helper only handles small OBUs");
        v.push(payload.len() as u8);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn av1_detects_two_access_units() {
        let mut det = Av1Splitter::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x01]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xAA, 0xBB, 0xCC]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xDD, 0xEE]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));

        let frames = push_collect(&mut det, &bytes);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_keyframe);
        assert!(!frames[1].is_keyframe);
        for f in &frames {
            assert!(f.data.len() >= EncodedFrame::HEADER_LEN);
        }
    }

    #[test]
    fn av1_streamed_one_byte_at_a_time() {
        let mut det = Av1Splitter::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_SEQUENCE_HEADER, &[0x00, 0x01]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xAA, 0xBB]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));
        bytes.extend_from_slice(&av1_obu(OBU_FRAME, &[0xCC]));
        bytes.extend_from_slice(&av1_obu(OBU_TEMPORAL_DELIMITER, &[]));

        let mut got = Vec::new();
        for b in &bytes {
            got.extend(push_collect(&mut det, std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2);
        assert!(got[0].is_keyframe);
        assert!(!got[1].is_keyframe);
    }

    #[test]
    fn av1_leb128_round_trip_small_and_multi_byte() {
        assert_eq!(Av1Splitter::read_leb128(&[0x00]), Some((0, 1)));
        assert_eq!(Av1Splitter::read_leb128(&[0x7F]), Some((127, 1)));
        assert_eq!(Av1Splitter::read_leb128(&[0xC8, 0x01]), Some((200, 2)));
    }

    /// Pseudo-random fuzz: feed a deterministic byte sequence to each
    /// splitter in random-sized chunks. Ensures: no panic, every emitted
    /// frame reserves the wire-protocol header, and the internal buffer
    /// never grows unbounded across 1 MiB of input.
    fn fuzz_one(mut splitter: Box<dyn FrameSplitter>) {
        // Linear-congruential PRNG so this test is deterministic and
        // does not pull in a `rand` dev-dep.
        let mut state: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };

        let total: usize = 1024 * 1024;
        let mut produced = 0usize;
        let mut buf = vec![0u8; 4096];
        let mut frames: Vec<EncodedFrame> = Vec::new();

        while produced < total {
            let n = ((next() as usize) % buf.len()).max(1);
            for b in buf[..n].iter_mut() {
                *b = (next() & 0xFF) as u8;
            }
            frames.clear();
            splitter.push(&buf[..n], &mut frames);
            for f in &frames {
                assert!(
                    f.data.len() >= EncodedFrame::HEADER_LEN,
                    "splitter emitted a frame smaller than HEADER_LEN",
                );
            }
            produced += n;

            // Buffered bytes must remain bounded by ~one frame's worth.
            assert!(
                splitter.buffered_bytes() < 8 * 1024 * 1024,
                "splitter internal buffer grew unboundedly: {} bytes",
                splitter.buffered_bytes(),
            );
        }
    }

    #[test]
    fn h264_fuzz_random_input_no_panic_no_unbounded_growth() {
        fuzz_one(Box::new(H264Splitter::new()));
    }

    #[test]
    fn hevc_fuzz_random_input_no_panic_no_unbounded_growth() {
        fuzz_one(Box::new(HevcSplitter::new()));
    }

    #[test]
    fn av1_fuzz_random_input_no_panic_no_unbounded_growth() {
        fuzz_one(Box::new(Av1Splitter::new()));
    }

    #[test]
    fn reset_clears_internal_buffer() {
        let mut det = H264Splitter::new();
        // Push partial data without a closing AUD so it stays buffered.
        det.push(&[0x00, 0x00, 0x00, 0x01, 0x09, 0xAA, 0xBB], &mut Vec::new());
        assert!(det.buffered_bytes() > 0);
        det.reset();
        assert_eq!(det.buffered_bytes(), 0);
    }
}
