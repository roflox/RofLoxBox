//! Which deck controls are shown, and where: rows in the main area of a
//! deck and a column beside it. Saved in the config; edited from the View
//! menu and by dragging in layout-editing mode.

use serde::{Deserialize, Serialize};

/// A section of a deck's controls: drawn as one panel, moved as one in
/// the layout editor; its controls can be hidden one by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Section {
    /// Name, title, info, overview: not in a panel.
    Track,
    /// CUE and Play: side by side, or one on the other ([`DeckLayout::stacked`]).
    Play,
    Transport,
    /// SHIFT, GATE and GRID (next to the transport by default).
    Functions,
    Loop,
    Jump,
    Memory,
    Pads,
    Tempo,
    Key,
}

impl Section {
    pub const ALL: [Section; 10] = [
        Self::Track,
        Self::Play,
        Self::Transport,
        Self::Functions,
        Self::Loop,
        Self::Jump,
        Self::Memory,
        Self::Pads,
        Self::Tempo,
        Self::Key,
    ];

    /// The name on its panel ("" for none).
    pub fn title(self) -> &'static str {
        match self {
            Self::Track | Self::Play | Self::Transport | Self::Functions | Self::Pads => "",
            Self::Loop => "LOOP",
            Self::Jump => "BEAT JUMP",
            Self::Memory => "MEMORY",
            Self::Tempo => "TEMPO",
            Self::Key => "KEY",
        }
    }

    /// Its name in menus.
    pub fn label(self) -> &'static str {
        match self {
            Self::Track => "Track info",
            Self::Play => "CUE / Play",
            Self::Transport => "Transport",
            Self::Functions => "SHIFT / GATE / GRID",
            Self::Loop => "Loop",
            Self::Jump => "Beat jump",
            Self::Memory => "Memory",
            Self::Pads => "Pads",
            Self::Tempo => "Tempo",
            Self::Key => "Key",
        }
    }

    /// Its controls, line by line, as they are arranged in it.
    pub fn lines(self) -> &'static [&'static [DeckElement]] {
        use DeckElement as E;
        match self {
            Self::Track => &[&[E::Header], &[E::Title], &[E::Info], &[E::Overview]],
            Self::Play => &[&[E::Cue, E::Play]],
            Self::Transport => &[&[E::Sync, E::Master, E::Quantize, E::Vinyl, E::Slip]],
            Self::Loop => &[&[E::LoopInOut, E::LoopResize, E::BeatLoops]],
            Self::Jump => &[&[E::BeatJump]],
            Self::Memory => &[&[E::MemoryCall, E::MemorySet]],
            Self::Functions => &[&[E::PadShift, E::Gate, E::GridEdit]],
            Self::Pads => &[&[E::PadModes], &[E::Pads]],
            Self::Tempo => &[
                &[E::TempoDisplay],
                &[E::TempoRange],
                &[E::MasterTempo],
                &[E::TempoFader],
                &[E::TempoReset],
                &[E::Nudge],
            ],
            Self::Key => &[&[E::KeyShift], &[E::KeySyncTap]],
        }
    }

    /// A column of buttons as wide as the side column, wherever it is.
    pub fn narrow(self) -> bool {
        matches!(self, Self::Tempo | Self::Key)
    }

    /// Whether it can be drawn one on the other instead of side by side.
    pub fn can_stack(self) -> bool {
        self == Self::Play
    }

    pub fn elements(self) -> impl Iterator<Item = DeckElement> {
        self.lines().iter().flat_map(|line| line.iter().copied())
    }
}

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
    /// The BPM in big in a ring (like rekordbox's jog), with − + for tiny
    /// tempo steps.
    TempoDisplay,
    TempoReset,
    Nudge,
    GridEdit,
}

impl DeckElement {
    #[cfg(test)]
    pub const ALL: [DeckElement; 30] = [
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
        Self::TempoDisplay,
        Self::TempoReset,
        Self::Nudge,
        Self::GridEdit,
    ];

    /// Groups for the View menu, as the decks' sections: (name, elements).
    pub const GROUPS: [(&'static str, &'static [DeckElement]); 10] = [
        ("Track info", &[Self::Header, Self::Title, Self::Info, Self::Overview]),
        ("CUE / Play", &[Self::Cue, Self::Play]),
        ("Transport", &[Self::Sync, Self::Master, Self::Quantize, Self::Vinyl, Self::Slip]),
        ("Loop", &[Self::LoopInOut, Self::LoopResize, Self::BeatLoops]),
        ("Beat jump", &[Self::BeatJump]),
        ("Memory", &[Self::MemoryCall, Self::MemorySet]),
        ("SHIFT / GATE / GRID", &[Self::PadShift, Self::Gate, Self::GridEdit]),
        ("Pads", &[Self::PadModes, Self::Pads]),
        (
            "Tempo",
            &[
                Self::TempoDisplay,
                Self::TempoRange,
                Self::MasterTempo,
                Self::TempoFader,
                Self::TempoReset,
                Self::Nudge,
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
            Self::BeatJump => "Beat jump ◀ − + ▶",
            Self::Gate => "GATE",
            Self::MemoryCall => "◀ MEMORY ▶",
            Self::MemorySet => "Memory SET / DELETE",
            Self::PadModes => "Pad modes",
            Self::PadShift => "Pad SHIFT",
            Self::Pads => "Performance pads",
            Self::TempoRange => "Tempo range",
            Self::MasterTempo => "Master Tempo (MT)",
            Self::KeyShift => "Key shift − +",
            Self::KeySyncTap => "KEY sync and TAP",
            Self::TempoFader => "Tempo fader",
            Self::TempoDisplay => "BPM with − +",
            Self::TempoReset => "Tempo reset",
            Self::Nudge => "Nudge − +",
            Self::GridEdit => "Beat grid edit",
        }
    }
}

/// Where a moved section goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// In front of (or, with `after`, behind) another section.
    Next { to: Section, after: bool },
    /// At the end of a main-area row.
    RowEnd(usize),
    /// On a new row at the bottom of the main area.
    NewRow,
    /// At the bottom of the side column.
    SideEnd,
}

/// Where a deck's sections are, and the controls not shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeckLayout {
    /// Rows of the main area, top to bottom: sections side by side.
    pub main: Vec<Vec<Section>>,
    /// The column beside it (tempo and key by default).
    pub side: Vec<Section>,
    /// Controls not shown; a section with none shown is not drawn.
    pub hidden: Vec<DeckElement>,
    /// Sections drawn one on the other (CUE over Play) rather than side by
    /// side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stacked: Vec<Section>,
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
    /// Presets that come with the app; "Default" is the one for the deck
    /// count (`four`).
    pub fn built_in(four: bool) -> Vec<LayoutPreset> {
        let default = if four { DeckLayout::four_decks() } else { DeckLayout::default() };
        let preset = |name: &str, layout, view| LayoutPreset {
            name: name.to_owned(),
            layout,
            view: Some(view),
        };
        vec![
            preset("Default", default.clone(), ViewSettings::default()),
            preset("Everything", DeckLayout::everything(), ViewSettings::default()),
            // The controller is the mixer: waveforms between the decks, no
            // mixer on screen.
            preset(
                "Vertical, mixer on the controller",
                default,
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
    /// Every control shown.
    pub fn everything() -> Self {
        use Section as S;
        Self {
            main: vec![
                vec![S::Track],
                vec![S::Play, S::Transport, S::Functions],
                vec![S::Loop],
                vec![S::Jump, S::Memory],
                vec![S::Pads],
            ],
            side: vec![S::Tempo, S::Key],
            hidden: Vec::new(),
            stacked: Vec::new(),
        }
    }

    /// The four-deck default: every control on screen (loops, beat jump,
    /// the pads' ◀ ▶, key, tempo fader…), since four decks are more than a
    /// two-deck controller has buttons for.
    pub fn four_decks() -> Self {
        let mut layout = Self::everything();
        // − + by the BPM instead, as rekordbox has.
        layout.set_shown(DeckElement::TempoFader, false);
        layout
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

    /// Whether any of a section's controls is shown.
    pub fn section_shown(&self, section: Section) -> bool {
        section.elements().any(|e| self.is_shown(e))
    }

    /// Drops duplicates and empty rows, and puts back sections a saved
    /// layout lacks (where the full layout has them).
    pub fn repair(&mut self) {
        let mut seen = Vec::new();
        for row in &mut self.main {
            row.retain(|s| {
                let new = !seen.contains(s);
                seen.push(*s);
                new
            });
        }
        self.side.retain(|s| {
            let new = !seen.contains(s);
            seen.push(*s);
            new
        });
        self.main.retain(|row| !row.is_empty());
        let everything = Self::everything();
        for section in Section::ALL.into_iter().filter(|s| !seen.contains(s)) {
            if everything.side.contains(&section) {
                self.side.push(section);
                continue;
            }
            // Next to a neighbour it has in the full layout, else on a row
            // of its own.
            let row = everything.main.iter().find(|row| row.contains(&section));
            let placed = row.and_then(|row| {
                let at = row.iter().position(|s| *s == section)?;
                row.iter().enumerate().filter(|(i, _)| *i != at).find_map(|(i, neighbour)| {
                    let r = self.main.iter().position(|r| r.contains(neighbour))?;
                    let k = self.main[r].iter().position(|s| s == neighbour)?;
                    Some((r, if i < at { k + 1 } else { k }))
                })
            });
            match placed {
                Some((r, k)) => self.main[r].insert(k, section),
                None => self.main.push(vec![section]),
            }
        }
    }

    fn remove(&mut self, section: Section) {
        for row in &mut self.main {
            row.retain(|s| *s != section);
        }
        self.side.retain(|s| *s != section);
    }

    /// Moves a section.
    pub fn move_to(&mut self, section: Section, place: Place) {
        if matches!(place, Place::Next { to, .. } if to == section) {
            return;
        }
        // Emptied rows are dropped only at the end, so row numbers hold.
        self.remove(section);
        match place {
            Place::Next { to, after } => {
                let offset = usize::from(after);
                if let Some(row) = self.main.iter_mut().find(|r| r.contains(&to)) {
                    let at = row.iter().position(|s| *s == to).unwrap_or(0) + offset;
                    row.insert(at, section);
                } else if let Some(at) = self.side.iter().position(|s| *s == to) {
                    self.side.insert(at + offset, section);
                } else {
                    self.main.push(vec![section]);
                }
            }
            Place::RowEnd(row) if row < self.main.len() => self.main[row].push(section),
            Place::RowEnd(_) | Place::NewRow => self.main.push(vec![section]),
            Place::SideEnd => self.side.push(section),
        }
        self.main.retain(|row| !row.is_empty());
    }

    pub fn is_stacked(&self, section: Section) -> bool {
        self.stacked.contains(&section)
    }

    pub fn set_stacked(&mut self, section: Section, stacked: bool) {
        self.stacked.retain(|s| *s != section);
        if stacked {
            self.stacked.push(section);
        }
    }

    /// Hides (or shows again) all of a section's controls.
    pub fn set_section_shown(&mut self, section: Section, shown: bool) {
        for element in section.elements() {
            self.set_shown(element, shown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The View menu's groups are the decks' sections.
    #[test]
    fn groups_are_sections() {
        for (_, elements) in DeckElement::GROUPS {
            let section = Section::ALL.into_iter().find(|s| s.elements().any(|e| e == elements[0]));
            let own: Vec<DeckElement> = section.unwrap().elements().collect();
            assert!(elements.iter().all(|e| own.contains(e)) && own.len() == elements.len(), "{elements:?}");
        }
    }

    #[test]
    fn groups_hold_every_element_once() {
        let grouped: Vec<DeckElement> =
            DeckElement::GROUPS.iter().flat_map(|(_, e)| e.iter().copied()).collect();
        assert_eq!(grouped.len(), DeckElement::ALL.len());
        assert!(DeckElement::ALL.iter().all(|e| grouped.contains(e)));
    }

    #[test]
    fn four_deck_default_shows_everything() {
        let four = DeckLayout::four_decks();
        assert!(DeckElement::ALL.iter().all(|e| *e == DeckElement::TempoFader || four.is_shown(*e)));
        assert!(!DeckLayout::default().is_shown(DeckElement::MemoryCall));
        let default = |four| LayoutPreset::built_in(four).remove(0).layout;
        assert_eq!(default(true), four);
        assert_eq!(default(false), DeckLayout::default());
    }

    #[test]
    fn sections_hold_every_element_once() {
        let all: Vec<DeckElement> = Section::ALL.iter().flat_map(|s| s.elements()).collect();
        assert_eq!(all.len(), DeckElement::ALL.len());
        assert!(DeckElement::ALL.iter().all(|e| all.contains(e)));
    }

    #[test]
    fn default_has_every_section_once() {
        let mut layout = DeckLayout::default();
        let before = layout.clone();
        layout.repair();
        assert_eq!(layout, before);
    }

    #[test]
    fn moving_sections() {
        use Section as S;
        let mut layout = DeckLayout::everything();
        layout.move_to(S::Memory, Place::Next { to: S::Loop, after: true });
        assert_eq!(layout.main[2], vec![S::Loop, S::Memory]);
        assert_eq!(layout.main[3], vec![S::Jump]);
        layout.move_to(S::Tempo, Place::NewRow);
        assert_eq!(layout.main.last().unwrap(), &vec![S::Tempo]);
        assert_eq!(layout.side, vec![S::Key]);
        // A row left empty goes.
        layout.move_to(S::Jump, Place::SideEnd);
        assert!(!layout.main.iter().any(|r| r.is_empty()));
        assert_eq!(layout.side.last(), Some(&S::Jump));
        layout.set_section_shown(S::Pads, false);
        assert!(!layout.section_shown(S::Pads));
    }

    #[test]
    fn repair_puts_back_missing_sections() {
        use Section as S;
        let mut layout =
            DeckLayout { main: vec![vec![S::Loop, S::Loop]], side: vec![], hidden: vec![], stacked: vec![] };
        layout.repair();
        assert_eq!(layout.main[0], vec![S::Loop]);
        let all: usize = layout.main.iter().map(Vec::len).sum::<usize>() + layout.side.len();
        assert_eq!(all, Section::ALL.len());
        assert!(layout.side.contains(&S::Tempo));
        // One the full layout has by another goes back next to it.
        let mut layout = DeckLayout::everything();
        layout.remove(S::Play);
        layout.repair();
        assert_eq!(layout.main[1], vec![S::Play, S::Transport, S::Functions]);
    }
}
