# RofLoxBox

(Working name in the code: deckhand.)

DJ library tooling in Rust, compatible with Pioneer DJ hardware. No
subscriptions, accounts, telemetry or ads. See [PLAN.md](PLAN.md) for scope,
ground rules and roadmap.

## Status

Reads and writes everything rekordbox puts on a USB/SD export:
`export.pdb`, `exportExt.pdb`, analysis files (`ANLZ*.DAT/.EXT/.2EX`) and the
settings files. New databases are byte-identical to the ones rekordbox writes.

```sh
cargo run -- pdb info      /Volumes/USB/PIONEER/rekordbox/export.pdb
cargo run -- pdb tracks    /Volumes/USB/PIONEER/rekordbox/export.pdb
cargo run -- pdb playlists /Volumes/USB/PIONEER/rekordbox/export.pdb
cargo run -- pdb tags      /Volumes/USB/PIONEER/rekordbox/exportExt.pdb
cargo run -- anlz          /Volumes/USB/PIONEER/USBANLZ/P016/0000875E/ANLZ0000.EXT
cargo run -- settings      /Volumes/USB/PIONEER/MYSETTING.DAT
cargo run -- usb rewrite   /Volumes/USB /Volumes/OTHER_USB   # rewrite an export with deckhand's writers
cargo test
```

## Playing

```sh
cargo run --release -p deckhand-app
```

Pick **Collection** in the top bar to use deckhand's own library: add a
folder of music or single files (or drop them on the window), import your
rekordbox library (in rekordbox: File → Export Collection in xml format, then
**Import rekordbox XML…** here — tracks, playlists, beat grids, hot cues,
memory cues/loops, ratings and colours come along), make playlists
and folders, drag tracks onto playlists, right-click tracks for more. Or plug
in a rekordbox USB export (or pick its folder) to play from it. Load tracks
by double-clicking, and mix. Connect a controller by USB, open
**Controller**, pick its MIDI port and press **Learn** next to each
function, then move that control. Mappings are saved in
`~/Library/Application Support/deckhand/mappings/` (`%APPDATA%\deckhand\mappings\`
on Windows). **MT** next to the tempo fader is Master Tempo: the key stays
the same when the tempo changes. **⚙ Audio** sets the output, sample rate,
buffer size (latency) and interpolation quality. Pioneer DJ / AlphaTheta
controllers are asked for their fader and knob positions when they connect,
so the software starts where the hardware is.

The strip under the waveforms holds Beat FX (14 types, beat-synced to the
master deck, assignable to a channel, the master or the sampler, with tap
and Release FX), Color FX for the channel knobs (Filter, Space, Dub Echo,
Crush, Noise, Pitch), Master Cue and an 8-slot sampler that plays the first
eight audio files in the `samples` folder next to the config. Decks have
SLIP, key shift / key sync and TAP BPM; controller pads also cover Pad FX,
Key Shift and Keyboard modes.

The computer keyboard mixes too, with the keys of rekordbox's Performance 1
preset: `Z`/`N` play, `A`/`H` cue, `Q`/`Y` sync, `W` `E`/`U` `I` pitch bend,
`1`–`4`/`6`–`9` hot cues, `D` `F` `G`/`K` `L` `;` loops, Shift + arrows load.
Preferences → Keyboard lists them all, lets you move any of them and turns
them off.

Right-click tracks → Make: an acapella, an instrumental, the track without
drums, or one stem alone, as a new 24-bit WAV track carrying the original's
cues, memory cues, loops, beat grid and key, next to it in its playlists.
Separation runs any model you add (Preferences → Stems: a folder with a
`model.toml` naming its stems and the command that separates a track), with
presets for Demucs and audio-separator; results are cached per track.

Dragging a running waveform stops the deck under the mouse, and it plays on
from there when let go. Preferences → Appearance has dark and light themes.

### Installers

Each script leaves its results in `target/dist/`:

- `scripts/build-macos.sh [--universal]` (on a Mac): `RofLoxBox.app`, a
  `.dmg` to drag it into Applications, and a `.zip`. `--universal` also
  runs on Intel Macs (`rustup target add x86_64-apple-darwin`).
- `scripts/build-windows.sh` (on macOS or Linux): a self-contained 64-bit
  Windows 10/11 `RofLoxBox.exe` in a `.zip` and, with NSIS
  (`brew install makensis`), a `-setup.exe` installer. Needs
  `cargo install cargo-xwin` and `rustup target add x86_64-pc-windows-msvc`.
- `scripts/build-linux.sh` (on Linux, with `libasound2-dev`): a `.tar.gz`
  and, with `appimagetool`, an AppImage.

Pushing a version tag (`git tag v0.1.0 && git push --tags`) runs
`.github/workflows/release.yml`, which builds all three on GitHub's machines
and makes a draft release with them and their SHA-256 sums.

Signing needs your own certificates; without them the builds are unsigned
(macOS ad hoc: open it once with right-click → Open; Windows SmartScreen
warns once). The scripts read `MACOS_SIGN_IDENTITY` and
`MACOS_NOTARY_PROFILE` (Apple Developer ID and notarization) and
`WINDOWS_SIGN_PFX` / `WINDOWS_SIGN_PASSWORD`; the workflow reads the
repository secrets listed at its top. Linux packages are not signed.

The icon is `packaging/icon.svg`; `packaging/make-icons.sh` renders the
`.icns`, `.ico` and `.png` files in `packaging/icons/` from it.

## Crates

- `crates/formats` (`deckhand-formats`): Pioneer on-disk formats
- `crates/cli` (`deckhand`): command-line tool
- `crates/library` (`deckhand-library`): deckhand's own collection
  (`collection.json`), folder scanning and tag reading (`lofty`)
- `crates/audio` (`deckhand-audio`): decoding and the real-time mixer
- `crates/control` (`deckhand-control`): MIDI input, mappings, learn mode
- `crates/app` (`deckhand-app`): the desktop application

## Support me

[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?style=for-the-badge&logo=buy-me-a-coffee&logoColor=black)](https://buymeacoffee.com/roflox)
