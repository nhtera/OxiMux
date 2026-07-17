//! Linear-interpolation resampler: any device rate → 16 kHz mono f32.
//!
//! The engine (sherpa-onnx) expects 16 kHz. We must NOT ask CoreAudio to
//! capture at 16 kHz directly — on macOS that path can silently yield zeroed
//! samples — so we capture at the device-default rate and downsample here.
//! sherpa also aborts the process if consecutive decode buffers carry
//! different rates, so every buffer handed to the engine passes through this.

/// The one sample rate the offline recognizers accept.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Downmix an interleaved multi-channel f32 buffer to mono by averaging the
/// channels of each frame. A `channels == 1` input is returned as-is.
pub fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    interleaved
        .chunks_exact(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Resample mono f32 `input` from `in_rate` to `out_rate` with linear
/// interpolation. Identity when the rates match or the input is empty. This is
/// the same cheap approach both reference apps use — good enough for speech,
/// and it never allocates unboundedly (output length is proportional to input).
pub fn resample_linear(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if input.is_empty() || in_rate == 0 || out_rate == 0 || in_rate == out_rate {
        return input.to_vec();
    }
    let ratio = out_rate as f64 / in_rate as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        // Position in input-sample space for output sample i.
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = input[idx.min(last)];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_rates_match() {
        let x = vec![0.1, -0.2, 0.3];
        assert_eq!(resample_linear(&x, 16_000, 16_000), x);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(resample_linear(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn downsample_48k_to_16k_thirds_the_length() {
        let input: Vec<f32> = (0..4800).map(|i| (i as f32).sin()).collect();
        let out = resample_linear(&input, 48_000, 16_000);
        // 4800 * (16000/48000) = 1600, within one sample.
        assert!((out.len() as i64 - 1600).abs() <= 1, "len was {}", out.len());
    }

    #[test]
    fn sine_frequency_is_preserved_by_zero_crossings() {
        // A 440 Hz sine at 48 kHz resampled to 16 kHz must keep ~440 Hz, which
        // we check via zero-crossing count (2 per period).
        let freq = 440.0_f32;
        let in_rate = 48_000u32;
        let secs = 1.0_f32;
        let n = (in_rate as f32 * secs) as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / in_rate as f32).sin())
            .collect();
        let out = resample_linear(&input, in_rate, 16_000);
        let crossings = out
            .windows(2)
            .filter(|w| (w[0] <= 0.0 && w[1] > 0.0) || (w[0] >= 0.0 && w[1] < 0.0))
            .count();
        // ~2 crossings per cycle * 440 cycles ≈ 880, allow generous tolerance.
        assert!((crossings as i64 - 880).abs() < 40, "crossings={crossings}");
    }

    #[test]
    fn downmix_stereo_averages_frames() {
        // frames: (1.0, 0.0), (0.0, 1.0) → 0.5, 0.5
        let stereo = vec![1.0, 0.0, 0.0, 1.0];
        assert_eq!(downmix_to_mono(&stereo, 2), vec![0.5, 0.5]);
    }

    #[test]
    fn downmix_mono_passthrough() {
        let mono = vec![0.3, 0.4];
        assert_eq!(downmix_to_mono(&mono, 1), mono);
    }
}
