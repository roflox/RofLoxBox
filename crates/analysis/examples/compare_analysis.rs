//! Compares deckhand's beat grids and keys with rekordbox's, as imported
//! into a collection (read only): BPM, phase, downbeat and key.
//!
//! `cargo run --release -p deckhand-analysis --example compare_analysis -- collection.json [tracks]`

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use deckhand_analysis::beats::{DEFAULT_RANGE, beat_grid};
use deckhand_analysis::key::{Key, detect_key};
use deckhand_library::Collection;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let collection = Collection::open(Path::new(&args.next().expect("collection.json")))?;
    let limit: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
    // Tracks with one rekordbox tempo marker and their audio at hand.
    let tracks: Vec<_> = collection
        .tracks
        .iter()
        .filter(|t| t.tempo.len() == 1 && t.tempo[0].bpm > 0.0 && t.path.is_file())
        .take(limit)
        .collect();
    let rows = Mutex::new(Vec::new());
    let next = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for _ in 0..std::thread::available_parallelism().map_or(4, |n| n.get()) {
            scope.spawn(|| {
                loop {
                    let i = {
                        let mut n = next.lock().expect("a worker panicked");
                        *n += 1;
                        *n - 1
                    };
                    let Some(track) = tracks.get(i) else { break };
                    let Ok(audio) = deckhand_audio::decode_file(&track.path) else { continue };
                    let grid = beat_grid(&audio.samples, audio.sample_rate, DEFAULT_RANGE);
                    let key = detect_key(&audio.samples, audio.sample_rate);
                    rows.lock().expect("a worker panicked").push((track, grid, key));
                }
            });
        }
    });
    let mut rows = rows.into_inner().expect("a worker panicked");
    rows.sort_by_key(|r| r.0.id);

    let (mut same_bpm, mut octave, mut phase_ok, mut downbeat_ok, mut errors) =
        (0, 0, 0, 0, Vec::new());
    let mut keys = [0usize; 5];
    for (track, grid, key) in &rows {
        // Keys: exact, relative major/minor, a fifth away, parallel, other.
        let truth_key = track.key.as_deref().and_then(parse_key);
        if let (Some(t), Some(k)) = (truth_key, key) {
            keys[relation(t, *k)] += 1;
        }
        let truth = &track.tempo[0];
        let beat = 60.0 / truth.bpm;
        let key_text = match (truth_key, key) {
            (Some(t), Some(k)) => format!("key {} vs {}", k.camelot(), t.camelot()),
            _ => String::new(),
        };
        let Some(grid) = grid else {
            println!("{:<40.40} {:>7.2}  no grid", track.title, truth.bpm);
            continue;
        };
        let bpm_ok = (grid.bpm - truth.bpm).abs() < 0.015;
        let ratio = grid.bpm / truth.bpm;
        if !bpm_ok && [0.5, 2.0, 2.0 / 3.0, 1.5].iter().any(|r| (ratio - r).abs() < 0.01) {
            octave += 1;
        }
        let wrap = |x: f64, span: f64| (x + span / 2.0).rem_euclid(span) - span / 2.0;
        let error = wrap(grid.first_beat - truth.start, beat);
        // Downbeats: where beat 1 falls, compared within a bar.
        let truth_one = truth.start - f64::from(truth.beat.clamp(1, 4) - 1) * beat;
        let ours_one = grid.first_beat - f64::from(grid.first_number - 1) * beat;
        let bar_shift = (wrap(ours_one - truth_one, 4.0 * beat) / beat).round() as i64;
        if bpm_ok {
            same_bpm += 1;
            errors.push(error);
            if error.abs() < 0.010 {
                phase_ok += 1;
                if bar_shift.rem_euclid(4) == 0 {
                    downbeat_ok += 1;
                }
            }
        }
        println!(
            "{:<40.40} {:>7.2} {:>7.2}  phase {:>+6.1} ms  bar {:>+2}  {key_text}",
            track.title,
            truth.bpm,
            grid.bpm,
            error * 1000.0,
            bar_shift
        );
    }
    errors.sort_by(f64::total_cmp);
    let n = rows.len().max(1);
    let pct = |c: usize, of: usize| 100.0 * c as f64 / of.max(1) as f64;
    println!(
        "\n{} tracks: BPM {:.1}% ({} half/double/thirds), phase within 10 ms {:.1}% of right BPMs \
         (median error {:+.1} ms), downbeat {:.1}% of those",
        rows.len(),
        pct(same_bpm, n),
        octave,
        pct(phase_ok, same_bpm),
        errors.get(errors.len() / 2).copied().unwrap_or(0.0) * 1000.0,
        pct(downbeat_ok, phase_ok),
    );
    let all: usize = keys.iter().sum();
    let majors = rows
        .iter()
        .filter(|r| r.0.key.as_deref().and_then(parse_key).is_some_and(|k| !k.minor))
        .count();
    let majors_right = rows
        .iter()
        .filter(|r| {
            let t = r.0.key.as_deref().and_then(parse_key);
            t.is_some_and(|t| !t.minor && r.2 == Some(t))
        })
        .count();
    println!("major-key tracks right: {majors_right} of {majors}");
    println!(
        "{all} keys: exact {:.1}%, relative {:.1}%, fifth {:.1}%, parallel {:.1}%, other {:.1}%",
        pct(keys[0], all),
        pct(keys[1], all),
        pct(keys[2], all),
        pct(keys[3], all),
        pct(keys[4], all)
    );
    Ok(())
}

/// A key written as Camelot (`4A`) or classic (`Fm`, `Ab`).
fn parse_key(text: &str) -> Option<Key> {
    let text = text.trim();
    if let Some(n) = text.strip_suffix(['A', 'B']).and_then(|n| n.parse::<i32>().ok())
        && (1..=12).contains(&n)
    {
        let minor = text.ends_with('A');
        let base = if minor { 9 } else { 0 };
        return Some(Key { pitch: (base + 7 * (n - 8)).rem_euclid(12) as u8, minor });
    }
    let mut chars = text.chars();
    let letter = chars.next()?.to_ascii_uppercase();
    let mut pitch = [9, 11, 0, 2, 4, 5, 7][(letter as u8).checked_sub(b'A')? as usize % 7];
    let rest: String = chars.collect();
    let rest = match rest.chars().next() {
        Some('#') => {
            pitch += 1;
            &rest[1..]
        }
        Some('b') => {
            pitch += 11;
            &rest[1..]
        }
        _ => &rest[..],
    };
    Some(Key { pitch: (pitch % 12) as u8, minor: rest.starts_with('m') })
}

/// 0 same, 1 relative, 2 a fifth up or down (same mode), 3 parallel, 4 other.
fn relation(truth: Key, ours: Key) -> usize {
    let as_major = |k: Key| if k.minor { (k.pitch + 3) % 12 } else { k.pitch };
    let fifth = |a: u8, b: u8| matches!((i32::from(a) - i32::from(b)).rem_euclid(12), 5 | 7);
    if truth == ours {
        0
    } else if truth.minor != ours.minor && as_major(truth) == as_major(ours) {
        1
    } else if truth.minor == ours.minor && fifth(truth.pitch, ours.pitch) {
        2
    } else if truth.pitch == ours.pitch {
        3
    } else {
        4
    }
}
