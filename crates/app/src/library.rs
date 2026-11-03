//! The library of a USB/SD export: playlists and tracks from `export.pdb`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use deckhand_formats::pdb::{Library, LibraryTrack, PlaylistKind, PlaylistNode};

/// A playlist entry in the sidebar, flattened with its nesting depth.
#[derive(Debug, Clone)]
pub struct PlaylistRow {
    /// The playlist's or folder's id in `export.pdb`.
    pub id: u32,
    pub name: String,
    pub depth: usize,
    /// `None` for folders.
    pub track_ids: Option<Vec<u32>>,
}

/// An opened export.
pub struct UsbLibrary {
    pub root: PathBuf,
    pub library: Library,
    pub playlists: Vec<PlaylistRow>,
    /// The selected playlist (index into `playlists`); `None`: all tracks.
    pub playlist: Option<usize>,
    /// Folders closed in the sidebar (indexes into `playlists`).
    pub collapsed: HashSet<usize>,
    /// Its section in the sidebar is open.
    pub open: bool,
    /// A drive of its own (a USB stick or SD card) that can be ejected,
    /// rather than a folder on another disk.
    pub removable: bool,
}

impl UsbLibrary {
    /// Opens the export whose `PIONEER` directory is directly below `root`.
    pub fn open(root: &Path) -> Result<Self> {
        let pdb = root.join("PIONEER/rekordbox/export.pdb");
        let library = Library::open(&pdb).with_context(|| format!("reading {}", pdb.display()))?;
        let mut playlists = Vec::new();
        flatten(&library.playlists, 0, &mut playlists);
        Ok(Self {
            root: root.to_path_buf(),
            library,
            playlists,
            playlist: None,
            collapsed: HashSet::new(),
            open: true,
            removable: is_drive(root),
        })
    }

    /// The name shown for the medium.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned())
    }

    /// Tracks of a playlist (or the whole collection for `None`), filtered
    /// by a case-insensitive search over title, artist, album and genre,
    /// each with its position in the list before searching.
    pub fn numbered_tracks(
        &self,
        playlist: Option<usize>,
        search: &str,
    ) -> Vec<(usize, &LibraryTrack)> {
        let needle = search.trim().to_lowercase();
        let matches = |t: &&LibraryTrack| {
            needle.is_empty()
                || [Some(&t.track.title), t.artist.as_ref(), t.album.as_ref(), t.genre.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|field| field.to_lowercase().contains(&needle))
        };
        match playlist.and_then(|i| self.playlists.get(i)).and_then(|p| p.track_ids.as_ref()) {
            Some(ids) => ids
                .iter()
                .enumerate()
                .filter_map(|(i, id)| self.library.track(*id).map(|t| (i, t)))
                .filter(|(_, t)| matches(t))
                .collect(),
            None => self.library.tracks.iter().enumerate().filter(|(_, t)| matches(t)).collect(),
        }
    }

    /// The export's database.
    pub fn pdb_path(&self) -> PathBuf {
        self.root.join("PIONEER/rekordbox/export.pdb")
    }

    /// The stick also holds rekordbox's newer OneLibrary (Device Library
    /// Plus), which deckhand cannot update.
    pub fn has_one_library(&self) -> bool {
        self.root.join("PIONEER/rekordbox/exportLibrary.db").is_file()
    }

    /// Absolute path of a file referenced from the export (`/Contents/…`).
    pub fn resolve(&self, export_path: &str) -> PathBuf {
        self.root.join(export_path.trim_start_matches('/'))
    }
}

fn flatten(nodes: &[PlaylistNode], depth: usize, out: &mut Vec<PlaylistRow>) {
    for node in nodes {
        match &node.kind {
            PlaylistKind::Folder(children) => {
                out.push(PlaylistRow {
                    id: node.id,
                    name: node.name.clone(),
                    depth,
                    track_ids: None,
                });
                flatten(children, depth + 1, out);
            }
            PlaylistKind::Playlist(ids) => {
                out.push(PlaylistRow {
                    id: node.id,
                    name: node.name.clone(),
                    depth,
                    track_ids: Some(ids.clone()),
                });
            }
        }
    }
}

/// Mounted volumes (drive letters on Windows) that contain a rekordbox export.
pub fn find_exports() -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        (b'A'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))).collect()
    } else {
        let Ok(entries) = std::fs::read_dir("/Volumes") else { return Vec::new() };
        entries.filter_map(|e| e.ok().map(|e| e.path())).collect()
    };
    let mut found: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| p.join("PIONEER/rekordbox/export.pdb").is_file())
        .collect();
    found.sort();
    found
}

/// Whether a path is the root of a mounted drive (`/Volumes/NAME`, `E:\`).
fn is_drive(path: &Path) -> bool {
    if cfg!(windows) {
        path.parent().is_none()
    } else {
        path.parent() == Some(Path::new("/Volumes"))
    }
}

/// Safely removes a drive, like the Finder's or Explorer's Eject.
pub fn eject(root: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    return windows_eject(root);
    #[cfg(not(windows))]
    {
        use std::process::Command;
        let output = if cfg!(target_os = "macos") {
            Command::new("diskutil").arg("eject").arg(root).output()
        } else {
            Command::new("umount").arg(root).output()
        }
        .context("starting the eject command")?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            anyhow::bail!(
                "{}",
                if message.is_empty() { "the drive is in use".to_owned() } else { message }
            );
        }
        Ok(())
    }
}

/// Windows: locks and dismounts the volume, then lets it be removed and
/// ejects it, the steps Microsoft gives for safe removal.
#[cfg(windows)]
fn windows_eject(root: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA,
        IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    };

    // `E:\` → `\\.\E:`
    let letter = root.to_string_lossy().trim_end_matches('\\').to_owned();
    let device: Vec<u16> =
        std::ffi::OsStr::new(&format!(r"\\.\{letter}")).encode_wide().chain([0]).collect();
    // SAFETY: plain Win32 calls on a handle this function opens and closes.
    unsafe {
        let handle = CreateFileW(
            device.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            anyhow::bail!("could not open {letter}: {}", std::io::Error::last_os_error());
        }
        let control = |code: u32, input: *const std::ffi::c_void, size: u32| {
            let mut returned = 0u32;
            DeviceIoControl(
                handle,
                code,
                input,
                size,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            ) != 0
        };
        // Locking fails while a file on the drive is open; retry briefly.
        let mut locked = false;
        for _ in 0..20 {
            if control(FSCTL_LOCK_VOLUME, std::ptr::null(), 0) {
                locked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let result = if !locked {
            Err(anyhow::anyhow!("the drive is in use; is a file on it still open?"))
        } else if !control(FSCTL_DISMOUNT_VOLUME, std::ptr::null(), 0) {
            Err(anyhow::anyhow!("could not dismount {letter}: {}", std::io::Error::last_os_error()))
        } else {
            let allow = PREVENT_MEDIA_REMOVAL { PreventMediaRemoval: false };
            let allow_ptr = (&raw const allow).cast();
            control(
                IOCTL_STORAGE_MEDIA_REMOVAL,
                allow_ptr,
                size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
            );
            if control(IOCTL_STORAGE_EJECT_MEDIA, std::ptr::null(), 0) {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "could not eject {letter}: {}",
                    std::io::Error::last_os_error()
                ))
            }
        };
        CloseHandle(handle);
        result
    }
}

/// `m:ss` for a duration in seconds.
pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}
