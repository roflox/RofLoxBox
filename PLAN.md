# deckhand — plan

A DJ library and performance application written in Rust that is compatible
with Pioneer DJ hardware (CDJ/XDJ players, USB exports, Pro DJ Link), without
subscriptions, accounts, telemetry or ads. "deckhand" is a working name.

## Ground rules

1. **Own code.** Written from public format documentation and from test
   files: real exports and what the hardware and software do with them.
2. **No third-party assets.** No icons, strings, ML models, skins or
   bundled libraries copied out of the rekordbox app bundle.
3. **Out of scope, permanently:** licensing and plan checks, account
   login (Gigya), product registration, analytics and crash reporting
   (Bugsnag), ads and upsell UI, KUVO. Features that exist only behind
   a paid plan are either left out or built independently; their
   licensing is never bypassed. Encrypted rekordbox databases
   (`master.db`) are not decrypted — library import goes through
   rekordbox XML or USB exports.
4. **Interoperability is verified, not assumed.** Every format reader is
   tested against real exports; every writer is tested by round-trip and,
   at milestones, by loading the result on real hardware.

## Primary references

- Deep Symmetry crate-digger: `export.pdb` and ANLZ format analysis and
  Kaitai specs (https://github.com/Deep-Symmetry/crate-digger)
- Deep Symmetry dysentery: Pro DJ Link protocol analysis
  (https://github.com/Deep-Symmetry/dysentery)
- rekordcrate (Rust, MPL-2.0): readers for pdb / ANLZ / settings; used as a
  test oracle and fixture source (https://github.com/Holzhaus/rekordcrate)
- rekordbox XML format (published by Pioneer DJ)

## Workspace layout

| Crate | Purpose |
|---|---|
| `crates/formats` (`deckhand-formats`) | Pioneer on-disk formats: `export.pdb`, `exportExt.pdb`, ANLZ, `*SETTING.DAT` |
| `crates/cli` (`deckhand`) | Command-line tool to inspect and (later) build exports |
| `crates/library` | Own library model and storage (SQLite), importers |
| `crates/audio` | Decoding and output |
| `crates/analysis` | BPM, beatgrid, key, waveforms |
| `crates/engine` | Decks, mixer, sync, cues, loops, effects, controllers |
| `crates/prodjlink` | Pro DJ Link networking |
| `crates/app` | Desktop UI |

Crates are added when their phase starts.

## Phases

Each step ends with tests passing; a phase ends with its acceptance check.

### Phase 1 — `export.pdb` reader
1. ✅ Workspace, plan, fixtures.
2. ✅ Low-level reader: file header, table directory, page chain walking,
   page headers, row index (row groups + presence bits).
3. ✅ DeviceSQL strings: short ASCII, long ASCII, UTF-16LE, ISRC variant.
4. ✅ Typed rows: tracks, artists, albums, genres, labels, keys, colors,
   artwork, playlist tree, playlist entries, history playlists/entries.
5. ✅ High-level `Library` view: tracks with resolved names, playlist tree
   with ordered tracks.
6. ✅ CLI: `deckhand pdb info|tracks|playlists`.
7. ✅ Tests: fixture assertions + cross-check against rekordcrate.

**Acceptance:** ✅ reads every fixture; ✅ row counts and playlist names
match rekordcrate (and the page headers); ⏳ reads a USB export you made
yourself — add it under `crates/formats/tests/fixtures/`.

Finding: rekordcrate 0.3.0 undercounts rows on pages with more than 255 row
slots (`pdb/num_rows` fixture: 6637 vs 7440 playlist entries). Our count
matches every page header and playlist positions are contiguous. Worth
reporting upstream.

### Phase 2 — the other export files (read)
1. ✅ `exportExt.pdb` (My Tag categories and tag–track links).
2. ✅ ANLZ `.DAT` / `.EXT` / `.2EX`: beat grid (PQTZ), cue lists
   (PCOB/PCO2 with comments, colors, quantized loops), waveforms
   (PWAV, PWV2–PWV7), song structure (PSSI, unmasked), path (PPTH),
   VBR index (PVBR). Unrecognized sections are kept verbatim.
3. ✅ Settings container: header strings, data block, CRC-16/XMODEM
   verification (data-only for player/device files, whole prefix for mixer).
4. ✅ Typed settings for all four files (51 options). Offsets mapped by
   diffing single-option fixtures; each of the 74 fixtures changes exactly
   its field. Values no fixture shows (defaults, gaps in evenly spaced
   sequences) are marked *inferred* in the docs; unmapped bytes decode as
   `Unknown(byte)`.
5. ✅ CLI: `deckhand anlz`, `deckhand settings`.
6. ✅ PQT2 (no public docs; own analysis): header repeats the first and
   last PQTZ beat and the beat count — verified on both fixtures. The
   2-byte-per-beat payload is exposed raw.

**Acceptance:** ✅ all fixture ANLZ and settings files parse; ✅ analysis
files agree with `export.pdb`; ✅ song structure matches rekordcrate;
✅ settings mapping verified per option.

Carried forward (needs data we don't have yet):
- Cue parsing on real files — fixtures contain no cues; tests use
  hand-built sections from the spec. Needs a self-made export with hot
  cues, memory cues, loops and comments.
- Language codes other than English/French/Greek/Turkish, waveform color
  `0x02`, and undecoded constant bytes in the settings blocks.
- Meaning of the PQT2 per-beat payload, the PWVC section (`.2EX`, three
  u16 values, no relation to waveform maxima found) and the 401st `PVBR`
  value (not the file size).

### Phase 3 — writers
1. ✅ pdb writer (`DatabaseWriter`): fresh-export page layout (index page +
   reserved page per table, greedy page fill, empty candidates past the end,
   rekordbox's write order for sequence numbers), row encoders for every
   modeled row type, raw rows for the rest, `new_export(date)` /
   `new_export_ext()` with the rows rekordbox puts in every export (colors,
   browse columns, menu tables, History row).
2. ✅ ANLZ writer (all sections; header constants as rekordbox writes them)
   and settings writer (defaults, typed edits, checksum).
3. ✅ Round trips: every fixture database re-written keeps all rows;
   rekordcrate reads what we write; edge cases (UTF-16, long ASCII, ISRC,
   multi-page tables) survive.
4. ✅ Byte-level comparison with rekordbox output:
   - `new_export` == rekordbox's empty `export.pdb`, byte for byte;
     `exportExt.pdb` likewise once its default My Tags are added;
   - every fixture ANLZ file and all 82 settings files re-serialize
     byte for byte;
   - every row of the fresh fixtures re-encodes byte for byte; rows of the
     edited fixture differ only in stale padding bytes.
5. ✅ CLI: `deckhand usb rewrite <source> <destination>` copies an export,
   rewriting every database, analysis and settings file.

**Acceptance:** ⏳ a stick produced by `deckhand usb rewrite` from a
rekordbox export browses and plays on a CDJ/XDJ (needs hardware).

Unverified defaults (no fixture data; check on hardware or with a
self-made export):
- `TrackExtras` for new tracks: `unknown_id` = 0, `database_pair` copied
  from the demo export.
- `PCP2` entries written from scratch: trailer bytes and comment layout
  when the comment is empty.
- Index pages are always written without entries (matches rekordbox's new
  exports; edited exports have entries).

### Phase 4 — library  (postponed; playing comes first)
1. Own SQLite schema (tracks, playlists, cues, beatgrids, tags, history).
2. Importers: rekordbox XML, USB export (`export.pdb` + ANLZ), folder scan
   with tag reading (e.g. `lofty`).
3. Exporters: rekordbox XML; USB export via phase 3 (copying audio files,
   path layout `/Contents/...`, `/PIONEER/USBANLZ/Pxxx/xxxxxxxx/`).
4. CLI: `deckhand import`, `deckhand export usb <mount>`.

**Acceptance:** first usable product — prepare USB sticks for club players
from a local library without rekordbox.

### Playable app (priority, started 2026-10-25)
Goal: play the user's library from a rekordbox USB export with the
AlphaTheta GRV6, later any controller rekordbox supports. The library
source is the USB export (the encrypted `master.db` is not touched).

Done (milestone 1):
- `crates/audio`: decoding (symphonia), real-time two-deck mixer (cpal,
  lock-free command queue): tempo, nudge, seek, channel fader, trim,
  3-band EQ with kill, crossfader, master, headphone cue on outputs 3–4.
- `crates/control`: MIDI input (midir), action list, TOML mappings with
  14-bit faders, relative encoders and inversion, learn mode.
- `crates/app` (egui): library browser (playlists, search), two decks with
  rekordbox waveforms, beat grid, hot/memory cues from the analysis files,
  CDJ-style cue, hot cues, tempo with ranges, mixer, controller window.

Done (milestone 2, 2026-10-25):
- Automatic mappings: rekordbox's `MidiMappings/*.midi.csv` files are read
  from the user's installation at run time (never bundled) and converted —
  works for every controller rekordbox supports. GRV6: 261 controls,
  80 LED/meter outputs. Used automatically on first connection.
- LED and level-meter feedback over MIDI out.
- 4 decks (2/4-deck layout switch), 4-channel mixer with filter knob and
  crossfader assign (A/THRU/B).
- Sync with master deck (tempo + beat phase, drift correction), quantize,
  loops (in/out/reloop/auto/halve/double, beat-loop pads), beat-jump pads,
  jump to start, hot cue delete, tempo range cycling, vinyl scratching,
  browser back/forward, waveform zoom.
- Waveforms: labeled hot cue flags, memory cue markers, memory loops,
  cue point; RGB and 3-band styles, both following each channel's EQ.
  Finding: PWV7 stores low, mid, high (measured against the audio), not
  mid, high, low as the crate-digger notes say.
- Verified end to end with a virtual GRV6 (`fake_grv6` example): import,
  input translation (incl. 14-bit faders) and LED output.

- Keyboard mixing (`app/keyboard.rs`): rekordbox's Performance 1 keys, by
  physical position, rebindable in Preferences. Not yet: Slip Reverse, next/previous track,
  grid editing keys.

- Stems, part 1 (`stems.rs`): user-chosen separation models run as
  external commands, cached per track; Make → acapella / instrumental /
  without drums / single stems as new tracks with the original's cues and
  grid. Demucs htdemucs: 60 s for a 4.6 min track on an M1 CPU, stems
  sample-aligned with deckhand's decoding.

Next:
1. Stems part 2: live stems on the decks (Stems pads, Part Isolator, GRV6),
   then a built-in ONNX runner. Demucs' stems sum ~11 dB off the original,
   so live stems should rebuild "other" as the remainder.
2. Beat FX and the sampler; stems (needs separation models).
3. Save hot cues / cue points set in deckhand.
4. Tune jog scratch resolution on hardware (`SCRATCH_SECONDS_PER_STEP`).

### Phase 5 — audio and analysis
1. Decoding (`symphonia`), resampling, output (`cpal`).
2. BPM and beat grid, musical key, waveform generation (preview + detail,
   3-band and RGB colors) with algorithms of our own or under permissive
   licences.
3. Feed analysis into ANLZ writer so exports carry beat grids and
   waveforms.

### Phase 6 — performance engine
1. Decks: play/cue, hot cues, loops, beat jump, pitch/tempo with key lock
   (time-stretching).
2. Mixer, sync and quantize, effects.
3. MIDI controller support (`midir`) with mapping files.
4. Recording.

### Phase 7 — Pro DJ Link
1. Device discovery and status packets (passive).
2. Beat/tempo sync with players; Ableton Link.
3. Serving the library to players over the network (dbserver protocol).

### Phase 8 — desktop app
UI over library + engine (toolkit to be chosen: egui, iced or slint).

## Status log

- 2026-10-25 — Phase 1 steps 1–7 done: `export.pdb` + `exportExt.pdb`
  reader (`deckhand-formats`), CLI (`deckhand pdb …`), 14 tests passing.
  Remaining for acceptance: a self-made USB export as fixture.
- 2026-10-25 — Phase 2 steps 1–3 and 5 done: ANLZ reader (all documented
  sections), settings container + checksum, CLI commands, 27 tests passing.
  Open: typed settings fields, PQT2/PWVC, cues on real data.
- 2026-10-25 — Phase 2 done: typed settings (51 options, all 74 option
  fixtures verified), PQT2 summary decoded, 31 tests passing. Next: phase 3
  (writers).
- 2026-10-25 — Phase 3 done: pdb, ANLZ and settings writers; new exports
  are byte-identical to rekordbox's; `deckhand usb rewrite`; 41 tests
  passing. Waiting on hardware test.
- 2026-10-25 — Priority changed to the playable app. Milestone 1 done:
  audio engine, controller mappings with learn mode, GUI; 51 tests.
- 2026-10-25 — Milestone 2: rekordbox mapping import (all controllers),
  LEDs, 4 decks, sync, loops, beat jump, scratch, filter; 59 tests.
- 2026-10-25 — Cue markers on waveforms; EQ-reactive RGB and 3-band
  waveforms; 62 tests.
- 2026-10-25 — Memory cue controls (call ◀/▶ incl. memory loops, store,
  delete; GRV6 buttons mapped from rekordbox); gated hot cues (per-deck
  GATE mode + per-pad gate actions, all learnable). Next idea: a software
  Shift layer for controllers whose Shift doesn't change its messages.
- 2026-10-25 — Older controllers / rekordbox 6: importer reads column
  positions from the header, resolves generic `PADn_PadModeM` pads from
  their comments, knows legacy function names, matches ports without vendor
  prefixes and picks MIDI-mode variants; all 69 installed files import
  (DDJ-1000/800/FLX10 went from ~120 to ~385 controls). rekordbox 6 files
  are used when rekordbox 6 is installed or placed in
  `~/Library/Application Support/deckhand/rekordbox-mappings/`; any file
  can be imported directly.
- 2026-10-25 — Verified against rekordbox 6.8.6's 63 mapping files: every
  rb6 controller is also in rb7 (rb7 adds FLX2, GRV6, DJM-V5, XDJ-AN,
  XDJ-AZ); all 63 import with the same results as their rb7 versions.
  Fixed: rb6's DJM-S9 file uses bare-CR line endings (imported nothing
  before, 141 controls now). 67 tests.
- 2026-10-25 — Beat grid redrawn (outlined bar/phrase lines, beat ticks,
  brightness slider); beat jump like rekordbox (keeps phase, moves an
  active loop, ◀ size ▶ with adjustable size, pad jump range halve/double
  from the controller); rekordbox-style mixer (knobs, capped faders,
  segmented meters, crossfader) with per-controller channel order
  (1-2-3-4 or 3-1-2-4, saved in the mapping). 68 tests.
- 2026-10-25 — View menu: horizontal or vertical waveforms (vertical:
  columns between decks and mixer, upcoming music from the top), show/hide
  waveforms, mixer and library. Next idea: keyboard shortcuts.
- 2026-10-25 — Master Tempo (key lock): WSOLA time-stretcher in
  `deckhand-audio` (40 ms grains, ±12 ms similarity search, ~300× real
  time per deck), MT button and controller Master Tempo button/LED;
  `--example stretch` renders a track to WAV for listening. Waveforms follow
  the mixer's channel order. Windows build: `scripts/build-windows.sh`
  (cargo-xwin, static CRT, no console window); paths for `%APPDATA%`,
  drive letters and `Program Files` rekordbox installs. 70 tests.
- 2026-10-25 — Audio settings window (output, sample rate, buffer size with
  latency, Standard/High = linear/cubic interpolation); restarting the
  output keeps loaded tracks playing. Jog rewritten: the wheel moves a
  target in the audio engine and a damped follower glides there per sample
  (scratching and paused jogging), instead of per-UI-frame speed jumps and
  hard position jumps. Controllers get the Pioneer status request
  (`F0 00 40 05 00 00 <model> 00 03 01 F7`) on connect so faders and knobs
  start at their physical positions; model IDs in `deckhand-control`'s
  `pioneer.rs` (GRV6 = 04 0C, untested on hardware yet;
  `--example sysex_probe` checks it). 73 tests.
- 2026-10-25 — Controller functions: slip mode (shadow timeline in the
  engine for scratch, loops and held hot cues), key shift/sync/reset,
  Keyboard and Key Shift pad modes, TAP BPM (rewrites the grid), active
  loop, channel/crossfader fader start, Master Cue, browse page/top/bottom,
  focus switch. Effects in `deckhand-audio/src/fx.rs` (own DSP): Beat FX
  Delay, Echo, Low Cut Echo, Spiral, Helix, Reverb, Flanger, Phaser, Filter,
  Trans, Pitch, Roll, Mobius Saw/Tri with beat values, tap, assign
  (CH/master/sampler) and Release FX; 16 momentary Pad FX presets; Color FX
  Filter, Space, Dub Echo, Crush, Noise, Pitch; 8-slot sampler from the
  `samples` folder. `Feedback` is now deck or global (FX/CFX/Master Cue
  LEDs). Still unmapped on the GRV6: stems, Part Isolator, Drum Swap/Roll/
  Trans, preview player, related tracks, tag list. 85 tests.
- 2026-10-25 — Performance pads as in the DDJ-GRV6 manual (rekordbox
  section): per-deck pad modes (Hot Cue, Keyboard, Pad FX, Beat Jump, Beat
  Loop, Sampler, Key Shift) switched by the controller's mode buttons (LEDs
  follow) or on screen. Beat Jump ◀1 1▶ ◀2 2▶ ◀4 4▶ ◀8 8▶ with Shift + 7/8
  halving/doubling; Keyboard picks a hot cue first, pads ±0…+7 with Shift +
  7/8 paging and Shift + 1–6 deleting hot cues; Key Shift ±0…+7 paged;
  sampler 16 slots (decks 1/3 → 1–8, 2/4 → 9–16), Shift + pad stops or loads
  the selected track; CALL ◀/▶ halve/double a playing loop; pad LEDs per
  mode, SLIP flashes while slipping. `PadMode::pad_action` is the single
  pad layout table. 86 tests.
- 2026-10-25 — Shift / parameter layer of the pads: on-screen SHIFT latch
  (or the Shift key) and ◀ caption ▶ per pad mode via
  `PadMode::parameter_action` — memory call (Hot Cue), pages (Keyboard, Key
  Shift), A–H / I–P (Pad FX), amounts ×½/×2 and with Shift ×1/16/×16 (Beat
  Jump, rekordbox's Parameter1/2), loop sizes ×½/×2 (Beat Loop, our own),
  sampler bank 1–4 (4 × 16 slots, folder holds up to 64 files). Importer maps
  Parameter1*_Sampler, *_PadFx and Parameter2*_BeatJump. 87 tests.
- 2026-10-25 — Library, step 1 (own collection): `deckhand-library` with
  tracks (tags via lofty: title/artist/album/genre/label/comment/year/BPM/key
  /artwork flag, rating, colour, play count, cues field for later), nested
  playlists and folders, rescannable folders, atomic JSON storage in the data
  dir. App: Collection source beside USB exports, sidebar tree with create/
  rename/move/delete, add folder/files, drop files on the window, background
  scan with progress, track context menu (load, add to/remove from playlist,
  reveal, remove), drag tracks onto playlists, browse knob/LOAD work on it.
  CLI: `deckhand collection add|list`. Next: track analysis (BPM, grid, key,
  coloured waveforms) for collection tracks, then USB export, then browser.
- 2026-10-25 — rekordbox XML import (`deckhand-library/src/rekordbox_xml.rs`,
  quick-xml): COLLECTION tracks (Location URLs incl. Windows drive paths,
  tags, rating 0–255 → stars, colour labels, play count, date added), TEMPO
  beat-grid markers, POSITION_MARK hot/memory cues and loops with colours,
  PLAYLISTS tree (KeyType TrackID or location). Matched by file path; the
  playlists go in a top-level `rekordbox` folder that a re-import replaces.
  Decks use the imported grid and cues. App button + `deckhand collection
  import-xml`. 93 tests.
- 2026-10-25 — Shift as a modifier in mappings: `[[shift]]` controls (learned
  with "Learn Shift button", or the rekordbox file's `Shift` rows), bindings
  with `shift = true` apply while one is held (falling back to the normal
  binding), learning while holding Shift records a Shift combination, a
  button releases the action it pressed even if Shift was let go first.
  Controller window: Shift buttons count/learn/clear, per-function Shift
  checkbox. 95 tests.
- 2026-10-25 — Browser: shared track table (`app/src/browser.rs`) for the
  collection and USB exports. Click a header to sort (again reverses, a
  third time returns to playlist order; keys sort by Camelot), right-click a
  header to choose columns (Title, Artist, Album, Genre, Label, BPM, Key,
  Time, Year, Rating, Plays, Added, Bitrate, Comment); both saved in the
  config. Ratings are clickable dots for collection tracks. Browse knob and
  LOAD follow the sorted order. 97 tests.
- 2026-10-25 — Preferences window (Deck / Mixer / Audio tabs), modelled on
  rekordbox's controller preferences: Eject/Load Lock, Needle Lock, Memory
  Cue Call Lock, start at the earliest memory/hot cue, Instant Doubles, Auto
  Cue (level, per deck), Vinyl Speed Adjust (start-up and brake ramps in the
  engine), scratch sensitivity; crossfader curve (smooth/standard/sharp),
  channel fader curve (slow/standard/fast), Isolator vs EQ, master limiter.
  `scripts/build-macos.sh` builds an Apple Silicon `deckhand.app` (ad-hoc
  signed, zipped; `--universal` adds Intel). 100 tests.
- 2026-10-26 — Deck layout: every deck control is an element (`layout.rs`,
  28 of them: CUE, Play, SYNC, MASTER, Q, VINYL, SLIP, loop groups, memory,
  pad parts, tempo parts, title/info/overview). View → Deck elements shows
  or hides each one (separately for 2- and 4-deck layouts); View → Edit deck
  layout lets elements be dragged before/after others, to row ends, a new
  row, the side column, or Hide (`app/deck_view.rs`). Saved in the config,
  repaired on load. Status messages moved to a one-line, expandable bar at
  the bottom. 103 tests.
- 2026-10-26 — rekordbox-like look: darker flat theme (`widgets::apply_theme`),
  round CUE/PLAY buttons with orange/green rings that light up (CUE at the
  cue point, PLAY while playing; icons drawn), outlined buttons that light in
  their colour (BEAT SYNC blue, MASTER orange, Q/SLIP/MT red, loop orange);
  compact sampler buttons so the FX strip stays one line.
- 2026-10-26 — Session memory (`app/session.rs`, `[session]` in the config):
  per deck crossfader assign, tempo range, Q, VINYL, SLIP, MT, GATE, pad mode,
  jump/loop sizes, fader/trim/EQ/filter/headphone cue; crossfader, master and
  phones levels; Beat FX/Color FX settings; waveform zoom. Saved on quit and
  every 3 s when changed; restored before the audio starts.
- 2026-10-26 — Waveforms without rekordbox colour data (collection tracks,
  monochrome analyses) are coloured from their own band energies like the
  RGB view (bass red, mids green, highs blue) instead of plain blue. App is
  shown as RofLoxBox (window, RofLoxBox.app / .exe; code keeps deckhand).
