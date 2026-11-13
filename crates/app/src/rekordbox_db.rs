//! Reading rekordbox's own library (`master.db`): tracks with their tags,
//! hot cues, memory cues and loops, the playlist tree, and the beat grids
//! from rekordbox's analysis files beside it. With Preferences allowing it,
//! deckhand's changes are written back (`rekordbox_save`), with rekordbox
//! closed and a copy of the library kept first.
//!
//! The database is SQLCipher (encrypted SQLite), opened like rekordbox opens
//! it: SQLCipher decrypts the pages it reads and encrypts the ones it
//! writes, with its write-ahead log beside the file. deckhand doesn't carry
//! the passphrase: it is fetched from the pyrekordbox project, which
//! publishes it (or entered in Preferences), and kept in the settings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use deckhand_formats::anlz::AnlzFile;
use deckhand_library::{Cue, CueKind, ScannedFile, XmlLibrary, XmlNode, XmlTrack, file_stamp};
use rusqlite::OptionalExtension;

use crate::usb_import::grid_markers;

/// Where rekordbox keeps its library on this computer.
pub fn default_path() -> Option<PathBuf> {
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Pioneer/rekordbox")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Pioneer/rekordbox")
    };
    Some(dir.join("master.db"))
}

/// rekordbox's library as read.
pub struct Opened {
    pub library: XmlLibrary,
    /// The key that opened it.
    pub key: String,
    /// rekordbox's count of changes to it, to tell later whether it changed.
    pub count: i64,
}

/// Reads the library at `path` with the key `saved` (from the settings),
/// or, when there is none or it no longer fits, with the one pyrekordbox
/// publishes.
pub fn open_any(path: &Path, saved: &str) -> Result<Opened> {
    let saved = saved.trim();
    let first = if saved.is_empty() {
        None
    } else {
        match open(path, saved) {
            Ok(opened) => return Ok(opened),
            Err(err) => Some(err),
        }
    };
    let fetched = match fetch_key() {
        Ok(key) => key,
        Err(err) => return Err(first.unwrap_or(err)),
    };
    if fetched == saved {
        return Err(first.unwrap_or_else(|| anyhow::anyhow!("the key doesn't fit")));
    }
    open(path, &fetched)
}

/// pyrekordbox's sources: the (obfuscated) key, and what it is XORed with.
const PYREKORDBOX: [&str; 2] = [
    "https://raw.githubusercontent.com/dylanljones/pyrekordbox/master/pyrekordbox/masterdb/database.py",
    "https://raw.githubusercontent.com/dylanljones/pyrekordbox/master/pyrekordbox/utils.py",
];

/// The key pyrekordbox publishes, downloaded from its sources: `BLOB` is
/// base85 of the key zlib-compressed and XORed with `BLOB_KEY`.
pub fn fetch_key() -> Result<String> {
    let download = |url: &str| -> Result<String> {
        // curl comes with macOS, Windows 10 and later, and Linux.
        let output = std::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "15", url])
            .stdin(std::process::Stdio::null())
            .output()
            .context("downloading the rekordbox key needs curl")?;
        if !output.status.success() {
            bail!("couldn't download {url}: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let sources = PYREKORDBOX.iter().map(|url| download(url)).collect::<Result<Vec<_>>>()?;
    let quoted = |name: &str| -> Option<String> {
        let line = sources.iter().flat_map(|s| s.lines()).find(|l| l.starts_with(name))?;
        let start = line.find("b\"")? + 2;
        Some(line[start..start + line[start..].find('"')?].to_owned())
    };
    let (Some(blob), Some(mask)) = (quoted("BLOB = "), quoted("BLOB_KEY = ")) else {
        bail!("pyrekordbox no longer publishes the rekordbox key where deckhand looks");
    };
    deobfuscate(&blob, mask.as_bytes())
}

fn deobfuscate(blob: &str, mask: &[u8]) -> Result<String> {
    use std::io::Read;
    let data = base85(blob).context("pyrekordbox's key isn't readable")?;
    let xored: Vec<u8> = data.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m).collect();
    let mut key = String::new();
    flate2::read::ZlibDecoder::new(xored.as_slice())
        .read_to_string(&mut key)
        .context("pyrekordbox's key isn't readable")?;
    Ok(key)
}

/// Base85 as Python's `base64.b85decode` (RFC 1924's alphabet).
fn base85(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
    let digits: Vec<u32> = text
        .bytes()
        .map(|c| ALPHABET.iter().position(|a| *a == c).map(|d| d as u32))
        .collect::<Option<_>>()?;
    let mut out = Vec::new();
    for chunk in digits.chunks(5) {
        // A short last group is padded with the highest digit.
        let padding = 5 - chunk.len();
        let value = chunk
            .iter()
            .chain(std::iter::repeat_n(&84, padding))
            .try_fold(0u32, |v, d| v.checked_mul(85)?.checked_add(*d))?;
        out.extend_from_slice(&value.to_be_bytes()[..4 - padding]);
    }
    Some(out)
}

/// Opens the library at `path` with the passphrase `key`, to read or also
/// to change, as rekordbox does (changes rekordbox, still running, keeps in
/// its write-ahead log are seen too).
pub(crate) fn connect(path: &Path, key: &str, write: bool) -> Result<rusqlite::Connection> {
    if !path.is_file() {
        bail!("no rekordbox library at {}", path.display());
    }
    // Opened to write even to read: a read-only connection leaves the
    // write-ahead log's files behind, while rekordbox and SQLite tidy them
    // up on closing. Reading, SQLite refuses any change.
    let db =
        rusqlite::Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    db.pragma_update(None, "key", key.trim())?;
    db.busy_timeout(std::time::Duration::from_secs(3))?;
    // The key is only tried with the first read.
    db.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
        .map_err(|_| anyhow::anyhow!("the key doesn't open this rekordbox library"))?;
    if !write {
        db.pragma_update(None, "query_only", true)?;
    }
    Ok(db)
}

/// Reads the library at `path` (and its analysis files) with `key`.
fn open(path: &Path, key: &str) -> Result<Opened> {
    let db = connect(path, key, false)?;
    let share = path.parent().unwrap_or(Path::new("")).join("share");
    let library = read(&db, &share).with_context(|| format!("reading {}", path.display()))?;
    Ok(Opened { library, key: key.to_owned(), count: change_count(&db)? })
}

/// rekordbox's count of changes to its library.
fn change_count(db: &rusqlite::Connection) -> Result<i64> {
    let count = db
        .query_row(
            "SELECT int_1 FROM agentRegistry WHERE registry_id = 'localUpdateCount'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?;
    Ok(count.flatten().unwrap_or(0))
}

/// rekordbox was found closed: [`write`] takes this, so nothing writes its
/// library without asking first. (It could start right after; the change
/// count checked inside the write catches what it changed.)
pub(crate) struct Closed(());

/// [`Closed`] if rekordbox isn't running.
pub(crate) fn rekordbox_closed() -> Option<Closed> {
    (!rekordbox_running()).then_some(Closed(()))
}

/// Whether rekordbox is running: its library must not be written then.
pub fn rekordbox_running() -> bool {
    let output = if cfg!(windows) {
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq rekordbox.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("rekordbox.exe"))
    } else {
        std::process::Command::new("pgrep")
            .args(["-x", "rekordbox"])
            .output()
            .map(|o| o.status.success())
    };
    output.unwrap_or(false)
}

/// rekordbox's count of changes to its library: each change takes the
/// next number, and the changed row keeps it.
pub(crate) struct Usn(pub(crate) i64);

impl Usn {
    pub(crate) fn next(&mut self) -> i64 {
        self.0 += 1;
        self.0
    }
}

/// rekordbox's timestamp format.
pub(crate) fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f +00:00").to_string()
}

/// The folder the library was copied into before [`write`] changed it;
/// only `write` makes one. Other files of rekordbox's are changed only
/// through [`Backup::keep`], so they're copied first too.
pub(crate) struct Backup(PathBuf);

impl Backup {
    /// Copies `original`, a file under `root`, into the backup (at the same
    /// place under `name`); then it may be replaced.
    pub fn keep(&self, original: &Path, root: &Path, name: &str) -> Result<()> {
        let relative = original.strip_prefix(root).unwrap_or(original);
        let copy = self.0.join(name).join(relative);
        if let Some(dir) = copy.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(original, &copy)
            .with_context(|| format!("backing up {}", original.display()))?;
        Ok(())
    }
}

/// What [`write`] did.
pub(crate) struct Written<T> {
    /// What the edit returned.
    pub result: T,
    /// Where the old library was copied; `None` when nothing changed and
    /// nothing was written.
    pub backup: Option<Backup>,
    /// rekordbox's change count now.
    pub count: i64,
}

/// Opens rekordbox's library at `path` for changing and lets `edit` change
/// it in a transaction (counting each change with the [`Usn`]), as rekordbox
/// itself changes it. The library is copied into a new folder in `backups`
/// first, and checked whole before the changes are committed.
pub(crate) fn write<T>(
    _closed: &Closed,
    path: &Path,
    passphrase: &str,
    backups: &Path,
    edit: impl FnOnce(&rusqlite::Connection, &mut Usn) -> Result<T>,
) -> Result<Written<T>> {
    let mut db = connect(path, passphrase, true)?;
    let backup =
        backups.join(format!("master-{}", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")));
    std::fs::create_dir_all(&backup).with_context(|| format!("creating {}", backup.display()))?;
    // The library as it is, write-ahead log and all (nothing writes it now).
    for (from, name) in
        [(path.to_path_buf(), "master.db"), (path.with_extension("db-wal"), "master.db-wal")]
    {
        if from.is_file() {
            std::fs::copy(&from, backup.join(name))
                .with_context(|| format!("backing up to {}", backup.display()))?;
        }
    }
    let tx = db.transaction()?;
    let mut usn = Usn(change_count(&tx)?);
    let start = usn.0;
    let result = edit(&tx, &mut usn)?;
    if usn.0 == start {
        drop(tx);
        let _ = std::fs::remove_dir_all(&backup);
        return Ok(Written { result, backup: None, count: start });
    }
    tx.execute(
        "UPDATE agentRegistry SET int_1 = ?1, updated_at = ?2 WHERE registry_id = 'localUpdateCount'",
        rusqlite::params![usn.0, now()],
    )?;
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("the changed library didn't check out ({integrity}); nothing was written");
    }
    tx.commit()?;
    // Closing moves the write-ahead log into the file, as rekordbox does.
    db.close().map_err(|(_, e)| e)?;
    keep_newest(backups, 10);
    Ok(Written { result, backup: Some(Backup(backup)), count: usn.0 })
}

/// Deletes all but the newest `keep` backups (folders, or files from
/// before) in `dir`.
fn keep_newest(dir: &Path, keep: usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("master-")))
        .collect();
    // The names sort by date.
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for old in &files[..excess] {
        if old.is_dir() {
            let _ = std::fs::remove_dir_all(old);
        } else {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Hot cue slots A–H by rekordbox's cue kind (4 is not a hot cue).
pub(crate) fn hot_cue_slot(kind: i64) -> Option<u8> {
    match kind {
        1..=3 => Some(kind as u8 - 1),
        5..=9 => Some(kind as u8 - 2),
        _ => None,
    }
}

/// Hot cue colours by rekordbox's colour number.
pub(crate) const HOT_CUE_COLORS: [(i64, [u8; 3]); 14] = [
    (1, [48, 90, 255]),
    (8, [69, 172, 219]),
    (19, [48, 210, 110]),
    (21, [60, 235, 80]),
    (22, [40, 226, 20]),
    (23, [125, 193, 61]),
    (32, [195, 175, 4]),
    (36, [255, 140, 0]),
    (38, [224, 100, 27]),
    (42, [230, 40, 40]),
    (43, [255, 55, 111]),
    (45, [255, 18, 123]),
    (56, [180, 50, 255]),
    (60, [170, 114, 255]),
];

fn hot_cue_color(index: i64) -> Option<[u8; 3]> {
    HOT_CUE_COLORS.iter().find(|(i, _)| *i == index).map(|(_, rgb)| *rgb)
}

fn read(db: &rusqlite::Connection, share: &Path) -> Result<XmlLibrary> {
    let mut cues: HashMap<String, Vec<Cue>> = HashMap::new();
    let mut statement = db.prepare(
        "SELECT ContentID, InMsec, OutMsec, Kind, ColorTableIndex, Comment FROM djmdCue
         WHERE rb_local_deleted = 0 ORDER BY ContentID, InMsec",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let kind = match integer(row, 3)? {
            0 => CueKind::Memory,
            other => match hot_cue_slot(other) {
                Some(slot) => CueKind::Hot(slot),
                None => continue,
            },
        };
        let start = integer(row, 1)? as f64 / 1000.0;
        let end = Some(integer(row, 2)?).filter(|ms| *ms > 0).map(|ms| ms as f64 / 1000.0);
        let color = match kind {
            CueKind::Hot(_) => hot_cue_color(integer(row, 4)?),
            _ => None,
        };
        cues.entry(row.get(0)?).or_default().push(Cue {
            kind,
            start,
            end: end.filter(|e| *e > start),
            color,
            name: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        });
    }

    let mut tracks = Vec::new();
    let mut statement = db.prepare(
        "SELECT c.ID, c.FolderPath, c.Title, ar.Name, al.Name, g.Name, l.Name, k.ScaleName,
                c.BPM, c.Length, c.TrackNo, c.BitRate, c.SampleRate, c.Commnt, c.Rating,
                c.ReleaseYear, c.ColorID, c.DJPlayCount, c.StockDate, c.DateCreated,
                c.AnalysisDataPath, c.ImagePath
         FROM djmdContent c
         LEFT JOIN djmdArtist ar ON ar.ID = c.ArtistID
         LEFT JOIN djmdAlbum al ON al.ID = c.AlbumID
         LEFT JOIN djmdGenre g ON g.ID = c.GenreID
         LEFT JOIN djmdLabel l ON l.ID = c.LabelID
         LEFT JOIN djmdKey k ON k.ID = c.KeyID
         WHERE c.rb_local_deleted = 0",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let text = |i: usize| -> rusqlite::Result<String> {
            Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default())
        };
        let number = |i: usize| integer(row, i);
        let id: String = row.get(0)?;
        let path = PathBuf::from(text(1)?);
        // Streaming tracks have no file.
        if !path.is_absolute() {
            continue;
        }
        let analysis = Some(text(20)?)
            .filter(|p| !p.is_empty())
            .map(|p| share.join(p.trim_start_matches('/')))
            .filter(|p| p.is_file());
        let tempo = analysis
            .as_ref()
            .and_then(|dat| AnlzFile::open(dat).ok())
            .and_then(|dat| dat.beat_grid().map(grid_markers))
            .unwrap_or_default();
        let (file_size, modified) = file_stamp(&path);
        let positive = |n: i64| u32::try_from(n).ok().filter(|n| *n > 0);
        let file = ScannedFile {
            path: path.clone(),
            title: text(2)?,
            artist: text(3)?,
            album: text(4)?,
            genre: text(5)?,
            label: text(6)?,
            comment: text(13)?,
            year: u16::try_from(number(15)?).ok().filter(|y| *y > 0),
            track_number: positive(number(10)?),
            duration: number(9)? as f64,
            bpm: Some(number(8)? as f64 / 100.0).filter(|b| *b > 0.0),
            key: Some(text(7)?).filter(|k| !k.is_empty()),
            sample_rate: positive(number(12)?),
            bitrate: positive(number(11)?),
            has_artwork: !text(21)?.is_empty(),
            file_size,
            modified,
        };
        let added = [text(18)?, text(19)?].iter().find_map(|d| added_seconds(d));
        tracks.push(XmlTrack {
            location: path.to_string_lossy().into_owned(),
            file,
            rating: number(14)?.clamp(0, 5) as u8,
            color: u8::try_from(number(16)?).ok().filter(|c| (1..=8).contains(c)),
            play_count: u32::try_from(number(17)?).unwrap_or(0),
            added,
            tempo,
            cues: cues.remove(&id).unwrap_or_default(),
            analysis,
            id,
        });
    }

    let mut songs: HashMap<String, Vec<String>> = HashMap::new();
    let mut statement = db.prepare(
        "SELECT PlaylistID, ContentID FROM djmdSongPlaylist
         WHERE rb_local_deleted = 0 ORDER BY PlaylistID, TrackNo",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        songs.entry(row.get(0)?).or_default().push(row.get(1)?);
    }
    // (id, parent, name, folder), in rekordbox's order.
    let mut statement = db.prepare(
        "SELECT ID, ParentID, Name, Attribute FROM djmdPlaylist
         WHERE rb_local_deleted = 0 ORDER BY Seq",
    )?;
    let nodes: Vec<(String, String, String, i64)> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                integer(row, 3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let playlists = children("root", &nodes, &mut songs, 0);
    Ok(XmlLibrary { tracks, playlists })
}

/// The playlist tree below `parent`. Intelligent playlists (attribute 4)
/// are left out: they are rules, not tracks.
fn children(
    parent: &str,
    nodes: &[(String, String, String, i64)],
    songs: &mut HashMap<String, Vec<String>>,
    depth: usize,
) -> Vec<XmlNode> {
    if depth > 32 {
        return Vec::new();
    }
    nodes
        .iter()
        .filter(|(_, p, _, _)| p == parent)
        .filter_map(|(id, _, name, attribute)| match attribute {
            1 => Some(XmlNode::Folder {
                name: name.clone(),
                children: children(id, nodes, songs, depth + 1),
                id: Some(id.clone()),
            }),
            0 => Some(XmlNode::Playlist {
                name: name.clone(),
                keys: songs.remove(id).unwrap_or_default(),
                by_location: false,
                id: Some(id.clone()),
            }),
            _ => None,
        })
        .collect()
}

/// A number column, which rekordbox sometimes stores as text (0 if empty).
pub(crate) fn integer(row: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<i64> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(i)? {
        ValueRef::Integer(n) => n,
        ValueRef::Real(n) => n as i64,
        ValueRef::Text(t) => {
            std::str::from_utf8(t).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
        }
        _ => 0,
    })
}

/// `YYYY-MM-DD` as Unix seconds (midnight UTC).
fn added_seconds(date: &str) -> Option<u64> {
    let date = chrono::NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
    u64::try_from(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp()).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small library like rekordbox's at `path`: encrypted with
    /// `passphrase`, its changes going through a write-ahead log.
    pub(crate) fn library(path: &Path, passphrase: &str) {
        let _ = std::fs::remove_file(path);
        let db = rusqlite::Connection::open(path).unwrap();
        db.pragma_update(None, "key", passphrase).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        // rekordbox's tables, with the columns it has.
        db.execute_batch(&format!(
            "CREATE TABLE djmdContent (ID PRIMARY KEY, FolderPath, FileNameL, FileNameS, Title,
                ArtistID, AlbumID, GenreID, BPM, Length, TrackNo, BitRate, BitDepth, Commnt,
                FileType, Rating, ReleaseYear, RemixerID, LabelID, OrgArtistID, KeyID,
                StockDate, ColorID, DJPlayCount, ImagePath, MasterDBID, MasterSongID,
                AnalysisDataPath, SearchStr, FileSize, DiscNo, ComposerID, Subtitle,
                SampleRate, DisableQuantize, Analysed, ReleaseDate, DateCreated, ContentLink,
                Tag, ModifiedByRBM, HotCueAutoLoad, DeliveryControl, DeliveryComment,
                CueUpdated, AnalysisUpdated, TrackInfoUpdated, Lyricist, ISRC,
                SamplerTrackInfo, SamplerPlayOffset, SamplerGain, VideoAssociate, LyricStatus,
                ServiceID, OrgFolderPath, Reserved1, Reserved2, Reserved3, Reserved4, ExtInfo,
                rb_file_id, DeviceID, rb_LocalFolderPath, SrcID, SrcTitle, SrcArtistName,
                SrcAlbumName, SrcLength, {COMMON});
             CREATE TABLE djmdArtist (ID PRIMARY KEY, Name, SearchStr, {COMMON});
             CREATE TABLE djmdAlbum (ID PRIMARY KEY, Name, AlbumArtistID, ImagePath,
                Compilation, SearchStr, {COMMON});
             CREATE TABLE djmdGenre (ID PRIMARY KEY, Name, {COMMON});
             CREATE TABLE djmdLabel (ID PRIMARY KEY, Name, {COMMON});
             CREATE TABLE djmdKey (ID PRIMARY KEY, ScaleName, Seq, {COMMON});
             CREATE TABLE djmdCue (ID PRIMARY KEY, ContentID, InMsec, InFrame, InMpegFrame,
                InMpegAbs, OutMsec, OutFrame, OutMpegFrame, OutMpegAbs, Kind, Color,
                ColorTableIndex, ActiveLoop, Comment, BeatLoopSize, CueMicrosec,
                InPointSeekInfo, OutPointSeekInfo, ContentUUID, {COMMON});
             CREATE TABLE djmdPlaylist (ID PRIMARY KEY, Seq, Name, ImagePath, Attribute,
                ParentID, SmartList, {COMMON});
             CREATE TABLE djmdSongPlaylist (ID PRIMARY KEY, PlaylistID, ContentID, TrackNo,
                {COMMON});
             CREATE TABLE agentRegistry (registry_id PRIMARY KEY, int_1, updated_at);
             INSERT INTO agentRegistry VALUES ('localUpdateCount', 500, '');
             INSERT INTO djmdArtist (ID, Name) VALUES ('a1', 'Artist');
             INSERT INTO djmdKey (ID, ScaleName) VALUES ('k1', '8A');
             INSERT INTO djmdContent (ID, FolderPath, Title, ArtistID, KeyID, BPM, Length,
                TrackNo, BitRate, SampleRate, Commnt, Rating, ReleaseYear, ColorID,
                DJPlayCount, StockDate, DateCreated, AnalysisDataPath, ImagePath, UUID,
                rb_local_usn, DeviceID, MasterDBID, ContentLink)
             VALUES ('t1', '/music/One.wav', 'One', 'a1', 'k1', 17400, 200, 1, 1411, 44100,
                'nice', 4, 2024, '3', 7, '2025-02-24', '2024-06-05', '', '', 'u-t1', 400,
                'dev', 'mdb', 77);
             INSERT INTO djmdContent (ID, FolderPath, Title, BPM, Length, TrackNo, BitRate,
                SampleRate, Commnt, Rating, ReleaseYear, ColorID, DJPlayCount, StockDate,
                DateCreated, AnalysisDataPath, ImagePath, UUID, rb_local_usn)
             VALUES ('t2', '/music/Two.wav', 'Two', 12000, 100, 0, 0, 0, '', 0, 0, '0', 0, '',
                '', '', '', 'u-t2', 401);
             INSERT INTO djmdContent (ID, FolderPath, Title, rb_local_deleted, rb_local_usn)
             VALUES ('t3', '/music/Gone.wav', 'Gone', 1, 402);
             INSERT INTO djmdCue (ID, ContentID, InMsec, OutMsec, Kind, ColorTableIndex,
                Comment) VALUES ('c1', 't1', 1000, -1, 1, 22, '');
             INSERT INTO djmdCue (ID, ContentID, InMsec, OutMsec, Kind, ColorTableIndex,
                Comment) VALUES ('c2', 't1', 2000, -1, 5, 42, 'drop');
             INSERT INTO djmdCue (ID, ContentID, InMsec, OutMsec, Kind, ColorTableIndex,
                Comment) VALUES ('c3', 't1', 3000, 5000, 0, 0, '');
             INSERT INTO djmdCue (ID, ContentID, InMsec, OutMsec, Kind, ColorTableIndex,
                Comment) VALUES ('c4', 't1', 100, 900, 4, 0, '');
             INSERT INTO djmdPlaylist (ID, Seq, Name, Attribute, ParentID)
                VALUES ('f1', 1, 'Sets', 1, 'root');
             INSERT INTO djmdPlaylist (ID, Seq, Name, Attribute, ParentID)
                VALUES ('p1', 1, 'Friday', 0, 'f1');
             INSERT INTO djmdPlaylist (ID, Seq, Name, Attribute, ParentID)
                VALUES ('p2', 2, 'All', 0, 'root');
             INSERT INTO djmdPlaylist (ID, Seq, Name, Attribute, ParentID)
                VALUES ('s1', 3, 'Smart', 4, 'root');
             INSERT INTO djmdSongPlaylist (ID, PlaylistID, ContentID, TrackNo)
                VALUES ('e1', 'p1', 't2', 1);
             INSERT INTO djmdSongPlaylist (ID, PlaylistID, ContentID, TrackNo)
                VALUES ('e2', 'p1', 't1', 2);
             INSERT INTO djmdSongPlaylist (ID, PlaylistID, ContentID, TrackNo)
                VALUES ('e3', 'p2', 't1', 1);",
            COMMON = "UUID, rb_data_status DEFAULT 0, rb_local_data_status DEFAULT 0, rb_local_deleted DEFAULT 0, rb_local_synced DEFAULT 0, usn, rb_local_usn, created_at NOT NULL DEFAULT '', updated_at NOT NULL DEFAULT ''"
        ))
        .unwrap();
    }

    /// A temporary file name for a test library.
    pub(crate) fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("deckhand-rb-{name}-{}.db", std::process::id()))
    }

    #[test]
    fn reads_an_encrypted_library() {
        let path = temporary("read");
        library(&path, "secret");
        assert_ne!(&std::fs::read(&path).unwrap()[..16], b"SQLite format 3\0", "encrypted");
        assert!(connect(&path, "wrong", false).is_err());
        let reading = connect(&path, "secret", false).unwrap();
        assert!(reading.execute("DELETE FROM djmdContent", []).is_err(), "reading changes nothing");
        drop(reading);
        for log in ["db-wal", "db-shm"] {
            assert!(!path.with_extension(log).exists(), "nothing left beside it");
        }
        let opened = open(&path, "secret").unwrap();
        assert_eq!(opened.count, 500);
        let library = opened.library;
        let _ = std::fs::remove_file(&path);
        assert_eq!(library.tracks.len(), 2);
        let one = &library.tracks[0];
        assert_eq!((one.file.title.as_str(), one.file.artist.as_str()), ("One", "Artist"));
        assert_eq!(one.file.bpm, Some(174.0));
        assert_eq!(one.file.key.as_deref(), Some("8A"));
        assert_eq!((one.rating, one.color, one.play_count), (4, Some(3), 7));
        assert_eq!(one.added, Some(1_740_355_200));
        let cues: Vec<_> = one.cues.iter().map(|c| (c.kind, c.start, c.end, c.color)).collect();
        assert_eq!(
            cues,
            [
                (CueKind::Hot(0), 1.0, None, Some([40, 226, 20])),
                (CueKind::Hot(3), 2.0, None, Some([230, 40, 40])),
                (CueKind::Memory, 3.0, Some(5.0), None),
            ]
        );
        assert_eq!(one.cues[1].name, "drop");
        assert_eq!(library.tracks[1].color, None);
        assert_eq!(
            library.playlists,
            [
                XmlNode::Folder {
                    name: "Sets".into(),
                    children: vec![XmlNode::Playlist {
                        name: "Friday".into(),
                        keys: vec!["t2".into(), "t1".into()],
                        by_location: false,
                        id: Some("p1".into()),
                    }],
                    id: Some("f1".into()),
                },
                XmlNode::Playlist {
                    name: "All".into(),
                    keys: vec!["t1".into()],
                    by_location: false,
                    id: Some("p2".into()),
                },
            ]
        );
    }

    #[test]
    fn decodes_an_obfuscated_key() {
        use std::io::Write;
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        zlib.write_all(b"the key").unwrap();
        let xored: Vec<u8> =
            zlib.finish().unwrap().iter().zip(b"mask".iter().cycle()).map(|(b, m)| b ^ m).collect();
        // Python: base64.b85encode(xored)
        let encode = |bytes: &[u8]| -> String {
            const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
            let mut out = String::new();
            for chunk in bytes.chunks(4) {
                let mut word = [0u8; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                let mut value = u32::from_be_bytes(word);
                let mut digits = [0u8; 5];
                for d in digits.iter_mut().rev() {
                    *d = ALPHABET[(value % 85) as usize];
                    value /= 85;
                }
                out.extend(digits[..chunk.len() + 1].iter().map(|&d| d as char));
            }
            out
        };
        assert_eq!(deobfuscate(&encode(&xored), b"mask").unwrap(), "the key");
        assert_eq!(base85("Xk~0{Zv"), Some(b"hello".to_vec()));
        assert_eq!(base85("not base85 \""), None);
    }

    /// Reads this computer's rekordbox library, fetching the key:
    /// `cargo test -p deckhand-app real_library -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_library() {
        let started = std::time::Instant::now();
        let library = open_any(&default_path().unwrap(), "").unwrap().library;
        let grids = library.tracks.iter().filter(|t| !t.tempo.is_empty()).count();
        let cues: usize = library.tracks.iter().map(|t| t.cues.len()).sum();
        println!(
            "{} tracks ({grids} with a grid, {cues} cues), {} top-level playlists, {:.1} s",
            library.tracks.len(),
            library.playlists.len(),
            started.elapsed().as_secs_f64()
        );
    }
}
