#![allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]

use super::{Error, Resampler, to_i16};

fn sine(rate: u32, hz: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|n| (2.0 * core::f64::consts::PI * hz * n as f64 / f64::from(rate)).sin() as f32 * 0.5)
        .collect()
}

/// Upward zero crossings per second of a mono signal.
fn pitch(samples: &[f32], rate: u32) -> f64 {
    let crossings = samples
        .windows(2)
        .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
        .count();
    crossings as f64 * f64::from(rate) / samples.len() as f64
}

fn convert(from: u32, to: u32, input: &[f32], chunk: usize) -> Vec<f32> {
    let mut resampler = Resampler::new(from, to, 1).unwrap();
    let mut out = Vec::new();
    for piece in input.chunks(chunk) {
        resampler.process(piece, &mut out);
    }
    resampler.flush(&mut out);
    out
}

#[test]
fn ratios_are_in_lowest_terms() {
    assert_eq!(
        Resampler::new(44_100, 48_000, 2).unwrap().ratio(),
        (160, 147),
        "CD"
    );
    assert_eq!(
        Resampler::new(11_025, 48_000, 1).unwrap().ratio(),
        (640, 147),
        "Doom"
    );
    assert_eq!(
        Resampler::new(48_000, 48_000, 1).unwrap().ratio(),
        (1, 1),
        "none"
    );
}

#[test]
fn nonsense_is_refused() {
    assert_eq!(
        Resampler::new(0, 48_000, 2).err(),
        Some(Error::Rates),
        "zero rate"
    );
    assert_eq!(
        Resampler::new(44_100, 48_000, 0).err(),
        Some(Error::Rates),
        "no channels"
    );
    assert_eq!(
        Resampler::new(48_001, 48_000, 1).err(),
        Some(Error::Rates),
        "too fine"
    );
}

#[test]
fn length_follows_the_ratio() {
    let out = convert(44_100, 48_000, &vec![0.0; 44_100], 1000);
    // One second in, a second out, give or take the filter's half-width.
    assert!(
        (out.len() as i64 - 48_000).abs() <= 20,
        "{} frames",
        out.len()
    );
}

#[test]
fn a_constant_stays_constant() {
    let out = convert(44_100, 48_000, &vec![0.25; 4410], 333);
    for sample in &out[40..out.len() - 40] {
        assert!((sample - 0.25).abs() < 1e-3, "{sample}");
    }
}

#[test]
fn pitch_is_kept() {
    for (from, hz) in [(44_100, 440.0), (11_025, 1000.0), (44_100, 5000.0)] {
        let out = convert(from, 48_000, &sine(from, hz, from as usize), 777);
        let heard = pitch(&out, 48_000);
        assert!(
            (heard - hz).abs() < 3.0,
            "{from} Hz source: {hz} became {heard}"
        );
    }
}

#[test]
fn chunking_does_not_change_the_output() {
    let input = sine(44_100, 997.0, 5000);
    let whole = convert(44_100, 48_000, &input, input.len());
    let pieces = convert(44_100, 48_000, &input, 7);
    assert_eq!(whole, pieces, "the same frames, however the input is cut");
}

#[test]
fn stereo_channels_stay_apart() {
    let mut resampler = Resampler::new(44_100, 48_000, 2).unwrap();
    let input: Vec<f32> = (0..4410).flat_map(|_| [0.5, -0.5]).collect();
    let mut out = Vec::new();
    resampler.process(&input, &mut out);
    for frame in out.as_chunks::<2>().0.iter().skip(40) {
        assert!(
            (frame[0] - 0.5).abs() < 1e-3 && (frame[1] + 0.5).abs() < 1e-3,
            "{frame:?}"
        );
    }
}

/// The power of `hz` in `samples`, by Goertzel's recurrence.
fn power(samples: &[f32], rate: u32, hz: f64) -> f64 {
    let w = 2.0 * core::f64::consts::PI * hz / f64::from(rate);
    let (mut s1, mut s2) = (0.0_f64, 0.0_f64);
    for &x in samples {
        let s0 = f64::from(x) + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
}

#[test]
fn images_are_filtered_out() {
    // Raising 11025 Hz to 48 kHz makes images of a 1 kHz tone at
    // 11025 - 1000 and 11025 + 1000 Hz; the filter must take them to far
    // below the tone.
    let out = convert(11_025, 48_000, &sine(11_025, 1000.0, 11_025), 1000);
    let body = &out[100..out.len() - 100];
    let tone = power(body, 48_000, 1000.0);
    for image in [10_025.0, 12_025.0] {
        let db = 10.0 * (power(body, 48_000, image) / tone).log10();
        assert!(db < -60.0, "image at {image} Hz is {db:.1} dB");
    }
}

#[test]
fn samples_clip_to_sixteen_bits() {
    assert_eq!(
        (to_i16(2.0), to_i16(-2.0), to_i16(0.0)),
        (32767, -32767, 0),
        "clip"
    );
}
