//! Compares deckhand's waveforms with the ones rekordbox stored on a
//! USB export (read only).
//!
//! `cargo run --release -p deckhand-analysis --example compare_waveforms -- /Volumes/STICK [tracks]`

use std::path::Path;

use anyhow::Result;
use deckhand_analysis::waveform::color_detail;
use deckhand_formats::anlz::{AnlzFile, WaveformKind};
use deckhand_formats::pdb::Library;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("export root");
    let limit: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(5);
    let root = Path::new(&root);
    let below = |p: &str| root.join(p.trim_start_matches('/'));
    // (title, audio path, analysis .DAT path) from the export's database, or
    // from the analysis files themselves (an imported folder has no database).
    let only = std::env::var("DECKHAND_ONLY").ok().map(|e| e.to_lowercase());
    let tracks: Vec<(String, String, std::path::PathBuf)> =
        match Library::open(root.join("PIONEER/rekordbox/export.pdb")) {
            Ok(library) => library
                .tracks
                .iter()
                .map(|t| {
                    (t.track.title.clone(), t.track.file_path.clone(), below(&t.track.analyze_path))
                })
                .collect(),
            Err(_) => {
                let mut found = Vec::new();
                let mut dirs = vec![root.join("PIONEER/USBANLZ")];
                while let Some(dir) = dirs.pop() {
                    for entry in std::fs::read_dir(&dir)?.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            dirs.push(path);
                        } else if path.extension().is_some_and(|e| e == "DAT")
                            && let Some(audio) =
                                AnlzFile::open(&path).ok().and_then(|f| f.path().map(str::to_owned))
                        {
                            let title = Path::new(&audio)
                                .file_stem()
                                .map_or(String::new(), |s| s.to_string_lossy().into_owned());
                            found.push((title, audio, path));
                        }
                    }
                }
                found.sort();
                found
            }
        };
    let tracks = tracks.into_iter().filter(|(_, audio, _)| {
        only.as_ref().is_none_or(|ext| audio.to_lowercase().ends_with(ext.as_str()))
            && below(audio).is_file()
    });
    let mut totals = [0usize; 4];
    for (title, file_path, dat) in tracks.take(limit) {
        // DECKHAND_PREVIEW: compare the preview (PWAV) and dots (PWV2).
        if std::env::var_os("DECKHAND_PREVIEW").is_some() {
            let Ok(file) = AnlzFile::open(&dat) else { continue };
            let (Some(pwav), Some(pwv2)) =
                (file.waveform(WaveformKind::Preview), file.waveform(WaveformKind::TinyPreview))
            else {
                continue;
            };
            let Ok(audio) = deckhand_audio::decode_file(&below(&file_path)) else { continue };
            let (ours, dots) =
                deckhand_analysis::preview::preview(&audio.samples, audio.sample_rate);
            let count = |a: &[u8], b: &[u8], f: &dyn Fn(u8) -> u8| {
                a.iter().zip(b).filter(|(x, y)| f(**x) == f(**y)).count()
            };
            let (theirs, their_dots) = (&pwav.data, &pwv2.data);
            let n = ours.len().min(theirs.len());
            let same = count(&ours, theirs, &|v| v);
            let dots_same = count(&dots, their_dots, &|v| v);
            totals[0] += n;
            totals[1] += same;
            totals[2] += dots.len().min(their_dots.len());
            totals[3] += dots_same;
            let bad: Vec<String> = (0..n)
                .filter(|&i| ours[i] != theirs[i])
                .take(3)
                .map(|i| {
                    format!(
                        "#{i} {}/{} vs {}/{}",
                        ours[i] & 31,
                        ours[i] >> 5,
                        theirs[i] & 31,
                        theirs[i] >> 5
                    )
                })
                .collect();
            let bad_dots: Vec<String> = (0..dots.len().min(their_dots.len()))
                .filter(|&i| dots[i] != their_dots[i])
                .take(3)
                .map(|i| format!("#{i} {} vs {}", dots[i], their_dots[i]))
                .collect();
            println!(
                "{title:<36.36} PWAV {}/{} height {} white {}  PWV2 {}/{}  {} | {}",
                same,
                n,
                count(&ours, theirs, &|v| v & 31),
                count(&ours, theirs, &|v| v >> 5),
                dots_same,
                their_dots.len(),
                bad.join(" "),
                bad_dots.join(" ")
            );
            continue;
        }
        let Ok(ext) = AnlzFile::open(dat.with_extension("EXT")) else {
            continue;
        };
        // DECKHAND_MONO: compare the monochrome detail (PWV3) instead.
        if std::env::var_os("DECKHAND_MONO").is_some() {
            let Some(theirs) = ext.waveform(WaveformKind::Detail) else { continue };
            let Ok(audio) = deckhand_audio::decode_file(&below(&file_path)) else { continue };
            let ours = deckhand_analysis::waveform::mono_detail(&audio.samples, audio.sample_rate);
            let theirs: Vec<u8> = theirs.data.clone();
            let n = ours.len().min(theirs.len());
            let same = (0..n).filter(|&i| ours[i] == theirs[i]).count();
            let h = (0..n).filter(|&i| ours[i] & 31 == theirs[i] & 31).count();
            let w = (0..n).filter(|&i| ours[i] >> 5 == theirs[i] >> 5).count();
            let pct = |c: usize| 100.0 * c as f64 / n.max(1) as f64;
            totals[0] += n;
            totals[1] += same;
            let bad: Vec<String> = (0..n)
                .filter(|&i| ours[i] != theirs[i])
                .take(4)
                .map(|i| {
                    format!(
                        "#{i} {}/{} vs {}/{}",
                        ours[i] & 31,
                        ours[i] >> 5,
                        theirs[i] & 31,
                        theirs[i] >> 5
                    )
                })
                .collect();
            println!(
                "{title:<40.40} PWV3 cols {}/{} exact {:.1}% height {:.1}% white {:.1}%  {}",
                ours.len(),
                theirs.len(),
                pct(same),
                pct(h),
                pct(w),
                bad.join(" ")
            );
            continue;
        }
        // DECKHAND_OVERVIEW: compare the colour overview (PWV4) instead.
        if std::env::var_os("DECKHAND_OVERVIEW").is_some() {
            let Some(theirs) = ext.waveform(WaveformKind::ColorPreview) else { continue };
            let Ok(audio) = deckhand_audio::decode_file(&below(&file_path)) else { continue };
            let ours =
                deckhand_analysis::waveform::color_overview(&audio.samples, audio.sample_rate);
            let theirs: Vec<[i8; 6]> =
                theirs.columns().map(|c| std::array::from_fn(|i| c[i] as i8)).collect();
            let n = ours.len().min(theirs.len());
            let same = (0..n).filter(|&i| ours[i] == theirs[i]).count();
            let fields: Vec<String> = (0..6)
                .map(|f| {
                    let m = (0..n).filter(|&i| ours[i][f] == theirs[i][f]).count();
                    format!("{:.0}", 100.0 * m as f64 / n.max(1) as f64)
                })
                .collect();
            totals[0] += n;
            totals[1] += same;
            let bad: Vec<String> = (0..n)
                .filter(|&i| ours[i] != theirs[i])
                .take(5)
                .map(|i| format!("#{i} {:?}/{:?}", ours[i], theirs[i]))
                .collect();
            println!("    {}", bad.join(" "));
            println!(
                "{title:<40.40} PWV4 exact {:.1}%  fields {}  e.g. ours {:?} theirs {:?}",
                100.0 * same as f64 / n.max(1) as f64,
                fields.join("/"),
                ours[n / 2],
                theirs[n / 2]
            );
            continue;
        }
        let Some(theirs) = ext.waveform(WaveformKind::ColorDetail) else { continue };
        let theirs: Vec<u16> = theirs.columns().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let audio = match deckhand_audio::decode_file(&below(&file_path)) {
            Ok(audio) => audio,
            Err(err) => {
                println!("{title}: {err:#}");
                continue;
            }
        };
        // DECKHAND_OFFSET frames of silence first (negative: skip frames).
        let offset: isize =
            std::env::var("DECKHAND_OFFSET").ok().and_then(|o| o.parse().ok()).unwrap_or(0);
        let samples: Vec<f32> = if offset >= 0 {
            std::iter::repeat_n(0.0, 2 * offset as usize)
                .chain(audio.samples.iter().copied())
                .collect()
        } else {
            audio.samples.iter().copied().skip(2 * (-offset) as usize).collect()
        };
        // DECKHAND_TAIL frames of silence at the end (experiment).
        let tail: usize =
            std::env::var("DECKHAND_TAIL").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let samples: Vec<f32> =
            samples.into_iter().chain(std::iter::repeat_n(0.0, 2 * tail)).collect();
        // DECKHAND_CUT frames dropped at the end (experiment).
        let cut: usize =
            std::env::var("DECKHAND_CUT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let samples: Vec<f32> = samples[..samples.len().saturating_sub(2 * cut)].to_vec();
        // DECKHAND_DECIMATE=k keeps every k-th frame (experiment).
        let k: usize =
            std::env::var("DECKHAND_DECIMATE").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        let samples: Vec<f32> = samples.chunks(2).step_by(k).flatten().copied().collect();
        let ours = color_detail(&samples, audio.sample_rate / k as u32);
        // Decoders differ in how much silence they put first: find the shift
        // (in columns) where the heights agree best, and compare there.
        let height_of = |v: u16| (v >> 2) & 0x1f;
        let agree = |shift: isize| {
            (0..ours.len() as isize)
                .filter(|&i| {
                    let j = i + shift;
                    j >= 0
                        && (j as usize) < theirs.len()
                        && height_of(ours[i as usize]) == height_of(theirs[j as usize])
                })
                .count()
        };
        let shift = (-20..=20).max_by_key(|&s| agree(s)).unwrap_or(0);
        let theirs: Vec<u16> = theirs.iter().skip(shift.max(0) as usize).copied().collect();
        let ours: Vec<u16> = ours.iter().skip((-shift).max(0) as usize).copied().collect();
        println!("  best shift {shift} columns");
        let n = ours.len().min(theirs.len());
        if let Ok(dump) = std::env::var("DECKHAND_DUMP") {
            let text: String = (0..n).map(|i| format!("{} {}\n", ours[i], theirs[i])).collect();
            std::fs::write(format!("{dump}-{title}.txt"), text)?;
        }
        let height = |v: u16| (v >> 2) & 0x1f;
        let colour = |v: u16| v >> 7;
        let same = (0..n).filter(|&i| ours[i] == theirs[i]).count();
        let same_height = (0..n).filter(|&i| height(ours[i]) == height(theirs[i])).count();
        let near_height =
            (0..n).filter(|&i| height(ours[i]).abs_diff(height(theirs[i])) <= 1).count();
        let same_colour = (0..n).filter(|&i| colour(ours[i]) == colour(theirs[i])).count();
        for (total, count) in totals.iter_mut().zip([n, same, same_height, same_colour]) {
            *total += count;
        }
        let pct = |c: usize| 100.0 * c as f64 / n.max(1) as f64;
        println!(
            "{:<40.40} {} Hz, columns ours {} theirs {}: exact {:.1}%, height {:.1}% (±1 {:.1}%), colour {:.1}%",
            title,
            audio.sample_rate,
            ours.len(),
            theirs.len(),
            pct(same),
            pct(same_height),
            pct(near_height),
            pct(same_colour),
        );
        // Channel against channel (r, g, b), and how heights differ.
        let channel = |v: u16, c: usize| (v >> (13 - 3 * c)) & 7;
        let mut grid = String::new();
        for c in 0..3 {
            for d in 0..3 {
                let m = (0..n).filter(|&i| channel(ours[i], c) == channel(theirs[i], d)).count();
                grid += &format!("{:5.1}", pct(m));
            }
            grid += " |";
        }
        let mut diff = [0usize; 7];
        for i in 0..n {
            let d = height(ours[i]) as i32 - height(theirs[i]) as i32;
            diff[(d.clamp(-3, 3) + 3) as usize] += 1;
        }
        let diff: Vec<String> = diff.iter().map(|&c| format!("{:.0}", pct(c))).collect();
        println!("    ours r/g/b vs theirs r g b: {grid}  height diff -3..3: {}", diff.join(" "));
        // The first differing columns, to see what differs.
        let diffs: Vec<String> = (0..n)
            .filter(|&i| ours[i] != theirs[i])
            .take(4)
            .map(|i| {
                let parts = |v: u16| (v >> 13, (v >> 10) & 7, (v >> 7) & 7, height(v));
                format!("#{i} ours {:?} theirs {:?}", parts(ours[i]), parts(theirs[i]))
            })
            .collect();
        if !diffs.is_empty() {
            println!("    {}", diffs.join(" | "));
        }
    }
    let [n, same, h, c] = totals;
    if std::env::var_os("DECKHAND_PREVIEW").is_some() {
        let pct = |a: usize, b: usize| 100.0 * a as f64 / b.max(1) as f64;
        println!("all: PWAV {:.1}%, PWV2 {:.1}%", pct(same, n), pct(c, h));
        return Ok(());
    }
    let pct = |v: usize| 100.0 * v as f64 / n.max(1) as f64;
    println!("all: exact {:.1}%, height {:.1}%, colour {:.1}%", pct(same), pct(h), pct(c));
    Ok(())
}
