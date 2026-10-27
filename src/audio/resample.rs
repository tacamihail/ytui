//! Tiny streaming resampler (Catmull-Rom cubic) used when the track's sample
//! rate is not 48 kHz. YouTube Music mostly serves 44.1 kHz AAC, the output
//! device runs at a fixed 48 kHz, so this is the common path.
//!
//! Input is appended to a small buffer that keeps one sample of history before
//! the current fractional position. Samples are only emitted once the cubic
//! needs no future data (`pos + 2 < len`), so processing a stream in arbitrary
//! chunks produces exactly the same output as processing it in one go.

pub struct Resampler {
    step: f64,
    pos: f64,
    buf: Vec<[f32; 2]>,
}

impl Resampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: f64::from(src_rate) / f64::from(dst_rate),
            pos: 0.0,
            buf: Vec::with_capacity(8192),
        }
    }

    /// Appends interleaved stereo samples at the destination rate to `out`.
    pub fn process(&mut self, input: &[[f32; 2]], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        self.buf.extend_from_slice(input);
        let n = self.buf.len();
        if n < 3 {
            return;
        }

        let mut t = self.pos;
        while t + 2.0 < n as f64 {
            let i = t as usize;
            let f = (t - i as f64) as f32;
            let p0 = if i > 0 { self.buf[i - 1] } else { self.buf[0] };
            let p1 = self.buf[i];
            let p2 = self.buf[i + 1];
            let p3 = self.buf[i + 2];
            let (l, r) = cubic(p0, p1, p2, p3, f);
            out.push(l);
            out.push(r);
            t += self.step;
        }

        // Drop everything before the sample preceding the current position.
        let drop = (t as usize).saturating_sub(1);
        if drop > 0 {
            self.buf.drain(..drop);
            t -= drop as f64;
        }
        self.pos = t;
    }
}

fn cubic(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> (f32, f32) {
    let ch = |a: f32, b: f32, c: f32, d: f32| {
        let t2 = t * t;
        let t3 = t2 * t;
        0.5 * ((2.0 * b)
            + (-a + c) * t
            + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
            + (-a + 3.0 * b - 3.0 * c + d) * t3)
    };
    (
        ch(p0[0], p1[0], p2[0], p3[0]),
        ch(p0[1], p1[1], p2[1], p3[1]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, secs: f64) -> Vec<[f32; 2]> {
        let n = (f64::from(rate) * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let s = (2.0 * std::f64::consts::PI * hz * t).sin() as f32 * 0.5;
                [s, s]
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f64 {
        let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        (sum / samples.len() as f64).sqrt()
    }

    #[test]
    fn upsamples_44k_to_48k() {
        let input = sine(44_100, 440.0, 1.0);
        let mut rs = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for chunk in input.chunks(1024) {
            rs.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!(
            (frames as f64 - 48_000.0).abs() < 480.0,
            "expected ~48000 frames, got {frames}"
        );
        let level = rms(&out);
        assert!(
            (level - 0.3535).abs() < 0.02,
            "sine amplitude changed: rms={level}"
        );
        assert!(out.iter().all(|s| s.is_finite()), "non-finite sample");
    }

    #[test]
    fn chunking_does_not_change_output() {
        let input = sine(44_100, 997.0, 0.5);
        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000).process(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut rs = Resampler::new(44_100, 48_000);
        for chunk in input.chunks(997) {
            rs.process(chunk, &mut chunked);
        }

        assert_eq!(whole.len(), chunked.len());
        let max = whole
            .iter()
            .zip(&chunked)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max < 1e-6, "chunk boundaries audible: max diff {max}");
    }

    #[test]
    fn downsamples_48k_to_44k() {
        let input = sine(48_000, 440.0, 1.0);
        let mut rs = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        for chunk in input.chunks(4096) {
            rs.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!(
            (frames as f64 - 44_100.0).abs() < 441.0,
            "expected ~44100 frames, got {frames}"
        );
    }
}
