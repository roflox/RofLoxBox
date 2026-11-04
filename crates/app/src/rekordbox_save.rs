//! Saving deckhand's changes into rekordbox's library (`master.db`): track
//! tags, ratings and colours, hot cues, memory cues and loops, beat grids
//! (in rekordbox's analysis files beside the database), and the playlists
//! of the collection's `rekordbox` folder: made, renamed, moved, reordered
//! or deleted. Tracks put into those playlists that rekordbox doesn't have
//! yet are added to its collection (rekordbox analyses them itself).
//!
//! Rows are written the way rekordbox writes them: each changed track or
//! playlist takes the next number of the library's change count, and the
//! playlists are listed in `masterPlaylists6.xml` too. Saving needs the
//! library as deckhand last read it: when rekordbox changed it since, its
//! changes would be lost, so nothing is written.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use deckhand_formats::anlz::{AnlzFile, Section};
use deckhand_library::{Cue, CueKind, Node, NodeKind, Track};
use rusqlite::{Connection, OptionalExtension, params};

use crate::deck::{KeyNotation, format_key, parse_key};
use crate::rekordbox_db::{HOT_CUE_COLORS, Usn, integer, now, rekordbox_running, write};

/// The collection as it is to be saved.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Every track of the collection; matched to rekordbox's by file path.
    pub tracks: Vec<Track>,
    /// The playlists and folders in the collection's `rekordbox` folder
    /// (the ones at its top without a parent); `None` without that folder,
    /// leaving rekordbox's playlists as they are.
    pub playlists: Option<Vec<Node>>,
    /// rekordbox's change count when deckhand last read the library.
    pub read_count: Option<i64>,
}

/// What a save changed.
#[derive(Debug, Default, PartialEq)]
pub struct Saved {
    /// Tracks whose tags, rating or colour changed.
    pub tracks: usize,
    /// Tracks whose cues changed.
    pub cues: usize,
    /// Tracks whose beat grid changed.
    pub grids: usize,
    /// Tracks new to rekordbox.
    pub added: usize,
    /// Playlists and folders made, changed or deleted.
    pub playlists: usize,
    /// rekordbox's ids of the playlists and folders made, by node id.
    pub ids: Vec<(u32, String)>,
    /// rekordbox's change count after saving.
    pub count: i64,
}

impl Saved {
    pub fn is_empty(&self) -> bool {
        self.tracks + self.cues + self.grids + self.added + self.playlists == 0
    }
}

/// rekordbox is running: its library can't be changed now.
#[derive(Debug)]
pub struct RekordboxRunning;

impl std::fmt::Display for RekordboxRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("close rekordbox first: its library can't be changed while it runs")
    }
}

impl std::error::Error for RekordboxRunning {}

/// Writes `snapshot` into rekordbox's library at `path` where it differs.
/// rekordbox must be closed ([`RekordboxRunning`] otherwise); the library
/// is copied into `backups` first.
pub fn save(path: &Path, key: &str, snapshot: &Snapshot, backups: &Path) -> Result<Saved> {
    if rekordbox_running() {
        return Err(RekordboxRunning.into());
    }
    let Some(read_count) = snapshot.read_count else {
        bail!("read rekordbox's library into deckhand first");
    };
    let share = path.parent().unwrap_or(Path::new("")).join("share");
    let written = write(path, key, backups, |db, usn| {
        if usn.0 != read_count {
            bail!(
                "rekordbox changed its library since deckhand read it, and saving would undo \
                 that. Read the rekordbox library again (deckhand's changes to it are replaced), \
                 then save"
            );
        }
        let mut library = Library::open(db, usn)?;
        let plan = library.save(snapshot, &share)?;
        Ok(plan)
    })?;
    let mut plan = written.result;
    plan.saved.count = written.count;
    if let Some(backup) = &written.backup {
        let list = path.with_file_name("masterPlaylists6.xml");
        if !plan.list.is_empty() && list.is_file() {
            std::fs::copy(&list, backup.join("masterPlaylists6.xml"))?;
            update_playlist_list(&list, &plan.list)?;
        }
        for (dat, track) in &plan.grids {
            write_grid(dat, track, &share, backup)
                .with_context(|| format!("writing the beat grid of {}", track.title))?;
        }
    }
    Ok(plan.saved)
}

/// What is left to do once the database is saved.
#[derive(Default)]
struct Plan {
    saved: Saved,
    /// Changes to `masterPlaylists6.xml`.
    list: Vec<ListChange>,
    /// rekordbox analysis files (`.DAT`) to give the track's grid.
    grids: Vec<(PathBuf, Track)>,
}

/// A change to rekordbox's list of playlists, by playlist id.
#[derive(Debug, Clone, PartialEq)]
enum ListChange {
    Add { id: String, parent: String, attribute: i64 },
    Update { id: String, parent: String },
    Remove(String),
}

/// A track of rekordbox's: its ids and its analysis file.
#[derive(Clone)]
struct Content {
    id: String,
    uuid: String,
    analysis: String,
}

struct Library<'a> {
    db: &'a Connection,
    usn: &'a mut Usn,
    now: String,
    /// Tracks by file path: rekordbox can have a file as more than one.
    contents: HashMap<String, Vec<Content>>,
    /// File paths by track id.
    paths: HashMap<String, String>,
    /// Artist, album, genre and label ids by (table, name).
    names: HashMap<(&'static str, String), String>,
    /// Key ids with the key they stand for.
    keys: Vec<(String, (i32, bool))>,
    notation: KeyNotation,
}

impl<'a> Library<'a> {
    fn open(db: &'a Connection, usn: &'a mut Usn) -> Result<Self> {
        let mut contents = HashMap::new();
        let mut paths = HashMap::new();
        let mut statement = db.prepare(
            "SELECT FolderPath, ID, UUID, AnalysisDataPath FROM djmdContent
             WHERE rb_local_deleted = 0",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let text = |i: usize| -> rusqlite::Result<String> {
                Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default())
            };
            let path = text(0)?;
            paths.insert(text(1)?, path.clone());
            let content = Content { id: text(1)?, uuid: text(2)?, analysis: text(3)? };
            contents.entry(path).or_insert_with(Vec::new).push(content);
        }
        let mut names = HashMap::new();
        for table in ["djmdArtist", "djmdAlbum", "djmdGenre", "djmdLabel"] {
            let mut statement = db.prepare(&format!(
                "SELECT Name, ID FROM {table} WHERE rb_local_deleted = 0 ORDER BY created_at"
            ))?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let name: Option<String> = row.get(0)?;
                names.entry((table, name.unwrap_or_default())).or_insert(row.get(1)?);
            }
        }
        let mut keys = Vec::new();
        let mut notation = KeyNotation::Classic;
        let mut statement =
            db.prepare("SELECT ID, ScaleName FROM djmdKey WHERE rb_local_deleted = 0")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            if name.ends_with(['A', 'B']) && name.starts_with(|c: char| c.is_ascii_digit()) {
                notation = KeyNotation::Alphanumeric;
            }
            if let Some(key) = parse_key(&name) {
                keys.push((row.get(0)?, key));
            }
        }
        drop(rows);
        drop(statement);
        Ok(Self { db, usn, now: now(), contents, paths, names, keys, notation })
    }

    fn save(&mut self, snapshot: &Snapshot, share: &Path) -> Result<Plan> {
        let mut plan = Plan::default();
        let by_id: HashMap<u32, &Track> = snapshot.tracks.iter().map(|t| (t.id, t)).collect();
        // Tracks of the saved playlists rekordbox doesn't have yet.
        let mut wanted: Vec<u32> = Vec::new();
        for node in snapshot.playlists.iter().flatten() {
            for id in &node.tracks {
                if !wanted.contains(id) {
                    wanted.push(*id);
                }
            }
        }
        for id in wanted {
            let Some(track) = by_id.get(&id) else { continue };
            let path = track.path.to_string_lossy().into_owned();
            if !self.contents.contains_key(&path) && track.path.is_file() {
                self.add_track(track)?;
                plan.saved.added += 1;
            }
        }
        // A file rekordbox has twice is unchanged when deckhand has it as
        // either; changed, both get deckhand's.
        for track in &snapshot.tracks {
            let path = track.path.to_string_lossy();
            let Some(rows) = self.contents.get(path.as_ref()).cloned() else { continue };
            let mut differs = true;
            for row in &rows {
                differs &= self.update_track(&row.id, track, false)?;
            }
            if differs {
                for row in &rows {
                    self.update_track(&row.id, track, true)?;
                }
                plan.saved.tracks += 1;
            }
            let mut differs = true;
            for row in &rows {
                differs &= self.save_cues(&row.id, &row.uuid, &track.cues, false)?;
            }
            if differs {
                for row in &rows {
                    self.save_cues(&row.id, &row.uuid, &track.cues, true)?;
                }
                plan.saved.cues += 1;
            }
            let mut grids = Vec::new();
            let mut same = false;
            for row in &rows {
                match grid_differs(&row.analysis, track, share) {
                    Grid::Same => same = true,
                    Grid::Differs(dat) => grids.push((row.id.clone(), dat)),
                    Grid::None => {}
                }
            }
            if !same && !grids.is_empty() {
                for (id, dat) in grids {
                    self.set_bpm(&id, track)?;
                    plan.grids.push((dat, track.clone()));
                }
                plan.saved.grids += 1;
            }
        }
        if let Some(playlists) = &snapshot.playlists {
            self.save_playlists(playlists, &by_id, &mut plan)?;
        }
        Ok(plan)
    }

    /// An id for a new row of `table`: a random number not taken yet, as
    /// rekordbox makes them.
    fn new_id(&self, table: &str, column: &str) -> Result<String> {
        loop {
            let mut bytes = [0u8; 4];
            getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no random numbers: {e}"))?;
            let id = u32::from_le_bytes(bytes);
            if id < 100 {
                continue;
            }
            let id = id.to_string();
            let taken = self
                .db
                .query_row(&format!("SELECT 1 FROM {table} WHERE {column} = ?1"), [&id], |_| Ok(()))
                .optional()?
                .is_some();
            if !taken {
                return Ok(id);
            }
        }
    }

    /// The id of the artist, album, genre or label `name` (`None` when
    /// empty), added when rekordbox doesn't have it.
    fn name_id(&mut self, table: &'static str, name: &str) -> Result<Option<String>> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        if let Some(id) = self.names.get(&(table, name.to_owned())) {
            return Ok(Some(id.clone()));
        }
        let id = self.new_id(table, "ID")?;
        self.db.execute(
            &format!(
                "INSERT INTO {table} (ID, Name, UUID, rb_local_usn, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)"
            ),
            params![id, name, uuid()?, self.usn.next(), self.now],
        )?;
        self.names.insert((table, name.to_owned()), id.clone());
        Ok(Some(id))
    }

    /// The id of rekordbox's key for `text` (`None` when not a key), added
    /// in rekordbox's notation when missing.
    fn key_id(&mut self, text: Option<&str>) -> Result<Option<String>> {
        let Some(key) = text.and_then(parse_key) else { return Ok(None) };
        if let Some((id, _)) = self.keys.iter().find(|(_, k)| *k == key) {
            return Ok(Some(id.clone()));
        }
        let id = self.new_id("djmdKey", "ID")?;
        let seq = self.keys.len() as i64 + 1;
        self.db.execute(
            "INSERT INTO djmdKey (ID, ScaleName, Seq, UUID, rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, format_key(key, self.notation), seq, uuid()?, self.usn.next(), self.now],
        )?;
        self.keys.push((id.clone(), key));
        Ok(Some(id))
    }

    /// Adds a track to rekordbox's collection, as its import of a file does.
    fn add_track(&mut self, track: &Track) -> Result<()> {
        let path = track.path.to_string_lossy().into_owned();
        let extension =
            track.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        let file_type = match extension.as_str() {
            "mp3" => 1,
            "mp4" => 3,
            "m4a" | "aac" => 4,
            "flac" => 5,
            "wav" => 11,
            "aif" | "aiff" => 12,
            _ => return Ok(()),
        };
        let id = self.new_id("djmdContent", "ID")?;
        let file_id = self.new_id("djmdContent", "rb_file_id")?;
        let uuid = uuid()?;
        // This computer, as rekordbox recorded it for its other tracks.
        let device: Option<(Option<String>, Option<String>, Option<i64>)> = self
            .db
            .query_row(
                "SELECT DeviceID, MasterDBID, ContentLink FROM djmdContent
                 WHERE rb_local_deleted = 0 AND DeviceID IS NOT NULL
                 GROUP BY DeviceID, MasterDBID, ContentLink ORDER BY count(*) DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (device, master_db, link) = device.unwrap_or_default();
        let artist = self.name_id("djmdArtist", &track.artist)?;
        let album = self.name_id("djmdAlbum", &track.album)?;
        let genre = self.name_id("djmdGenre", &track.genre)?;
        let label = self.name_id("djmdLabel", &track.label)?;
        let key = self.key_id(track.key.as_deref())?;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let file_name = track.path.file_name().map(|n| n.to_string_lossy().into_owned());
        let file_size = std::fs::metadata(&track.path).map_or(track.file_size, |m| m.len());
        self.db.execute(
            "INSERT INTO djmdContent (ID, FolderPath, FileNameL, Title, ArtistID, AlbumID,
                GenreID, LabelID, KeyID, BPM, Length, TrackNo, BitRate, Commnt, FileType,
                Rating, ReleaseYear, StockDate, ColorID, DJPlayCount, MasterDBID, MasterSongID,
                FileSize, DiscNo, SampleRate, DateCreated, ContentLink, HotCueAutoLoad,
                DeliveryControl, SamplerTrackInfo, SamplerPlayOffset, SamplerGain,
                VideoAssociate, LyricStatus, ServiceID, ExtInfo, rb_file_id, DeviceID, UUID,
                rb_local_usn, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, 0, ?20, ?1, ?21, 0, ?22, ?18, ?23, 'on', 'on', 0, 0, 0.0, '0', 0, 0,
                'null', ?24, ?25, ?26, ?27, ?28, ?28)",
            params![
                id,
                path,
                file_name,
                track.title,
                artist,
                album,
                genre,
                label,
                key,
                track.bpm.map_or(0, |b| (b * 100.0).round() as i64),
                track.duration.round() as i64,
                track.track_number.unwrap_or(0),
                track.bitrate.unwrap_or(0),
                track.comment,
                file_type,
                track.rating.min(5),
                track.year.unwrap_or(0),
                today,
                track.color.filter(|c| (1..=8).contains(c)).unwrap_or(0).to_string(),
                master_db,
                file_size as i64,
                track.sample_rate.unwrap_or(0),
                link,
                file_id,
                device,
                uuid,
                self.usn.next(),
                self.now,
            ],
        )?;
        self.paths.insert(id.clone(), path.clone());
        self.contents.entry(path).or_default().push(Content { id, uuid, analysis: String::new() });
        Ok(())
    }

    /// Gives a track of rekordbox's the collection's tags, rating and
    /// colour where they differ (only tells, without `write`); returns
    /// whether they did.
    fn update_track(&mut self, id: &str, track: &Track, write: bool) -> Result<bool> {
        let current = self.db.query_row(
            "SELECT c.Title, ar.Name, al.Name, g.Name, l.Name, c.Commnt, c.Rating, c.ColorID,
                    k.ScaleName, c.ReleaseYear, c.TrackNo
             FROM djmdContent c
             LEFT JOIN djmdArtist ar ON ar.ID = c.ArtistID
             LEFT JOIN djmdAlbum al ON al.ID = c.AlbumID
             LEFT JOIN djmdGenre g ON g.ID = c.GenreID
             LEFT JOIN djmdLabel l ON l.ID = c.LabelID
             LEFT JOIN djmdKey k ON k.ID = c.KeyID
             WHERE c.ID = ?1",
            [id],
            |row| {
                let text = |i: usize| -> rusqlite::Result<String> {
                    Ok(row.get::<_, Option<String>>(i)?.unwrap_or_default())
                };
                Ok((
                    [text(0)?, text(1)?, text(2)?, text(3)?, text(4)?, text(5)?],
                    integer(row, 6)?,
                    integer(row, 7)?,
                    text(8)?,
                    integer(row, 9)?,
                    integer(row, 10)?,
                ))
            },
        )?;
        let (texts, rating, color, key, year, number) = current;
        let wanted =
            [&track.title, &track.artist, &track.album, &track.genre, &track.label, &track.comment];
        let color_wanted = i64::from(track.color.filter(|c| (1..=8).contains(c)).unwrap_or(0));
        // A key in another notation is the same key.
        let key_same = match (parse_key(&key), track.key.as_deref().and_then(parse_key)) {
            (Some(a), Some(b)) => a == b,
            (_, None) => true,
            (None, Some(_)) => false,
        };
        let same = texts.iter().zip(wanted).all(|(a, b)| a.trim() == b.trim())
            && rating == i64::from(track.rating.min(5))
            && color == color_wanted
            && key_same
            && year == i64::from(track.year.unwrap_or(0))
            && number == i64::from(track.track_number.unwrap_or(0));
        if same || !write {
            return Ok(!same);
        }
        let artist = self.name_id("djmdArtist", &track.artist)?;
        let album = self.name_id("djmdAlbum", &track.album)?;
        let genre = self.name_id("djmdGenre", &track.genre)?;
        let label = self.name_id("djmdLabel", &track.label)?;
        let key_id = if key_same { None } else { self.key_id(track.key.as_deref())? };
        self.db.execute(
            "UPDATE djmdContent SET Title = ?2, ArtistID = ?3, AlbumID = ?4, GenreID = ?5,
                LabelID = ?6, Commnt = ?7, Rating = ?8, ColorID = ?9,
                KeyID = coalesce(?10, KeyID), ReleaseYear = ?11, TrackNo = ?12,
                rb_local_usn = ?13, updated_at = ?14
             WHERE ID = ?1",
            params![
                id,
                track.title,
                artist,
                album,
                genre,
                label,
                track.comment,
                track.rating.min(5),
                color_wanted.to_string(),
                key_id,
                track.year.unwrap_or(0),
                track.track_number.unwrap_or(0),
                self.usn.next(),
                self.now,
            ],
        )?;
        Ok(true)
    }

    /// Gives a track of rekordbox's the collection's hot cues, memory cues
    /// and loops (only tells, without `write`); returns whether they
    /// changed. rekordbox's other cue kinds stay.
    fn save_cues(&mut self, id: &str, uuid: &str, cues: &[Cue], write: bool) -> Result<bool> {
        // (row id, kind, in, out, colour index, comment)
        let mut statement = self.db.prepare(
            "SELECT ID, Kind, InMsec, OutMsec, ColorTableIndex, Comment FROM djmdCue
             WHERE ContentID = ?1 AND rb_local_deleted = 0 ORDER BY InMsec",
        )?;
        let mut existing: Vec<(String, i64, i64, i64, i64, String)> = statement
            .query_map([id], |row| {
                Ok((
                    row.get(0)?,
                    integer(row, 1)?,
                    integer(row, 2)?,
                    integer(row, 3)?,
                    integer(row, 4)?,
                    row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(statement);
        existing.retain(|c| c.1 == 0 || crate::rekordbox_db::hot_cue_slot(c.1).is_some());
        let mut changed = false;
        let mut used = vec![false; existing.len()];
        for cue in cues {
            let kind = match cue.kind {
                CueKind::Memory => 0,
                CueKind::Hot(slot @ 0..=2) => i64::from(slot) + 1,
                CueKind::Hot(slot @ 3..=7) => i64::from(slot) + 2,
                CueKind::Hot(_) => continue,
            };
            let start = (cue.start * 1000.0).round().max(0.0) as i64;
            let end = cue.end.map_or(-1, |e| (e * 1000.0).round().max(0.0) as i64);
            let color = match cue.kind {
                CueKind::Hot(_) => cue.color.map(nearest_hot_cue_color),
                CueKind::Memory => None,
            };
            let found = existing
                .iter()
                .enumerate()
                .position(|(i, c)| !used[i] && c.1 == kind && (c.2 - start).abs() <= 1);
            match found {
                Some(i) => {
                    used[i] = true;
                    let row = &existing[i];
                    let out = if row.3 > 0 { row.3 } else { -1 };
                    let color = color.unwrap_or(row.4);
                    if out != end || row.4 != color || row.5 != cue.name {
                        if !write {
                            return Ok(true);
                        }
                        self.db.execute(
                            "UPDATE djmdCue SET OutMsec = ?2, OutFrame = ?3, ColorTableIndex = ?4,
                                Comment = ?5, BeatLoopSize = 0, updated_at = ?6
                             WHERE ID = ?1",
                            params![row.0, end, frames(end), color, cue.name, self.now],
                        )?;
                        changed = true;
                    }
                }
                None => {
                    if !write {
                        return Ok(true);
                    }
                    let looped = end > start;
                    let cue_id = self.new_id("djmdCue", "ID")?;
                    self.db.execute(
                        "INSERT INTO djmdCue (ID, ContentID, InMsec, InFrame, InMpegFrame,
                            InMpegAbs, OutMsec, OutFrame, OutMpegFrame, OutMpegAbs, Kind, Color,
                            ColorTableIndex, ActiveLoop, Comment, BeatLoopSize, CueMicrosec,
                            InPointSeekInfo, OutPointSeekInfo, ContentUUID, UUID, created_at,
                            updated_at)
                         VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6, 0, 0, ?7, ?8, ?9, 0, ?10, 0, 0,
                            '', '', ?11, ?12, ?13, ?13)",
                        params![
                            cue_id,
                            id,
                            start,
                            frames(start),
                            if looped { end } else { -1 },
                            if looped { frames(end) } else { 0 },
                            kind,
                            // rekordbox's own: -1 for a plain memory cue.
                            if kind == 0 && !looped { -1 } else { 255 },
                            color.unwrap_or(0),
                            cue.name,
                            uuid,
                            self::uuid()?,
                            self.now,
                        ],
                    )?;
                    changed = true;
                }
            }
        }
        for (i, row) in existing.iter().enumerate() {
            if !used[i] {
                if !write {
                    return Ok(true);
                }
                self.db.execute("DELETE FROM djmdCue WHERE ID = ?1", [&row.0])?;
                changed = true;
            }
        }
        if changed {
            self.db.execute(
                "UPDATE djmdContent SET CueUpdated = CAST(coalesce(CueUpdated, 0) + 1 AS TEXT),
                    rb_local_usn = ?2, updated_at = ?3
                 WHERE ID = ?1",
                params![id, self.usn.next(), self.now],
            )?;
        }
        Ok(changed)
    }

    /// Sets the BPM of a track of rekordbox's to its new beat grid's.
    fn set_bpm(&mut self, id: &str, track: &Track) -> Result<()> {
        let bpm = track.tempo.first().map_or(0, |m| (m.bpm * 100.0).round() as i64);
        self.db.execute(
            "UPDATE djmdContent SET BPM = ?2, rb_local_usn = ?3, updated_at = ?4 WHERE ID = ?1",
            params![id, bpm, self.usn.next(), self.now],
        )?;
        Ok(())
    }

    /// Makes rekordbox's playlist tree that of `nodes`: rekordbox's
    /// intelligent playlists, which deckhand doesn't show, keep their place.
    fn save_playlists(
        &mut self,
        nodes: &[Node],
        tracks: &HashMap<u32, &Track>,
        plan: &mut Plan,
    ) -> Result<()> {
        // (parent, seq, name, attribute) by id.
        let mut statement = self.db.prepare(
            "SELECT ID, ParentID, Seq, Name, Attribute FROM djmdPlaylist
             WHERE rb_local_deleted = 0 ORDER BY Seq",
        )?;
        let rows: Vec<(String, String, i64, String, i64)> = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_else(|| "root".into()),
                    integer(row, 2)?,
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    integer(row, 4)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(statement);
        let attribute = |kind: &NodeKind| match kind {
            NodeKind::Playlist => 0,
            NodeKind::Folder => 1,
        };
        // Which of rekordbox's playlists each node is (made when none).
        let mut ids: HashMap<u32, String> = HashMap::new();
        let mut claimed: HashSet<String> = HashSet::new();
        for node in nodes {
            if let Some(id) = &node.rekordbox
                && rows.iter().any(|r| &r.0 == id && r.4 == attribute(&node.kind))
                && claimed.insert(id.clone())
            {
                ids.insert(node.id, id.clone());
            }
        }

        // Deleted in deckhand: gone from rekordbox, with what they hold.
        let mut doomed: Vec<String> = rows
            .iter()
            .filter(|r| matches!(r.4, 0 | 1) && !claimed.contains(&r.0))
            .map(|r| r.0.clone())
            .collect();
        let mut i = 0;
        while i < doomed.len() {
            let parent = doomed[i].clone();
            for row in rows.iter().filter(|r| r.1 == parent) {
                if !doomed.contains(&row.0) {
                    doomed.push(row.0.clone());
                }
            }
            i += 1;
        }
        for id in &doomed {
            self.db.execute("DELETE FROM djmdSongPlaylist WHERE PlaylistID = ?1", [id])?;
            self.db.execute("DELETE FROM djmdPlaylist WHERE ID = ?1", [id])?;
            self.usn.next();
            plan.list.push(ListChange::Remove(id.clone()));
            plan.saved.playlists += 1;
        }

        // Nodes new to rekordbox get their ids now.
        for node in nodes {
            if let std::collections::hash_map::Entry::Vacant(slot) = ids.entry(node.id) {
                let id = loop {
                    let id = self.new_id("djmdPlaylist", "ID")?;
                    if claimed.insert(id.clone()) {
                        break id;
                    }
                };
                slot.insert(id.clone());
                plan.saved.ids.push((node.id, id));
            }
        }
        let made: HashSet<String> = plan.saved.ids.iter().map(|(_, id)| id.clone()).collect();
        // (node, its id, its parent's id), parents first.
        let mut order: Vec<(&Node, String, String)> = Vec::new();
        fn walk<'n>(
            nodes: &'n [Node],
            parent: Option<u32>,
            parent_id: &str,
            ids: &HashMap<u32, String>,
            out: &mut Vec<(&'n Node, String, String)>,
            depth: usize,
        ) {
            if depth > 32 {
                return;
            }
            for node in nodes.iter().filter(|n| n.parent == parent) {
                let id = ids[&node.id].clone();
                out.push((node, id.clone(), parent_id.to_owned()));
                if node.kind == NodeKind::Folder {
                    walk(nodes, Some(node.id), &id, ids, out, depth + 1);
                }
            }
        }
        walk(nodes, None, "root", &ids, &mut order, 0);

        // Each parent's children in order, with rekordbox's intelligent
        // playlists where they were.
        let mut parents: Vec<String> = vec!["root".into()];
        parents.extend(order.iter().filter(|o| o.0.kind == NodeKind::Folder).map(|o| o.1.clone()));
        for parent in &parents {
            let mut children: Vec<String> =
                order.iter().filter(|o| &o.2 == parent).map(|o| o.1.clone()).collect();
            let siblings = rows.iter().filter(|r| &r.1 == parent && !doomed.contains(&r.0));
            for (at, row) in siblings.enumerate() {
                if !matches!(row.4, 0 | 1) {
                    children.insert(at.min(children.len()), row.0.clone());
                }
            }
            for (i, id) in children.iter().enumerate() {
                let seq = i as i64 + 1;
                let node = order.iter().find(|o| &o.1 == id).map(|o| o.0);
                let row = rows.iter().find(|r| &r.0 == id);
                match (node, row) {
                    (Some(node), _) if made.contains(id) => {
                        let attribute = attribute(&node.kind);
                        self.db.execute(
                            "INSERT INTO djmdPlaylist (ID, Seq, Name, ImagePath, Attribute,
                                ParentID, SmartList, UUID, rb_local_usn, created_at, updated_at)
                             VALUES (?1, ?2, ?3, '', ?4, ?5, '', ?6, ?7, ?8, ?8)",
                            params![
                                id,
                                seq,
                                node.name,
                                attribute,
                                parent,
                                uuid()?,
                                self.usn.next(),
                                self.now
                            ],
                        )?;
                        plan.list.push(ListChange::Add {
                            id: id.clone(),
                            parent: parent.clone(),
                            attribute,
                        });
                        plan.saved.playlists += 1;
                    }
                    (Some(node), Some(row)) => {
                        if row.1 != *parent || row.3 != node.name {
                            self.db.execute(
                                "UPDATE djmdPlaylist SET Seq = ?2, Name = ?3, ParentID = ?4,
                                    rb_local_usn = ?5, updated_at = ?6
                                 WHERE ID = ?1",
                                params![id, seq, node.name, parent, self.usn.next(), self.now],
                            )?;
                            plan.list.push(ListChange::Update {
                                id: id.clone(),
                                parent: parent.clone(),
                            });
                            plan.saved.playlists += 1;
                        } else if row.2 != seq {
                            self.db.execute(
                                "UPDATE djmdPlaylist SET Seq = ?2, rb_local_usn = ?3,
                                    updated_at = ?4
                                 WHERE ID = ?1",
                                params![id, seq, self.usn.next(), self.now],
                            )?;
                            plan.saved.playlists += 1;
                        }
                    }
                    (None, Some(row)) if row.2 != seq => {
                        // rekordbox's own, moved along by the others.
                        self.db.execute(
                            "UPDATE djmdPlaylist SET Seq = ?2, updated_at = ?3 WHERE ID = ?1",
                            params![id, seq, self.now],
                        )?;
                    }
                    _ => {}
                }
            }
        }

        // The playlists' tracks.
        for (node, id, _) in &order {
            if node.kind != NodeKind::Playlist {
                continue;
            }
            let wanted: Vec<String> = node
                .tracks
                .iter()
                .filter_map(|t| tracks.get(t))
                .map(|t| t.path.to_string_lossy().into_owned())
                .filter(|path| self.contents.contains_key(path))
                .collect();
            if self.save_entries(id, &wanted)? && !made.contains(id) {
                self.db.execute(
                    "UPDATE djmdPlaylist SET rb_local_usn = ?2, updated_at = ?3 WHERE ID = ?1",
                    params![id, self.usn.next(), self.now],
                )?;
                plan.list.push(ListChange::Update { id: id.clone(), parent: String::new() });
                plan.saved.playlists += 1;
            }
        }
        Ok(())
    }

    /// Makes a playlist's entries `wanted` (file paths, in order), keeping
    /// the rows of tracks that stay, and entries of tracks deckhand doesn't
    /// have (streamed ones) where they were; returns whether anything
    /// changed.
    fn save_entries(&mut self, playlist: &str, wanted: &[String]) -> Result<bool> {
        let mut statement = self.db.prepare(
            "SELECT ID, ContentID, TrackNo FROM djmdSongPlaylist
             WHERE PlaylistID = ?1 AND rb_local_deleted = 0 ORDER BY TrackNo",
        )?;
        let rows: Vec<(String, String, i64)> = statement
            .query_map([playlist], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    integer(row, 2)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(statement);
        // An entry: a row kept as it is, or a file.
        #[derive(PartialEq)]
        enum Entry<'e> {
            Kept(usize),
            File(&'e str),
        }
        let path_of = |row: &(String, String, i64)| {
            self.paths.get(&row.1).filter(|p| Path::new(p).is_absolute()).map(String::as_str)
        };
        let current: Vec<Entry> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| path_of(row).map_or(Entry::Kept(i), Entry::File))
            .collect();
        let mut entries: Vec<Entry> = wanted.iter().map(|p| Entry::File(p)).collect();
        for (i, entry) in current.iter().enumerate() {
            if let Entry::Kept(row) = entry {
                entries.insert(i.min(entries.len()), Entry::Kept(*row));
            }
        }
        let numbered = rows.iter().enumerate().all(|(i, r)| r.2 == i as i64 + 1);
        if numbered && entries == current {
            return Ok(false);
        }
        let mut pool: HashMap<&str, VecDeque<&(String, String, i64)>> = HashMap::new();
        for row in &rows {
            if let Some(path) = path_of(row) {
                pool.entry(path).or_default().push_back(row);
            }
        }
        for (i, entry) in entries.iter().enumerate() {
            let number = i as i64 + 1;
            let row = match entry {
                Entry::Kept(row) => Some(&rows[*row]),
                Entry::File(path) => pool.get_mut(path).and_then(VecDeque::pop_front),
            };
            match (row, entry) {
                (Some(row), _) if row.2 == number => {}
                (Some(row), _) => {
                    self.db.execute(
                        "UPDATE djmdSongPlaylist SET TrackNo = ?2, rb_local_usn = ?3,
                            updated_at = ?4
                         WHERE ID = ?1",
                        params![row.0, number, self.usn.next(), self.now],
                    )?;
                }
                (None, Entry::File(path)) => {
                    let content = self.contents[*path][0].id.clone();
                    self.db.execute(
                        "INSERT INTO djmdSongPlaylist (ID, PlaylistID, ContentID, TrackNo, UUID,
                            rb_local_usn, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        params![
                            uuid()?,
                            playlist,
                            content,
                            number,
                            uuid()?,
                            self.usn.next(),
                            self.now
                        ],
                    )?;
                }
                (None, Entry::Kept(_)) => {}
            }
        }
        for row in pool.values().flatten() {
            self.db.execute("DELETE FROM djmdSongPlaylist WHERE ID = ?1", [&row.0])?;
        }
        Ok(true)
    }
}

/// How a track's beat grid compares with the one in rekordbox's analysis
/// file.
enum Grid {
    Same,
    /// Changed in deckhand: the analysis file to write it into.
    Differs(PathBuf),
    /// No grid of rekordbox's to change (the track's is deckhand's own).
    None,
}

fn grid_differs(analysis: &str, track: &Track, share: &Path) -> Grid {
    if analysis.is_empty() || track.tempo.is_empty() {
        return Grid::None;
    }
    let dat = share.join(analysis.trim_start_matches('/'));
    let Some(grid) = AnlzFile::open(&dat).ok().and_then(|f| f.beat_grid().cloned()) else {
        return Grid::None;
    };
    let original = crate::usb_import::grid_markers(&grid);
    if original.is_empty() {
        Grid::None
    } else if crate::usb_export::same_markers(&original, &track.tempo) {
        Grid::Same
    } else {
        Grid::Differs(dat)
    }
}

/// rekordbox's colour number nearest to a hot cue's colour.
fn nearest_hot_cue_color(rgb: [u8; 3]) -> i64 {
    let distance =
        |c: &[u8; 3]| -> i32 { (0..3).map(|i| (i32::from(c[i]) - i32::from(rgb[i])).pow(2)).sum() };
    HOT_CUE_COLORS.iter().min_by_key(|(_, c)| distance(c)).map_or(0, |(index, _)| *index)
}

/// A time in rekordbox's frames (150 a second), as it stores beside
/// milliseconds.
fn frames(ms: i64) -> i64 {
    if ms < 0 { 0 } else { ms * 150 / 1000 }
}

/// A random UUID (version 4), as rekordbox gives its rows.
fn uuid() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no random numbers: {e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
}

/// A playlist id as `masterPlaylists6.xml` writes it: hexadecimal, `0` for
/// the top.
fn list_id(id: &str) -> String {
    if id == "root" {
        return "0".into();
    }
    id.parse::<u64>().map_or_else(|_| id.to_owned(), |n| format!("{n:X}"))
}

/// Applies `changes` to rekordbox's `masterPlaylists6.xml`, keeping the
/// rest of the file as it is.
fn update_playlist_list(path: &Path, changes: &[ListChange]) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let updated = changed_playlist_list(&text, changes, chrono::Utc::now().timestamp_millis());
    let temporary = path.with_extension("xml.deckhand");
    std::fs::write(&temporary, updated)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn changed_playlist_list(text: &str, changes: &[ListChange], millis: i64) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split(newline).map(str::to_owned).collect();
    let find = |lines: &[String], id: &str| {
        let needle = format!("<NODE Id=\"{}\"", list_id(id));
        lines.iter().position(|l| l.trim_start().starts_with(&needle))
    };
    let set = |line: &str, name: &str, value: &str| -> String {
        let key = format!(" {name}=\"");
        match line.find(&key) {
            Some(start) => {
                let from = start + key.len();
                let end = line[from..].find('"').map_or(line.len(), |e| from + e);
                format!("{}{value}{}", &line[..from], &line[end..])
            }
            None => line.to_owned(),
        }
    };
    for change in changes {
        match change {
            ListChange::Add { id, parent, attribute } => {
                if find(&lines, id).is_some() {
                    continue;
                }
                let line = format!(
                    "    <NODE Id=\"{}\" ParentId=\"{}\" Attribute=\"{attribute}\" \
                     Timestamp=\"{millis}\" Lib_Type=\"0\" CheckType=\"0\"/>",
                    list_id(id),
                    list_id(parent)
                );
                let at = lines
                    .iter()
                    .position(|l| l.trim_start().starts_with("</PLAYLISTS>"))
                    .unwrap_or(lines.len());
                lines.insert(at, line);
            }
            ListChange::Update { id, parent } => {
                if let Some(i) = find(&lines, id) {
                    let mut line = set(&lines[i], "Timestamp", &millis.to_string());
                    if !parent.is_empty() {
                        line = set(&line, "ParentId", &list_id(parent));
                    }
                    lines[i] = line;
                }
            }
            ListChange::Remove(id) => {
                if let Some(i) = find(&lines, id) {
                    lines.remove(i);
                }
            }
        }
    }
    lines.join(newline)
}

/// Writes a track's beat grid into rekordbox's analysis file `dat` (and
/// drops the summary of the old grid from the `.EXT` beside it), keeping
/// copies of both in `backup`.
fn write_grid(dat: &Path, track: &Track, share: &Path, backup: &Path) -> Result<()> {
    let mut file = AnlzFile::open(dat)?;
    let ext_path = dat.with_extension("EXT");
    let ext = AnlzFile::open(&ext_path).ok();
    for original in [Some(dat), ext.as_ref().map(|_| ext_path.as_path())].into_iter().flatten() {
        let relative = original.strip_prefix(share).unwrap_or(original);
        let copy = backup.join("share").join(relative);
        if let Some(dir) = copy.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(original, &copy)?;
    }
    crate::usb_export::replace_grid(&mut file, crate::usb_export::collection_grid(track));
    replace_file(dat, &file.to_bytes())?;
    if let Some(mut ext) = ext {
        ext.sections.retain(|s| !matches!(s, Section::ExtendedBeatGrid(_)));
        replace_file(&ext_path, &ext.to_bytes())?;
    }
    Ok(())
}

fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("deckhand");
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rekordbox_db::tests::library;
    use deckhand_formats::anlz::{Beat, BeatGrid};
    use deckhand_library::{Collection, ScannedFile};

    const LIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\r\n\
        <MASTER_PLAYLIST Version=\"3.0.0\" AutomaticSync=\"0\">\r\n  <PLAYLISTS>\r\n\
        \x20   <NODE Id=\"f1\" ParentId=\"0\" Attribute=\"1\" Timestamp=\"0\" Lib_Type=\"0\" CheckType=\"0\"/>\r\n\
        \x20   <NODE Id=\"p1\" ParentId=\"f1\" Attribute=\"0\" Timestamp=\"0\" Lib_Type=\"0\" CheckType=\"0\"/>\r\n\
        \x20   <NODE Id=\"p2\" ParentId=\"0\" Attribute=\"0\" Timestamp=\"0\" Lib_Type=\"0\" CheckType=\"0\"/>\r\n\
        \x20   <NODE Id=\"s1\" ParentId=\"0\" Attribute=\"4\" Timestamp=\"0\" Lib_Type=\"0\" CheckType=\"0\"/>\r\n\
        \x20 </PLAYLISTS>\r\n</MASTER_PLAYLIST>\r\n";

    /// A rekordbox library in a new folder: `master.db` (track `t1` with an
    /// analysis file holding a 174 BPM grid), `masterPlaylists6.xml`, and a
    /// new audio file `New.mp3`.
    fn folder(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("deckhand-rbsave-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("share/PIONEER/USBANLZ/a")).unwrap();
        library(&root.join("master.db"), "secret");
        let db = crate::rekordbox_db::connect(&root.join("master.db"), "secret", true).unwrap();
        db.execute(
            "UPDATE djmdContent SET AnalysisDataPath = '/PIONEER/USBANLZ/a/ANLZ0000.DAT'
             WHERE ID = 't1'",
            [],
        )
        .unwrap();
        drop(db);
        std::fs::write(root.join("masterPlaylists6.xml"), LIST).unwrap();
        let beats = (0..8)
            .map(|i| Beat {
                bar_position: i % 4 + 1,
                tempo: 17400,
                time_ms: 100 + u32::from(i) * 345,
            })
            .collect();
        let dat = AnlzFile {
            sections: vec![
                Section::Path("/music/One.wav".into()),
                Section::BeatGrid(BeatGrid { beats }),
            ],
        };
        std::fs::write(root.join("share/PIONEER/USBANLZ/a/ANLZ0000.DAT"), dat.to_bytes()).unwrap();
        std::fs::write(root.join("New.mp3"), b"not really audio").unwrap();
        root
    }

    /// The collection with rekordbox's library at `root` read into it.
    fn collection(root: &Path) -> Collection {
        let opened = crate::rekordbox_db::open_any(&root.join("master.db"), "secret").unwrap();
        let mut collection = Collection::default();
        collection.import_rekordbox(opened.library);
        collection.rekordbox_count = Some(opened.count);
        collection
    }

    fn snapshot(c: &Collection) -> Snapshot {
        Snapshot {
            tracks: c.tracks.clone(),
            playlists: c.rekordbox_playlists(),
            read_count: c.rekordbox_count,
        }
    }

    fn plain(root: &Path) -> Connection {
        crate::rekordbox_db::connect(&root.join("master.db"), "secret", false).unwrap()
    }

    fn node(c: &Collection, name: &str) -> u32 {
        c.nodes.iter().find(|n| n.name == name).unwrap().id
    }

    #[test]
    fn saves_tags_cues_grids_tracks_and_playlists() {
        let root = folder("all");
        let backups = root.join("backups");
        let mut c = collection(&root);
        let id = |c: &Collection, path: &str| {
            c.tracks.iter().find(|t| t.path == Path::new(path)).unwrap().id
        };
        let (one, two) = (id(&c, "/music/One.wav"), id(&c, "/music/Two.wav"));
        assert_eq!(c.track(one).unwrap().tempo.len(), 1, "the grid was read");
        c.update_track(one, |t| {
            t.title = "One (edit)".into();
            t.artist = "Someone".into();
            t.genre = "Drum & Bass".into();
            t.rating = 5;
            t.color = Some(6);
            // The same key in the other notation: no change.
            t.key = Some("Am".into());
            t.cues[0].start = 1.5;
            t.cues[1].name = "build".into();
            t.cues.push(Cue {
                kind: CueKind::Memory,
                start: 10.0,
                end: None,
                color: None,
                name: String::new(),
            });
            t.tempo[0].start = 0.12;
        });
        c.merge(vec![ScannedFile {
            path: root.join("New.mp3"),
            title: "New".into(),
            artist: "Artist".into(),
            album: String::new(),
            genre: String::new(),
            label: String::new(),
            comment: String::new(),
            year: Some(2025),
            track_number: None,
            duration: 181.4,
            bpm: Some(172.0),
            key: Some("5A".into()),
            sample_rate: Some(44100),
            bitrate: Some(320),
            has_artwork: false,
            file_size: 16,
            modified: 0,
        }]);
        let new = id(&c, root.join("New.mp3").to_str().unwrap());
        let friday = node(&c, "Friday");
        c.rename(friday, "Saturday");
        c.move_in_playlist(friday, &[1], 0);
        c.delete(node(&c, "All"));
        let made = c.create("Made", NodeKind::Playlist, Some(node(&c, "Sets")));
        c.add_to_playlist(made, &[new, one]);

        let saved = save(&root.join("master.db"), "secret", &snapshot(&c), &backups).unwrap();
        assert_eq!((saved.tracks, saved.cues, saved.grids, saved.added), (1, 1, 1, 1), "{saved:?}");
        assert_eq!(saved.ids.len(), 1);
        assert_eq!(saved.ids[0].0, made);

        let db = plain(&root);
        let count: i64 = db.query_row("SELECT int_1 FROM agentRegistry", [], |r| r.get(0)).unwrap();
        assert_eq!(count, saved.count);
        assert!(count > 500);
        let row: (String, String, String, i64, String, String, i64) = db
            .query_row(
                "SELECT c.Title, ar.Name, g.Name, c.Rating, c.ColorID, c.KeyID, c.BPM
                 FROM djmdContent c JOIN djmdArtist ar ON ar.ID = c.ArtistID
                 JOIN djmdGenre g ON g.ID = c.GenreID WHERE c.ID = 't1'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "One (edit)".into(),
                "Someone".into(),
                "Drum & Bass".into(),
                5,
                "6".into(),
                "k1".into(),
                17400
            )
        );
        let untouched: i64 = db
            .query_row("SELECT rb_local_usn FROM djmdContent WHERE ID = 't2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(untouched, 401);

        let mut statement = db
            .prepare(
                "SELECT ID, Kind, InMsec, OutMsec, Comment, InFrame FROM djmdCue ORDER BY InMsec",
            )
            .unwrap();
        let cues: Vec<(String, i64, i64, i64, String, Option<i64>)> = statement
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let kinds: Vec<(i64, i64, i64, &str)> =
            cues.iter().map(|c| (c.1, c.2, c.3, c.4.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (4, 100, 900, ""),
                (1, 1500, -1, ""),
                (5, 2000, -1, "build"),
                (0, 3000, 5000, ""),
                (0, 10000, -1, "")
            ]
        );
        assert_eq!(cues[2].0, "c2", "a changed cue keeps its row");
        assert_eq!(cues[1].5, Some(225), "frames beside milliseconds");

        let (new_id, file_type, device): (String, i64, String) = db
            .query_row(
                "SELECT ID, FileType, DeviceID FROM djmdContent WHERE FileNameL = 'New.mp3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((file_type, device.as_str()), (1, "dev"));

        let entries = |playlist: &str| -> Vec<(String, String)> {
            let mut statement = db
                .prepare("SELECT ID, ContentID FROM djmdSongPlaylist WHERE PlaylistID = ?1 ORDER BY TrackNo")
                .unwrap();
            statement
                .query_map([playlist], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(entries("p1"), [("e2".into(), "t1".into()), ("e1".into(), "t2".into())]);
        assert!(entries("p2").is_empty());
        let made_id = &saved.ids[0].1;
        let made_entries: Vec<String> = entries(made_id).into_iter().map(|e| e.1).collect();
        assert_eq!(made_entries, [new_id, "t1".to_owned()]);
        let mut statement = db
            .prepare("SELECT ID, ParentID, Seq, Name FROM djmdPlaylist ORDER BY ParentID, Seq")
            .unwrap();
        let playlists: Vec<(String, String, i64, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            playlists,
            [
                ("p1".into(), "f1".into(), 1, "Saturday".into()),
                (made_id.clone(), "f1".into(), 2, "Made".into()),
                ("f1".into(), "root".into(), 1, "Sets".into()),
                ("s1".into(), "root".into(), 2, "Smart".into()),
            ]
        );

        let list = std::fs::read_to_string(root.join("masterPlaylists6.xml")).unwrap();
        assert!(!list.contains("Id=\"p2\""));
        let made_hex = format!("{:X}", made_id.parse::<u64>().unwrap());
        assert!(list.contains(&format!("<NODE Id=\"{made_hex}\" ParentId=\"f1\" Attribute=\"0\"")));
        assert!(!list.contains("Id=\"p1\" ParentId=\"f1\" Attribute=\"0\" Timestamp=\"0\""));
        assert!(list.ends_with("</MASTER_PLAYLIST>\r\n"));

        let dat = AnlzFile::open(root.join("share/PIONEER/USBANLZ/a/ANLZ0000.DAT")).unwrap();
        assert_eq!(dat.beat_grid().unwrap().beats[0].time_ms, 120);
        let backup = std::fs::read_dir(&backups).unwrap().next().unwrap().unwrap().path();
        for kept in ["master.db", "masterPlaylists6.xml", "share/PIONEER/USBANLZ/a/ANLZ0000.DAT"] {
            assert!(backup.join(kept).is_file(), "{kept} backed up");
        }

        // Saved again: nothing left to change, nothing written.
        c.saved_to_rekordbox(&saved.ids, saved.count);
        let again = save(&root.join("master.db"), "secret", &snapshot(&c), &backups).unwrap();
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 1);

        // Read back, the library is what deckhand had.
        let reread = collection(&root);
        let one = reread.tracks.iter().find(|t| t.path == Path::new("/music/One.wav")).unwrap();
        assert_eq!(one.cues.len(), 4);
        assert!((one.tempo[0].start - 0.12).abs() < 0.001);
        let _ = std::fs::remove_dir_all(&root);
        let _ = two;
    }

    #[test]
    fn refuses_a_library_changed_since_read() {
        let root = folder("changed");
        let mut c = collection(&root);
        c.rekordbox_count = Some(499);
        let first = c.tracks[0].id;
        c.update_track(first, |t| t.rating = 1);
        let before = std::fs::read(root.join("master.db")).unwrap();
        let err =
            save(&root.join("master.db"), "secret", &snapshot(&c), &root.join("b")).unwrap_err();
        assert!(err.to_string().contains("changed its library"), "{err}");
        c.rekordbox_count = None;
        assert!(save(&root.join("master.db"), "secret", &snapshot(&c), &root.join("b")).is_err());
        assert_eq!(std::fs::read(root.join("master.db")).unwrap(), before);
        // Without the rekordbox folder, rekordbox's playlists stay.
        let mut c = collection(&root);
        c.delete(node(&c, "rekordbox"));
        let saved =
            save(&root.join("master.db"), "secret", &snapshot(&c), &root.join("b")).unwrap();
        assert!(saved.is_empty(), "{saved:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Saves into a copy of a real rekordbox library (`master.db`,
    /// `masterPlaylists6.xml`, `share/`) with a copy of deckhand's collection
    /// beside it: unchanged first (nothing may change), then with a few
    /// edits. `DECKHAND_RB_COPY=folder cargo test -p deckhand-app
    /// real_copy_save -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_copy_save() {
        let root = PathBuf::from(std::env::var("DECKHAND_RB_COPY").unwrap());
        let path = root.join("master.db");
        let key = crate::rekordbox_db::fetch_key().unwrap();
        let backups = root.join("backups");
        let mut c = Collection::open(&root.join("collection.json")).unwrap();
        let opened = crate::rekordbox_db::open_any(&path, &key).unwrap();
        let (tracks, cues) = (
            opened.library.tracks.len(),
            opened.library.tracks.iter().map(|t| t.cues.len()).sum::<usize>(),
        );
        let rows =
            |path: &Path| opened.library.tracks.iter().filter(|t| t.file.path == path).count();
        let rows: HashMap<PathBuf, usize> =
            c.tracks.iter().map(|t| (t.path.clone(), rows(&t.path))).collect();
        c.import_rekordbox(opened.library);
        c.rekordbox_count = Some(opened.count);
        let saved = save(&path, &key, &snapshot(&c), &backups).unwrap();
        println!("unchanged: {saved:?}");
        // Only tags edited in deckhand go over.
        let edited = c.tracks.iter().filter(|t| !t.edited.is_empty()).count();
        assert!(saved.tracks <= edited);
        assert_eq!((saved.cues, saved.grids, saved.added, saved.playlists), (0, 0, 0, 0));
        c.saved_to_rekordbox(&saved.ids, saved.count);

        // A hot cue, a renamed and reordered playlist, a new one.
        let playlists = c.rekordbox_playlists().unwrap();
        let list =
            playlists.iter().find(|n| n.kind == NodeKind::Playlist && n.tracks.len() > 2).unwrap();
        let (list, name, first) = (list.id, list.name.clone(), list.tracks[0]);
        let track = c.track(first).unwrap().clone();
        let free = (0..8).find(|s| !track.cues.iter().any(|q| q.kind == CueKind::Hot(*s))).unwrap();
        c.update_track(first, |t| {
            t.cues.push(Cue {
                kind: CueKind::Hot(free),
                start: 30.0,
                end: None,
                color: Some([230, 40, 40]),
                name: "deckhand".into(),
            })
        });
        c.rename(list, &format!("{name} (deckhand)"));
        c.move_in_playlist(list, &[0], 3);
        let top = c.nodes.iter().find(|n| n.name == "rekordbox" && n.parent.is_none()).unwrap().id;
        let made = c.create("deckhand test", NodeKind::Playlist, Some(top));
        // A track rekordbox doesn't have, and a beat grid moved.
        let new = c
            .tracks
            .iter()
            .find(|t| {
                rows[&t.path] == 0
                    && t.path.is_file()
                    && t.path.extension().is_some_and(|e| e == "mp3" || e == "wav")
            })
            .map(|t| t.id);
        c.add_to_playlist(made, &[first]);
        c.add_to_playlist(made, &new.into_iter().collect::<Vec<_>>());
        let gridded = c
            .tracks
            .iter()
            .find(|t| rows[&t.path] == 1 && !t.tempo.is_empty() && t.analysis.is_some())
            .map(|t| t.id)
            .unwrap();
        c.update_track(gridded, |t| t.tempo[0].start += 0.01);
        // The new track's cues go along.
        let new_cues = usize::from(new.is_some_and(|n| !c.track(n).unwrap().cues.is_empty()));
        let saved = save(&path, &key, &snapshot(&c), &backups).unwrap();
        println!("edited: {saved:?}");
        assert_eq!(
            (saved.cues, saved.ids.len(), saved.tracks, saved.grids, saved.added),
            (1 + new_cues, 1, 0, 1, usize::from(new.is_some()))
        );
        c.saved_to_rekordbox(&saved.ids, saved.count);
        let again = save(&path, &key, &snapshot(&c), &backups).unwrap();
        assert!(again.is_empty(), "{again:?}");

        let reread = crate::rekordbox_db::open_any(&path, &key).unwrap();
        assert_eq!(reread.library.tracks.len(), tracks + usize::from(new.is_some()));
        // A file rekordbox has twice gets the cue on both.
        assert_eq!(
            reread.library.tracks.iter().map(|t| t.cues.len()).sum::<usize>(),
            cues + rows[&track.path] + new.map_or(0, |n| c.track(n).unwrap().cues.len())
        );
        let mut back = Collection::default();
        back.import_rekordbox(reread.library);
        let cue = back
            .tracks
            .iter()
            .find(|t| t.path == track.path)
            .unwrap()
            .cues
            .iter()
            .find(|q| q.kind == CueKind::Hot(free))
            .cloned();
        println!("cue read back: {cue:?}");
        assert_eq!(cue.map(|q| (q.start, q.name)), Some((30.0, "deckhand".into())));
        let renamed = back.nodes.iter().find(|n| n.name == format!("{name} (deckhand)")).unwrap();
        let original = c.node(list).unwrap();
        let paths = |c: &Collection, ids: &[u32]| -> Vec<PathBuf> {
            ids.iter().map(|i| c.track(*i).unwrap().path.clone()).collect()
        };
        assert_eq!(paths(&back, &renamed.tracks), paths(&c, &original.tracks));
        assert!(
            back.nodes
                .iter()
                .any(|n| n.name == "deckhand test"
                    && n.tracks.len() == 1 + usize::from(new.is_some()))
        );
        let moved = c.track(gridded).unwrap();
        let back_grid = &back.tracks.iter().find(|t| t.path == moved.path).unwrap().tempo;
        println!("grid {:?} -> {:?}", moved.tempo.first(), back_grid.first());
        assert!((back_grid[0].start - moved.tempo[0].start).abs() < 0.002);
        println!("{} tracks, {} playlists read back", back.tracks.len(), back.nodes.len());
    }

    #[test]
    fn edits_the_playlist_list_in_place() {
        let changes = [
            ListChange::Remove("p2".into()),
            ListChange::Add { id: "3092974715".into(), parent: "root".into(), attribute: 0 },
            ListChange::Update { id: "p1".into(), parent: "root".into() },
        ];
        let text = changed_playlist_list(LIST, &changes, 1234);
        assert!(text.contains("<NODE Id=\"B85B0C7B\" ParentId=\"0\" Attribute=\"0\" Timestamp=\"1234\" Lib_Type=\"0\" CheckType=\"0\"/>\r\n  </PLAYLISTS>"));
        assert!(text.contains("<NODE Id=\"p1\" ParentId=\"0\" Attribute=\"0\" Timestamp=\"1234\""));
        assert!(!text.contains("p2"));
        assert_eq!(text.matches("\r\n").count(), LIST.matches("\r\n").count());
    }

    #[test]
    fn hot_cue_colours_and_ids() {
        assert_eq!(nearest_hot_cue_color([40, 226, 20]), 22);
        assert_eq!(nearest_hot_cue_color([250, 30, 30]), 42);
        let id = uuid().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_eq!(frames(22070), 3310);
        assert_eq!(frames(-1), 0);
    }
}
