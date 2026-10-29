//! Which deck controls are shown, and where: rows in the main area of a
//! deck and a column beside it. Saved in the config; edited from the View
//! menu and by dragging in layout-editing mode.

use serde::{Deserialize, Serialize};

/// One piece of a deck's controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeckElement {
    Header,
    Title,
    Info,
    Overview,
    Cue,
    Play,
    Sync,
    Master,
    Quantize,
    Vinyl,
    Slip,
    LoopInOut,
    LoopResize,
    BeatLoops,
    BeatJump,
    Gate,
    MemoryCall,
    MemorySet,
    PadModes,
    PadShift,
    Pads,
    TempoRange,
    MasterTempo,
    KeyShift,
    KeySyncTap,
    TempoFader,
    TempoReset,
    Nudge,
    GridEdit,
}

impl DeckElement {
    pub const ALL: [DeckElement; 29] = [
        Self::Header,
        Self::Title,
        Self::Info,
        Self::Overview,
        Self::Cue,
        Self::Play,
        Self::Sync,
        Self::Master,
        Self::Quantize,
        Self::Vinyl,
        Self::Slip,
        Self::LoopInOut,
        Self::LoopResize,
        Self::BeatLoops,
        Self::BeatJump,
        Self::Gate,
        Self::MemoryCall,
        Self::MemorySet,
        Self::PadModes,
        Self::PadShift,
        Self::Pads,
        Self::TempoRange,
        Self::MasterTempo,
        Self::KeyShift,
        Self::KeySyncTap,
        Self::TempoFader,
        Self::TempoReset,
        Self::Nudge,
        Self::GridEdit,
    ];

    /// Groups for the View menu: (name, elements).
    pub const GROUPS: [(&'static str, &'static [DeckElement]); 7] = [
        ("Track info", &[Self::Header, Self::Title, Self::Info, Self::Overview]),
        (
            "Transport",
            &[
                Self::Cue,
                Self::Play,
                Self::Sync,
                Self::Master,
                Self::Quantize,
                Self::Vinyl,
                Self::Slip,
            ],
        ),
        ("Loops", &[Self::LoopInOut, Self::LoopResize, Self::BeatLoops, Self::BeatJump]),
        ("Cues", &[Self::Gate, Self::MemoryCall, Self::MemorySet]),
        ("Pads", &[Self::PadModes, Self::PadShift, Self::Pads]),
        (
            "Tempo",
            &[
                Self::TempoRange,
                Self::MasterTempo,
                Self::TempoFader,
                Self::TempoReset,
                Self::Nudge,
                Self::GridEdit,
            ],
        ),
        ("Key", &[Self::KeyShift, Self::KeySyncTap]),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Header => "Deck name",
            Self::Title => "Title and artist",
            Self::Info => "BPM, key and time",
            Self::Overview => "Overview waveform",
            Self::Cue => "CUE",
            Self::Play => "Play/pause",
            Self::Sync => "SYNC",
            Self::Master => "MASTER",
            Self::Quantize => "Quantize (Q)",
            Self::Vinyl => "VINYL",
            Self::Slip => "SLIP",
            Self::LoopInOut => "Loop IN / OUT / RELOOP",
            Self::LoopResize => "Loop ½ / ×2",
            Self::BeatLoops => "Beat loops 1 4 8 16",
            Self::BeatJump => "Beat jump ◀ ▶",
            Self::Gate => "GATE",
            Self::MemoryCall => "Memory call ◀ ▶",
            Self::MemorySet => "MEMORY / DELETE",
            Self::PadModes => "Pad modes",
            Self::PadShift => "Pad SHIFT and parameters",
            Self::Pads => "Performance pads",
            Self::TempoRange => "Tempo range",
            Self::MasterTempo => "Master Tempo (MT)",
            Self::KeyShift => "Key shift − +",
            Self::KeySyncTap => "KEY sync and TAP",
            Self::TempoFader => "Tempo fader",
            Self::TempoReset => "Tempo reset",
            Self::Nudge => "Nudge − +",
            Self::GridEdit => "Beat grid edit",
        }
    }
}

/// Where a moved element goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// In front of (or, with `after`, behind) another element.
    Next { to: DeckElement, after: bool },
    /// At the end of a main-area row.
    RowEnd(usize),
    /// On a new row at the bottom of the main area.
    NewRow,
    /// At the bottom of the side column.
    SideEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeckLayout {
    /// Rows of the main area, top to bottom.
    pub main: Vec<Vec<DeckElement>>,
    /// The column beside it (the tempo column by default).
    pub side: Vec<DeckElement>,
    /// Elements not shown; they keep their place for when shown again.
    pub hidden: Vec<DeckElement>,
}

/// The "Default" preset: the controller does the transport, loops, memory
/// cues and key, so the screen keeps what it shows best.
impl Default for DeckLayout {
    fn default() -> Self {
        use DeckElement as E;
        Self {
            hidden: vec![
                E::LoopInOut,
                E::LoopResize,
                E::BeatLoops,
                E::BeatJump,
                E::Cue,
                E::Play,
                E::MemoryCall,
                E::MemorySet,
                E::PadShift,
                E::KeyShift,
                E::KeySyncTap,
                E::TempoFader,
            ],
            ..Self::everything()
        }
    }
}

/// A view saved under a name (View → View presets): the deck elements and,
/// for presets saved since view presets exist, the rest of the view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutPreset {
    pub name: String,
    pub layout: DeckLayout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewSettings>,
}

/// Everything in the View menu besides the deck elements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    pub vertical_waveforms: bool,
    pub show_waveforms: bool,
    pub show_mixer: bool,
    pub show_library: bool,
    pub show_fx: bool,
    pub screen_layout: crate::screen_layout::ScreenLayout,
    pub hidden_mixer_parts: Vec<crate::parts::MixerPart>,
    pub hidden_fx_parts: Vec<crate::parts::FxPart>,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            vertical_waveforms: false,
            show_waveforms: true,
            show_mixer: true,
            show_library: true,
            show_fx: true,
            screen_layout: Default::default(),
            hidden_mixer_parts: Vec::new(),
            hidden_fx_parts: Vec::new(),
        }
    }
}

impl ViewSettings {
    /// The same view (hidden parts in any order).
    pub fn matches(&self, other: &Self) -> bool {
        fn same<T: PartialEq>(a: &[T], b: &[T]) -> bool {
            a.len() == b.len() && a.iter().all(|x| b.contains(x))
        }
        let plain = |v: &Self| {
            (
                v.vertical_waveforms,
                v.show_waveforms,
                v.show_mixer,
                v.show_library,
                v.show_fx,
                v.screen_layout.clone(),
            )
        };
        plain(self) == plain(other)
            && same(&self.hidden_mixer_parts, &other.hidden_mixer_parts)
            && same(&self.hidden_fx_parts, &other.hidden_fx_parts)
    }
}

impl LayoutPreset {
    /// Presets that come with the app.
    pub fn built_in() -> Vec<LayoutPreset> {
        let preset = |name: &str, layout, view| LayoutPreset {
            name: name.to_owned(),
            layout,
            view: Some(view),
        };
        vec![
            preset("Default", DeckLayout::default(), ViewSettings::default()),
            preset("Everything", DeckLayout::everything(), ViewSettings::default()),
            // The controller is the mixer: waveforms between the decks, no
            // mixer on screen.
            preset(
                "Vertical, mixer on the controller",
                DeckLayout::default(),
                ViewSettings {
                    vertical_waveforms: true,
                    show_mixer: false,
                    ..ViewSettings::default()
                },
            ),
        ]
    }
}

impl DeckLayout {
    /// Every element shown.
    pub fn everything() -> Self {
        use DeckElement as E;
        Self {
            main: vec![
                vec![E::Header],
                vec![E::Title],
                vec![E::Info, E::GridEdit],
                vec![E::Overview],
                vec![E::Cue, E::Play, E::Sync, E::Master, E::Quantize, E::Vinyl, E::Slip],
                vec![E::LoopInOut, E::LoopResize, E::BeatLoops, E::BeatJump],
                vec![E::Gate, E::MemoryCall, E::MemorySet],
                vec![E::PadModes],
                vec![E::PadShift],
                vec![E::Pads],
            ],
            side: vec![
                E::TempoRange,
                E::MasterTempo,
                E::KeyShift,
                E::KeySyncTap,
                E::TempoFader,
                E::TempoReset,
                E::Nudge,
            ],
            hidden: Vec::new(),
        }
    }
}

impl DeckLayout {
    /// The four-deck default (the same as for two decks).
    pub fn four_decks() -> Self {
        Self::default()
    }

    pub fn is_shown(&self, element: DeckElement) -> bool {
        !self.hidden.contains(&element)
    }

    pub fn set_shown(&mut self, element: DeckElement, shown: bool) {
        self.hidden.retain(|e| *e != element);
        if !shown {
            self.hidden.push(element);
        }
    }

    /// Adds elements a saved layout does not know yet (from a newer
    /// version) and drops duplicates and empty rows.
    pub fn repair(&mut self) {
        let mut seen = Vec::new();
        for row in &mut self.main {
            row.retain(|e| {
                let new = !seen.contains(e);
                seen.push(*e);
                new
            });
        }
        self.side.retain(|e| {
            let new = !seen.contains(e);
            seen.push(*e);
            new
        });
        self.main.retain(|row| !row.is_empty());
        let missing: Vec<DeckElement> =
            DeckElement::ALL.into_iter().filter(|e| !seen.contains(e)).collect();
        if !missing.is_empty() {
            self.main.push(missing);
        }
    }

    fn remove(&mut self, element: DeckElement) {
        for row in &mut self.main {
            row.retain(|e| *e != element);
        }
        self.side.retain(|e| *e != element);
    }

    /// Moves an element; it is shown afterwards.
    pub fn move_to(&mut self, element: DeckElement, place: Place) {
        if place == (Place::Next { to: element, after: false })
            || place == (Place::Next { to: element, after: true })
        {
            return;
        }
        // Emptied rows are dropped only at the end, so row numbers hold.
        self.remove(element);
        match place {
            Place::Next { to, after } => {
                let offset = usize::from(after);
                if let Some(row) = self.main.iter_mut().find(|r| r.contains(&to)) {
                    let at = row.iter().position(|e| *e == to).unwrap_or(0) + offset;
                    row.insert(at, element);
                } else if let Some(at) = self.side.iter().position(|e| *e == to) {
                    self.side.insert(at + offset, element);
                } else {
                    self.main.push(vec![element]);
                }
            }
            Place::RowEnd(row) if row < self.main.len() => self.main[row].push(element),
            Place::RowEnd(_) | Place::NewRow => self.main.push(vec![element]),
            Place::SideEnd => self.side.push(element),
        }
        self.main.retain(|row| !row.is_empty());
        self.set_shown(element, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeckElement as E;

    #[test]
    fn groups_hold_every_element_once() {
        let grouped: Vec<DeckElement> =
            DeckElement::GROUPS.iter().flat_map(|(_, e)| e.iter().copied()).collect();
        assert_eq!(grouped.len(), DeckElement::ALL.len());
        assert!(DeckElement::ALL.iter().all(|e| grouped.contains(e)));
    }

    #[test]
    fn default_has_every_element_once() {
        let mut layout = DeckLayout::default();
        let before = layout.clone();
        layout.repair();
        assert_eq!(layout, before);
    }

    #[test]
    fn moving_elements() {
        let mut layout = DeckLayout::default();
        layout.move_to(E::Play, Place::Next { to: E::Cue, after: false });
        assert_eq!(&layout.main[4][..2], [E::Play, E::Cue]);
        layout.move_to(E::TempoFader, Place::NewRow);
        assert_eq!(layout.main.last().unwrap(), &vec![E::TempoFader]);
        assert!(!layout.side.contains(&E::TempoFader));
        // The header row empties and disappears.
        layout.move_to(E::Header, Place::SideEnd);
        assert_eq!(layout.main[0], vec![E::Title]);
        assert_eq!(layout.side.last(), Some(&E::Header));
        layout.set_shown(E::Slip, false);
        layout.move_to(E::Slip, Place::Next { to: E::Header, after: true });
        assert!(layout.is_shown(E::Slip), "moving shows it again");
    }

    #[test]
    fn repair_adds_unknown_elements() {
        let mut layout =
            DeckLayout { main: vec![vec![E::Play, E::Play]], side: vec![], hidden: vec![] };
        layout.repair();
        assert_eq!(layout.main[0], vec![E::Play]);
        assert_eq!(layout.main[1].len(), DeckElement::ALL.len() - 1);
    }
}
