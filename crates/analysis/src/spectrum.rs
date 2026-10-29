//! Short-time spectra of a mono signal, frame by frame.

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// Calls `each(index, magnitudes)` for every frame of `size` samples taken
/// every `hop` samples (Hann window). `magnitudes` has `size / 2 + 1` bins;
/// the last frames run past the end and see zeros there.
pub fn for_each_frame(mono: &[f32], size: usize, hop: usize, mut each: impl FnMut(usize, &[f32])) {
    let fft: Arc<dyn Fft<f32>> = FftPlanner::new().plan_fft_forward(size);
    let window: Vec<f32> = (0..size)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
        .collect();
    let mut buffer = vec![Complex32::default(); size];
    let mut scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
    let mut magnitudes = vec![0.0f32; size / 2 + 1];
    let frames = mono.len().div_ceil(hop.max(1));
    for index in 0..frames {
        let start = index * hop;
        for (i, slot) in buffer.iter_mut().enumerate() {
            let x = mono.get(start + i).copied().unwrap_or(0.0);
            *slot = Complex32::new(x * window[i], 0.0);
        }
        fft.process_with_scratch(&mut buffer, &mut scratch);
        for (m, c) in magnitudes.iter_mut().zip(&buffer) {
            *m = c.norm();
        }
        each(index, &magnitudes);
    }
}

/// Interleaved stereo mixed to mono.
pub fn mono(samples: &[f32]) -> Vec<f32> {
    samples.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5).collect()
}
