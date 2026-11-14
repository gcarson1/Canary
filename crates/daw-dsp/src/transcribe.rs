//! Audio to MIDI, offline: a monophonic melody (YIN pitch tracking) or drum hits
//! (transients, sorted into kick, snare and hat by brightness). Runs on a worker
//! thread.

/// A detected note: start and length in frames, MIDI key, velocity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detected {
    pub start: usize,
    pub length: usize,
    pub key: u8,
    pub velocity: u8,
}

const WINDOW: usize = 1024;
const HOP: usize = 512;
/// YIN's dip threshold: lower is stricter about calling a frame pitched.
const THRESHOLD: f32 = 0.15;
/// Frames quieter than this (RMS) are rests.
const GATE: f32 = 0.005;
/// A note must last this many hops (about 32 ms at 48 kHz).
const MIN_HOPS: usize = 3;

/// The pitch in Hz of `frame` (YIN, de Cheveigné & Kawahara 2002), between
/// `lo` and `hi` Hz, or `None` if the frame is not clearly pitched.
fn yin(frame: &[f32], sample_rate: f32, lo: f32, hi: f32) -> Option<f32> {
    let w = frame.len() / 2;
    let tau_min = (sample_rate / hi).max(2.0) as usize;
    let tau_max = ((sample_rate / lo) as usize).min(w - 1);
    if tau_min >= tau_max {
        return None;
    }
    // Difference function, then its cumulative mean normalized form.
    let mut d = vec![0.0f32; tau_max + 1];
    for (tau, slot) in d.iter_mut().enumerate().skip(1) {
        *slot = (0..w).map(|j| (frame[j] - frame[j + tau]).powi(2)).sum();
    }
    let mut running = 0.0;
    let mut cmnd = vec![1.0f32; tau_max + 1];
    for tau in 1..=tau_max {
        running += d[tau];
        cmnd[tau] = if running > 0.0 {
            d[tau] * tau as f32 / running
        } else {
            1.0
        };
    }
    // The first dip under the threshold, followed to its bottom.
    let mut tau = (tau_min..tau_max).find(|&t| cmnd[t] < THRESHOLD)?;
    while tau + 1 < tau_max && cmnd[tau + 1] < cmnd[tau] {
        tau += 1;
    }
    // Parabolic interpolation around the dip for a finer period.
    let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
    let shift = (a - c) / (2.0 * (a - 2.0 * b + c));
    let period = tau as f32
        + if shift.is_finite() {
            shift.clamp(-1.0, 1.0)
        } else {
            0.0
        };
    Some(sample_rate / period)
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn velocity(level: f32) -> u8 {
    // -40 dBFS RMS maps to 1, -6 dBFS and louder to 127.
    let db = 20.0 * level.max(1e-6).log10();
    (((db + 40.0) / 34.0).clamp(0.0, 1.0) * 126.0 + 1.0) as u8
}

/// The notes of a monophonic line (voice, bass, lead) in `samples`.
pub fn melody(samples: &[f32], sample_rate: f32) -> Vec<Detected> {
    let mut keys: Vec<Option<(u8, f32)>> = Vec::new();
    let mut at = 0;
    while at + 2 * WINDOW <= samples.len() {
        let frame = &samples[at..at + 2 * WINDOW];
        let level = rms(&frame[..WINDOW]);
        let key = (level > GATE)
            .then(|| yin(frame, sample_rate, 50.0, 1_500.0))
            .flatten()
            .map(|hz| {
                (69.0 + 12.0 * (hz / 440.0).log2())
                    .round()
                    .clamp(0.0, 127.0) as u8
            })
            .map(|k| (k, level));
        keys.push(key);
        at += HOP;
    }
    // Runs of the same key become notes.
    let mut notes = Vec::new();
    let mut i = 0;
    while i < keys.len() {
        let Some((key, _)) = keys[i] else {
            i += 1;
            continue;
        };
        let mut j = i;
        let mut loudest = 0.0f32;
        while j < keys.len() && keys[j].is_some_and(|(k, _)| k == key) {
            loudest = loudest.max(keys[j].map_or(0.0, |k| k.1));
            j += 1;
        }
        if j - i >= MIN_HOPS {
            notes.push(Detected {
                start: i * HOP,
                length: (j - i) * HOP,
                key,
                velocity: velocity(loudest),
            });
        }
        i = j;
    }
    notes
}

/// Where energy jumps: 5 ms windows whose level is several times the last
/// one's and above the gate, at least 50 ms apart.
fn onsets(samples: &[f32], sample_rate: f32) -> Vec<usize> {
    let hop = ((0.005 * sample_rate) as usize).max(1);
    let gap = (0.05 * sample_rate) as usize;
    let mut out: Vec<usize> = Vec::new();
    let mut prev = 0.0f32;
    for (i, w) in samples.chunks(hop).enumerate() {
        let level = rms(w);
        let at = i * hop;
        if level > GATE && level > 3.0 * prev && out.last().is_none_or(|&l| at >= l + gap) {
            out.push(at);
        }
        prev = level;
    }
    out
}

/// Drum hits in `samples` as kick (36), snare (38) or closed hat (42), by how
/// bright each hit is (zero crossings in its first 20 ms).
pub fn drums(samples: &[f32], sample_rate: f32) -> Vec<Detected> {
    let span = (0.02 * sample_rate) as usize;
    onsets(samples, sample_rate)
        .into_iter()
        .filter_map(|start| {
            let hit = &samples[start..(start + span).min(samples.len())];
            if hit.len() < 8 || rms(hit) < GATE {
                return None;
            }
            let crossings = hit
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count();
            let hz = crossings as f32 / 2.0 / (hit.len() as f32 / sample_rate);
            let key = match hz {
                f if f < 250.0 => 36,
                f if f < 2_500.0 => 38,
                _ => 42,
            };
            Some(Detected {
                start,
                length: (0.1 * sample_rate) as usize,
                key,
                velocity: velocity(rms(hit)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SR: f32 = 48_000.0;

    fn tone(hz: f32, secs: f32) -> Vec<f32> {
        (0..(secs * SR) as usize)
            .map(|i| 0.4 * (TAU * hz * i as f32 / SR).sin())
            .collect()
    }

    #[test]
    fn a_sung_line_becomes_its_notes() {
        // A3, rest, E4.
        let mut line = tone(220.0, 0.5);
        line.extend(vec![0.0; 12_000]);
        line.extend(tone(329.63, 0.5));
        let notes = melody(&line, SR);
        let keys: Vec<u8> = notes.iter().map(|n| n.key).collect();
        assert_eq!(keys, [57, 64], "{notes:?}");
        assert!(notes[0].start < 1_000 && notes[1].start > 30_000);
        assert!(notes.iter().all(|n| n.velocity > 60));
        assert!(
            melody(&vec![0.0; 48_000], SR).is_empty(),
            "silence has no notes"
        );
    }

    #[test]
    fn drum_hits_sort_by_brightness() {
        let mut beat = vec![0.0f32; 48_000];
        let mut hit = |at: usize, hz: f32| {
            for i in 0..2_400 {
                let env = (-(i as f32) / 600.0).exp();
                beat[at + i] += 0.8 * env * (TAU * hz * i as f32 / SR).sin();
            }
        };
        hit(1_000, 60.0);
        hit(13_000, 1_000.0);
        hit(25_000, 8_000.0);
        let keys: Vec<u8> = drums(&beat, SR).iter().map(|d| d.key).collect();
        assert_eq!(keys, [36, 38, 42]);
    }
}
