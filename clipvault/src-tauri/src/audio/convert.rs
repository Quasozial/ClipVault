//! Fallback conversion to the pipe format (interleaved f32, 48 kHz, stereo).
//!
//! Normally WASAPI converts for us (`AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`), so
//! this only runs when a device refuses that. Quality is deliberately simple:
//! linear interpolation and a plain channel map, good enough for a fallback.

use super::pipes::{CHANNELS, SAMPLE_RATE};

/// Raw sample encoding of a device buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    I16,
    /// 24-bit packed in 3 bytes.
    I24,
    /// 32-bit container (also 24-in-32, which is left-justified).
    I32,
}

impl SampleFormat {
    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::F32 | SampleFormat::I32 => 4,
            SampleFormat::I16 => 2,
            SampleFormat::I24 => 3,
        }
    }
}

/// Decode little-endian device bytes into f32 samples (-1..1), appending to `out`.
/// A trailing partial sample is ignored.
pub fn decode(bytes: &[u8], fmt: SampleFormat, out: &mut Vec<f32>) {
    let n = fmt.bytes();
    out.reserve(bytes.len() / n);
    for b in bytes.chunks_exact(n) {
        out.push(match fmt {
            SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            SampleFormat::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0,
            SampleFormat::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            SampleFormat::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        });
    }
}

/// Map one interleaved frame to stereo: mono is duplicated, otherwise the first
/// two channels (front left/right in every WAVEFORMATEXTENSIBLE layout) are kept.
fn to_stereo(frame: &[f32]) -> [f32; 2] {
    match frame.len() {
        0 => [0.0, 0.0],
        1 => [frame[0], frame[0]],
        _ => [frame[0], frame[1]],
    }
}

/// Streaming converter from `in_rate` Hz / `in_ch` channels f32 to 48 kHz stereo.
/// Positions are exact rationals (units of 1/48000 input frame), so the output
/// does not depend on how the input is split into blocks.
pub struct Converter {
    in_ch: usize,
    in_rate: u64,
    /// Position of the next output frame relative to `prev`, in 1/48000 input frames.
    pos: u64,
    /// Last input frame of the previous block (`None` before the first frame).
    prev: Option<[f32; 2]>,
    scratch: Vec<[f32; 2]>,
}

const DEN: u64 = SAMPLE_RATE as u64;

impl Converter {
    pub fn new(in_rate: u32, in_ch: usize) -> Converter {
        Converter { in_ch: in_ch.max(1), in_rate: in_rate.max(1) as u64, pos: 0, prev: None, scratch: Vec::new() }
    }

    /// Convert one block of interleaved input; output is appended to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let mut frames = input.chunks_exact(self.in_ch).map(to_stereo);
        if self.in_rate == DEN {
            out.extend(frames.flatten());
            return;
        }
        // scratch[0] is the previous block's last frame, so interpolation spans blocks.
        let Some(first) = self.prev.or_else(|| frames.next()) else { return };
        self.scratch.clear();
        self.scratch.push(first);
        self.scratch.extend(frames);
        let last = (self.scratch.len() - 1) as u64;
        debug_assert_eq!(CHANNELS, 2);
        while self.pos < last * DEN {
            let i = (self.pos / DEN) as usize;
            let t = (self.pos % DEN) as f32 / DEN as f32;
            let (a, b) = (self.scratch[i], self.scratch[i + 1]);
            out.push(a[0] + (b[0] - a[0]) * t);
            out.push(a[1] + (b[1] - a[1]) * t);
            self.pos += self.in_rate;
        }
        self.pos -= last * DEN;
        self.prev = Some(self.scratch[last as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(v: &[f32]) -> usize {
        v.len() / CHANNELS
    }

    #[test]
    fn passthrough_when_format_matches() {
        let mut c = Converter::new(48_000, 2);
        let input: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        let mut out = Vec::new();
        c.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn mono_is_duplicated() {
        let mut c = Converter::new(48_000, 1);
        let mut out = Vec::new();
        c.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn surround_downmix_keeps_front_left_right() {
        // FL FR FC LFE BL BR
        let mut c = Converter::new(48_000, 6);
        let mut out = Vec::new();
        c.process(&[0.5, -0.5, 0.0, 0.9, 0.0, 0.0], &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 1e-6 && (out[1] + 0.5).abs() < 1e-6, "{out:?}");
    }

    #[test]
    fn resample_44100_to_48000_rate_and_level() {
        let mut c = Converter::new(44_100, 2);
        let mut out = Vec::new();
        // 1 s of constant 0.25 in 441-frame (10 ms) blocks.
        for _ in 0..100 {
            c.process(&vec![0.25; 441 * 2], &mut out);
        }
        let n = frames(&out) as i64;
        assert!((n - 48_000).abs() <= 2, "{n} frames");
        assert!(out.iter().all(|s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn resample_is_continuous_across_blocks() {
        let sine: Vec<f32> = (0..4410)
            .flat_map(|i| {
                let v = (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 44_100.0).sin();
                [v, v]
            })
            .collect();
        let mut whole = Vec::new();
        Converter::new(44_100, 2).process(&sine, &mut whole);
        let mut chunked = Vec::new();
        let mut c = Converter::new(44_100, 2);
        for block in sine.chunks(2 * 137) {
            c.process(block, &mut chunked);
        }
        assert_eq!(whole.len(), chunked.len());
        for (a, b) in whole.iter().zip(&chunked) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn downsample_96000_halves_frames() {
        let mut c = Converter::new(96_000, 2);
        let mut out = Vec::new();
        c.process(&vec![0.0; 9600 * 2], &mut out);
        let n = frames(&out) as i64;
        assert!((n - 4800).abs() <= 1, "{n}");
    }

    #[test]
    fn decode_integer_and_float() {
        let mut out = Vec::new();
        decode(&i16::MIN.to_le_bytes(), SampleFormat::I16, &mut out);
        decode(&(i32::MAX / 2 + 1).to_le_bytes(), SampleFormat::I32, &mut out);
        decode(&0.75f32.to_le_bytes(), SampleFormat::F32, &mut out);
        // 24-bit packed: 0x400000 = +0.5
        decode(&[0x00, 0x00, 0x40], SampleFormat::I24, &mut out);
        let want = [-1.0, 0.5, 0.75, 0.5];
        for (a, b) in out.iter().zip(want) {
            assert!((a - b).abs() < 1e-6, "{out:?}");
        }
        assert_eq!(out.len(), 4);
    }
}
