//! Mixing from the computer keyboard, with the keys of rekordbox's
//! "Performance 1" preset: left hand for the left deck, right hand for the
//! right one. Keys go by position, so the layout is the same on any
//! keyboard; in 4-deck mode they follow the side's active deck. Preferences
//! can move them; only the changes are saved, so defaults stay here.

use std::collections::{BTreeMap, HashMap};

use deckhand_audio::Command;
use deckhand_control::{DeckAction, GlobalAction, Value};
use eframe::egui::{self, Event, Key, Modifiers, RichText};

use super::DeckhandApp;
use crate::config::Preferences;
use crate::prefs_view::PrefsTab;

/// A tempo key's move, as a share of the original tempo.
const TEMPO_STEP: f64 = 0.001;
/// Jog steps per frame while a pitch bend key is held, as the on-screen nudge.
const BEND_STEPS: i32 = 3;
/// The width of the scrolling waveforms when zoom is reset.
pub(super) const DEFAULT_SECONDS_VISIBLE: f64 = 8.0;

/// The modifiers a binding asks for; others do not trigger it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mods {
    shift: bool,
    /// Cmd on macOS, Ctrl elsewhere.
    command: bool,
    /// The Ctrl key on macOS; elsewhere that is `command`.
    control: bool,
    alt: bool,
}

impl Mods {
    const PLAIN: Self = Self { shift: false, command: false, control: false, alt: false };
    const SHIFT: Self = Self { shift: true, ..Self::PLAIN };
    const COMMAND: Self = Self { command: true, ..Self::PLAIN };
    const SHIFT_COMMAND: Self = Self { shift: true, command: true, ..Self::PLAIN };
    const CONTROL: Self = Self { control: true, ..Self::PLAIN };

    fn of(modifiers: Modifiers) -> Self {
        Self {
            shift: modifiers.shift,
            command: modifiers.command,
            control: modifiers.ctrl && !modifiers.command,
            alt: modifiers.alt,
        }
    }

    /// Set modifiers with the names given for shift, command, control, alt.
    fn names(self, names: [&'static str; 4]) -> Vec<&'static str> {
        let set = [self.shift, self.command, self.control, self.alt];
        names.into_iter().zip(set).filter_map(|(name, on)| on.then_some(name)).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Combo {
    key: Key,
    mods: Mods,
}

impl Combo {
    /// As shown in Preferences.
    fn label(self) -> String {
        let (command, alt) =
            if cfg!(target_os = "macos") { ("Cmd", "Option") } else { ("Ctrl", "Alt") };
        let mut parts = self.mods.names(["Shift", command, "Ctrl", alt]);
        parts.push(self.key.symbol_or_name());
        parts.join(" + ")
    }

    /// As saved in the config.
    fn stored(self) -> String {
        let mut parts = self.mods.names(["Shift", "Command", "Control", "Alt"]);
        parts.push(self.key.name());
        parts.join("+")
    }

    fn parse(text: &str) -> Option<Self> {
        let (key, names) =
            text.rsplit('+').collect::<Vec<_>>().split_first().map(|(k, n)| (*k, n.to_vec()))?;
        let mut mods = Mods::PLAIN;
        for name in names {
            match name {
                "Shift" => mods.shift = true,
                "Command" => mods.command = true,
                "Control" => mods.control = true,
                "Alt" => mods.alt = true,
                _ => return None,
            }
        }
        Some(Self { key: Key::from_name(key)?, mods })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Deck(DeckAction),
    /// Held: bends the tempo up or down.
    Bend(i32),
    /// Moves the tempo up or down a step; repeats while held.
    Tempo(i32),
    /// Sampler pad: stops the slot.
    SamplerStop(u8),
    /// Repeats while held.
    Zoom(i32),
    ZoomReset,
    Record,
    /// Marks this moment of the recording.
    Marker,
    /// EDIT mode: 0 Normal tool, 1 Select tool, 2 split at the selection.
    Edit(u8),
    /// Jumps by this many eighths of a beat (back when negative).
    Jump(i32),
    /// Halves (false) or doubles (true) the beat jump size.
    JumpSize(bool),
    /// A loop of this many eighths of a beat.
    BeatLoop(i32),
}

impl KeyAction {
    fn repeats(self) -> bool {
        matches!(self, Self::Tempo(_) | Self::Zoom(_))
    }
}

#[derive(Debug, Clone)]
struct Binding {
    /// None when taken off its key.
    combo: Option<Combo>,
    /// Left or right deck; none for what is not a deck's.
    side: Option<usize>,
    action: KeyAction,
    label: String,
    /// The heading it is listed under.
    section: &'static str,
}

impl Binding {
    /// Names it in the config.
    fn id(&self) -> String {
        match self.side {
            Some(0) => format!("left {}", self.label),
            Some(_) => format!("right {}", self.label),
            None => self.label.clone(),
        }
    }

    /// Names it to the user.
    fn title(&self) -> String {
        match self.side {
            Some(0) => format!("{} (left)", self.label),
            Some(_) => format!("{} (right)", self.label),
            None => self.label.clone(),
        }
    }
}

/// The keys of one hand.
struct Hand {
    play: Key,
    cue: Key,
    sync: Key,
    master_tempo: Key,
    bend_down: Key,
    bend_up: Key,
    jump_back: Key,
    jump_forward: Key,
    memory_prev: Key,
    memory_next: Key,
    loop_in: Key,
    loop_out: Key,
    reloop: Key,
    load: Key,
    vinyl: (Mods, Key),
    hot_cues: [Key; 4],
    beat_loops: [Key; 5],
    sampler: [Key; 4],
}

const LEFT: Hand = Hand {
    play: Key::Z,
    cue: Key::A,
    sync: Key::Q,
    master_tempo: Key::Num5,
    bend_down: Key::W,
    bend_up: Key::E,
    jump_back: Key::R,
    jump_forward: Key::T,
    memory_prev: Key::R,
    memory_next: Key::T,
    loop_in: Key::D,
    loop_out: Key::F,
    reloop: Key::G,
    load: Key::ArrowLeft,
    vinyl: (Mods::COMMAND, Key::V),
    hot_cues: [Key::Num1, Key::Num2, Key::Num3, Key::Num4],
    beat_loops: [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5],
    sampler: [Key::X, Key::C, Key::V, Key::B],
};

const RIGHT: Hand = Hand {
    play: Key::N,
    cue: Key::H,
    sync: Key::Y,
    master_tempo: Key::Num0,
    bend_down: Key::U,
    bend_up: Key::I,
    jump_back: Key::O,
    jump_forward: Key::P,
    memory_prev: Key::O,
    memory_next: Key::P,
    loop_in: Key::K,
    loop_out: Key::L,
    reloop: Key::Semicolon,
    load: Key::ArrowRight,
    vinyl: (Mods::CONTROL, Key::V),
    hot_cues: [Key::Num6, Key::Num7, Key::Num8, Key::Num9],
    beat_loops: [Key::Num6, Key::Num7, Key::Num8, Key::Num9, Key::Num0],
    sampler: [Key::M, Key::Comma, Key::Period, Key::Slash],
};

/// The beat jump lengths with keys of their own (none by default), in
/// eighths of a beat: 1/8 … 64, as far as the jump size goes.
const JUMP_EIGHTHS: [i32; 10] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512];
/// The beat loops with keys, in eighths of a beat: ¼ … 32.
const LOOP_EIGHTHS: [i32; 8] = [2, 4, 8, 16, 32, 64, 128, 256];

/// Beats from eighths of a beat, as shown ("1/8", "½", "4").
fn eighths_label(eighths: i32) -> String {
    match eighths {
        4 => "½".to_owned(),
        _ => crate::effects::beat_label(f64::from(eighths) / 8.0),
    }
}

fn hand_bindings(side: usize, hand: &Hand, out: &mut Vec<Binding>) {
    use DeckAction as D;
    use KeyAction::{Bend, Deck, SamplerStop, Tempo};
    const PLAIN: Mods = Mods::PLAIN;
    const SHIFT: Mods = Mods::SHIFT;
    const COMMAND: Mods = Mods::COMMAND;
    let key = |mods, key| Some(Combo { key, mods });
    let mut section: &str;
    let mut add = |section: &'static str, combo: Option<Combo>, action, label: String| {
        out.push(Binding { combo, side: Some(side), action, label, section });
    };
    // Playback.
    for (combo, action, label) in [
        (key(PLAIN, hand.play), Deck(D::PlayPause), "Play/Pause"),
        (key(PLAIN, hand.cue), Deck(D::Cue), "Cue"),
        (key(SHIFT, hand.cue), Deck(D::JumpToStart), "Back to the start"),
        (key(PLAIN, hand.sync), Deck(D::Sync), "Beat Sync"),
        (key(SHIFT, hand.sync), Deck(D::SyncMaster), "Beat Sync Master"),
        (key(SHIFT, hand.master_tempo), Deck(D::KeyLock), "Master Tempo"),
        (key(PLAIN, hand.bend_down), Bend(-1), "Pitch bend −"),
        (key(PLAIN, hand.bend_up), Bend(1), "Pitch bend +"),
        (key(SHIFT, hand.bend_down), Tempo(-1), "Tempo − 0.1%"),
        (key(SHIFT, hand.bend_up), Tempo(1), "Tempo + 0.1%"),
        (None, Deck(D::TempoRangeCycle), "Tempo range"),
        (key(SHIFT, hand.load), Deck(D::Load), "Load selected track"),
        (key(hand.vinyl.0, hand.vinyl.1), Deck(D::Vinyl), "Vinyl mode"),
        (None, Deck(D::Slip), "Slip"),
        (None, Deck(D::Quantize), "Quantize"),
        (None, Deck(D::TapBpm), "Tap BPM"),
    ] {
        add("Playback", combo, action, label.to_owned());
    }
    // Hot cues A–H: the first four on keys.
    section = "Hot cues";
    for n in 0..8u8 {
        let name = char::from(b'A' + n);
        let hand_key = hand.hot_cues.get(usize::from(n)).copied();
        add(section, hand_key.and_then(|k| key(PLAIN, k)), Deck(D::HotCue(n)), format!("Hot cue {name}"));
    }
    for n in 0..8u8 {
        let name = char::from(b'A' + n);
        let hand_key = hand.hot_cues.get(usize::from(n)).copied();
        let combo = hand_key.and_then(|k| key(Mods::SHIFT_COMMAND, k));
        add(section, combo, Deck(D::HotCueDelete(n)), format!("Clear hot cue {name}"));
    }
    for n in 0..8u8 {
        let name = char::from(b'A' + n);
        add(section, None, Deck(D::HotCueGate(n)), format!("Hot cue {name} (held)"));
    }
    add(section, None, Deck(D::HotCueGateMode), "Hot cue gate on/off".to_owned());
    // Memory cues.
    for (combo, action, label) in [
        (key(PLAIN, hand.memory_prev), Deck(D::MemoryPrev), "Previous memory cue"),
        (key(PLAIN, hand.memory_next), Deck(D::MemoryNext), "Next memory cue"),
        (key(SHIFT, hand.memory_next), Deck(D::MemorySet), "Set memory cue"),
        (key(SHIFT, hand.memory_prev), Deck(D::MemoryDelete), "Delete memory cue"),
    ] {
        add("Memory cues", combo, action, label.to_owned());
    }
    // Loops: each beat loop length, ½ … 8 on keys.
    section = "Loops";
    for (combo, action, label) in [
        (key(PLAIN, hand.loop_in), Deck(D::LoopIn), "Loop In"),
        (key(PLAIN, hand.loop_out), Deck(D::LoopOut), "Loop Out"),
        (key(PLAIN, hand.reloop), Deck(D::ReloopExit), "Reloop/Exit"),
        (key(SHIFT, hand.loop_in), Deck(D::LoopResizeHalf), "Loop ½"),
        (key(SHIFT, hand.reloop), Deck(D::LoopResizeDouble), "Loop ×2"),
        (key(SHIFT, hand.loop_out), Deck(D::AutoLoop), "4-beat loop on/off"),
        (None, Deck(D::ActiveLoop), "Active loop on/off"),
        (None, Deck(D::LoopMoveBack), "Move loop back"),
        (None, Deck(D::LoopMoveForward), "Move loop forward"),
    ] {
        add(section, combo, action, label.to_owned());
    }
    for (i, eighths) in LOOP_EIGHTHS.into_iter().enumerate() {
        // ½ … 8 (the 2nd to 6th) are on the beat-loop keys.
        let combo = i.checked_sub(1).and_then(|k| hand.beat_loops.get(k)).and_then(|&k| key(COMMAND, k));
        add(section, combo, KeyAction::BeatLoop(eighths), format!("{} beat loop", eighths_label(eighths)));
    }
    // Beat jump: by the jump size, the size itself, and every length.
    section = "Beat jump";
    for (combo, action, label) in [
        (key(COMMAND, hand.jump_back), Deck(D::BeatJumpBack), "Beat jump back"),
        (key(COMMAND, hand.jump_forward), Deck(D::BeatJumpForward), "Beat jump forward"),
        (None, KeyAction::JumpSize(false), "Jump size ½"),
        (None, KeyAction::JumpSize(true), "Jump size ×2"),
    ] {
        add(section, combo, action, label.to_owned());
    }
    for eighths in JUMP_EIGHTHS {
        let beats = eighths_label(eighths);
        let unit = if eighths == 8 { "beat" } else { "beats" };
        add(section, None, KeyAction::Jump(-eighths), format!("Jump back {beats} {unit}"));
        add(section, None, KeyAction::Jump(eighths), format!("Jump forward {beats} {unit}"));
    }
    // Stems.
    section = "Stems";
    for (part, name) in deckhand_control::STEM_PARTS.iter().enumerate() {
        add(section, None, Deck(D::StemPart(part as u8)), format!("{name} on/off"));
    }
    add(section, None, Deck(D::PartIsolator), "Part Isolator".to_owned());
    // Pad FX, held.
    section = "Pad FX";
    for n in 0..16u8 {
        let name = char::from(b'A' + n);
        add(section, None, Deck(D::PadFx(n)), format!("Pad FX {name} (held)"));
    }
    // Sampler: pads 1–4 on keys.
    section = "Sampler";
    for n in 0..8u8 {
        let combo = hand.sampler.get(usize::from(n)).and_then(|&k| key(PLAIN, k));
        add(section, combo, Deck(D::SamplerPlay(n)), format!("Sampler pad {}", n + 1));
    }
    for n in 0..8u8 {
        let combo = hand.sampler.get(usize::from(n)).and_then(|&k| key(SHIFT, k));
        add(section, combo, SamplerStop(n), format!("Stop sampler pad {}", n + 1));
    }
    // Key.
    for (action, label) in [
        (D::KeySync, "Key Sync"),
        (D::KeyUp, "Key +1"),
        (D::KeyDown, "Key −1"),
        (D::KeyReset, "Key reset"),
    ] {
        add("Key", None, Deck(action), label.to_owned());
    }
    // The deck's channel.
    for (action, label) in [
        (D::HeadphoneCue, "Headphone cue"),
        (D::CrossfaderAssignA, "Crossfader A"),
        (D::CrossfaderAssignThru, "Crossfader THRU"),
        (D::CrossfaderAssignB, "Crossfader B"),
    ] {
        add("Channel", None, Deck(action), label.to_owned());
    }
}

fn bindings() -> Vec<Binding> {
    let mut all = Vec::new();
    hand_bindings(0, &LEFT, &mut all);
    hand_bindings(1, &RIGHT, &mut all);
    let global = [
        (Mods::PLAIN, Key::Equals, KeyAction::Zoom(1), "Zoom in"),
        (Mods::PLAIN, Key::Minus, KeyAction::Zoom(-1), "Zoom out"),
        (Mods::SHIFT, Key::Equals, KeyAction::ZoomReset, "Reset zoom"),
        (Mods::COMMAND, Key::Enter, KeyAction::Record, "Start/stop recording"),
        (Mods::SHIFT, Key::Enter, KeyAction::Marker, "Add recording marker"),
    ];
    for (mods, key, action, label) in global {
        let combo = Some(Combo { key, mods });
        let section = "View and recording";
        all.push(Binding { combo, side: None, action, label: label.to_owned(), section });
    }
    all
}

/// EDIT mode's keys: apart from the mixing keys (they never both apply),
/// so the same keys can be used.
const EDIT: &str = "EDIT: ";

fn edit_bindings() -> Vec<Binding> {
    // Cmd + E on a Mac, Ctrl + E elsewhere.
    let ctrl = Mods::COMMAND;
    [
        (Mods::PLAIN, Key::Num1, 0, "Normal tool"),
        (Mods::PLAIN, Key::Num2, 1, "Select tool"),
        (ctrl, Key::E, 2, "Split clips at the selection"),
    ]
    .into_iter()
    .map(|(mods, key, edit, label)| Binding {
        combo: Some(Combo { key, mods }),
        side: None,
        action: KeyAction::Edit(edit),
        label: format!("{EDIT}{label}"),
        section: "EDIT mode",
    })
    .collect()
}

/// The defaults of the list `id` is in.
fn defaults_for(id: &str) -> Vec<Binding> {
    if id.starts_with(EDIT) { edit_bindings() } else { bindings() }
}

/// The defaults with the user's changes (an empty one takes the key away).
fn effective(changes: &BTreeMap<String, String>) -> Vec<Binding> {
    with_changes(bindings(), changes)
}

fn effective_edit(changes: &BTreeMap<String, String>) -> Vec<Binding> {
    with_changes(edit_bindings(), changes)
}

fn with_changes(mut all: Vec<Binding>, changes: &BTreeMap<String, String>) -> Vec<Binding> {
    for binding in &mut all {
        match changes.get(&binding.id()).map(String::as_str) {
            Some("") => binding.combo = None,
            Some(text) => binding.combo = Combo::parse(text).or(binding.combo),
            None => {}
        }
    }
    all
}

/// Puts binding `id` on `combo`. Whatever had that key loses it, so a key
/// does one thing; returns its name. Changes back to a default are dropped.
fn assign(
    changes: &mut BTreeMap<String, String>,
    id: &str,
    combo: Option<Combo>,
) -> Option<String> {
    let taken = with_changes(defaults_for(id), changes)
        .into_iter()
        .find(|b| combo.is_some() && b.combo == combo && b.id() != id);
    if let Some(taken) = &taken {
        changes.insert(taken.id(), String::new());
    }
    changes.insert(id.to_owned(), combo.map(Combo::stored).unwrap_or_default());
    for default in bindings().into_iter().chain(edit_bindings()) {
        let text = default.combo.map(Combo::stored).unwrap_or_default();
        if changes.get(&default.id()) == Some(&text) {
            changes.remove(&default.id());
        }
    }
    taken.map(|b| b.title())
}

/// Rows of the key list: a function, on the left deck or for no deck, with
/// its right deck twin.
fn rows(all: &[Binding]) -> Vec<(&Binding, Option<&Binding>)> {
    all.iter()
        .filter(|b| b.side != Some(1))
        .map(|b| (b, all.iter().find(|r| r.side == Some(1) && r.label == b.label)))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Press(usize, KeyAction),
    Release(usize, KeyAction),
    /// A binding got a new key, or lost it.
    Assign(String, Option<Combo>),
}

/// Which keys are down, so each press gets its release.
pub(super) struct KeyboardMixer {
    bindings: Vec<Binding>,
    /// EDIT mode's keys.
    edit: Vec<Binding>,
    held: HashMap<Key, (usize, KeyAction)>,
    /// The binding waiting for its new key.
    capturing: Option<String>,
}

impl KeyboardMixer {
    pub(super) fn new(changes: &BTreeMap<String, String>) -> Self {
        Self {
            bindings: effective(changes),
            edit: effective_edit(changes),
            held: HashMap::new(),
            capturing: None,
        }
    }

    /// The EDIT mode tool whose key was pressed (not while typing).
    pub(super) fn edit_tool(&self, ctx: &egui::Context) -> Option<u8> {
        if ctx.wants_keyboard_input() || self.capturing.is_some() {
            return None;
        }
        ctx.input(|i| {
            i.events.iter().find_map(|event| match *event {
                Event::Key { key, physical_key, pressed: true, repeat: false, modifiers } => {
                    let combo =
                        Combo { key: physical_key.unwrap_or(key), mods: Mods::of(modifiers) };
                    self.edit.iter().find(|b| b.combo == Some(combo)).and_then(|b| match b.action {
                        KeyAction::Edit(edit) => Some(edit),
                        _ => None,
                    })
                }
                _ => None,
            })
        })
    }

    /// The EDIT mode tools' keys, as shown ("" for none).
    pub(super) fn tool_labels(&self) -> [String; 2] {
        std::array::from_fn(|i| {
            self.edit.get(i).and_then(|b| b.combo).map(Combo::label).unwrap_or_default()
        })
    }

    /// The Keyboard page of Preferences; returns whether anything changed
    /// there. New keys arrive through `read`.
    pub(super) fn prefs_ui(&mut self, ui: &mut egui::Ui, prefs: &mut Preferences) -> bool {
        let mut changed = ui
            .horizontal(|ui| {
                let changed = ui.checkbox(&mut prefs.keyboard_mixing, "Mix with the keyboard").changed();
                crate::widgets::help(ui, "Click, then press a key. Esc cancels, Delete clears.");
                changed
            })
            .inner;
        let reset = egui::Button::new("Reset to rekordbox's Performance 1");
        if ui.add_enabled(!prefs.key_bindings.is_empty(), reset).clicked() {
            prefs.key_bindings.clear();
            self.bindings = bindings();
            self.edit = edit_bindings();
            self.capturing = None;
            changed = true;
        }
        ui.add_space(6.0);
        ui.scope(|ui| {
            egui::Grid::new("keyboard keys")
                .num_columns(3)
                .striped(true)
                .spacing([24.0, 3.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Function").strong());
                    ui.label(RichText::new("Left deck").strong());
                    ui.label(RichText::new("Right deck").strong());
                    ui.end_row();
                    let mut section = "";
                    for (binding, right) in rows(&self.bindings) {
                        if binding.section != section {
                            section = binding.section;
                            ui.end_row();
                            ui.label(RichText::new(section).strong().size(14.0));
                            ui.end_row();
                        }
                        ui.label(&binding.label);
                        key_button(ui, binding, &mut self.capturing);
                        if let Some(right) = right {
                            key_button(ui, right, &mut self.capturing);
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            ui.label(RichText::new("EDIT mode").strong());
            egui::Grid::new("edit keys").num_columns(2).striped(true).spacing([24.0, 3.0]).show(
                ui,
                |ui| {
                    for binding in &self.edit {
                        ui.label(binding.label.trim_start_matches(EDIT));
                        key_button(ui, binding, &mut self.capturing);
                        ui.end_row();
                    }
                },
            );
        });
        changed
    }

    /// Takes the mixing keys out of `events` (the rest stay for the UI).
    /// `live` is off for new presses while typing or disabled; releases
    /// always count, and leaving the window lets go of everything, as its
    /// key releases go elsewhere. `decks` are the left and right deck.
    fn read(&mut self, events: &mut Vec<Event>, live: bool, decks: [usize; 2]) -> Vec<Step> {
        let mut steps = Vec::new();
        events.retain(|event| match *event {
            Event::WindowFocused(false) => {
                self.capturing = None;
                steps.extend(self.held.drain().map(|(_, (d, a))| Step::Release(d, a)));
                true
            }
            Event::Key { key, physical_key, pressed, repeat, modifiers } => {
                let key = physical_key.unwrap_or(key);
                if !pressed {
                    let Some((d, action)) = self.held.remove(&key) else { return true };
                    steps.push(Step::Release(d, action));
                    return false;
                }
                let combo = Combo { key, mods: Mods::of(modifiers) };
                if self.capturing.is_some() {
                    if !repeat && let Some(id) = self.capturing.take() {
                        match key {
                            Key::Escape => {}
                            Key::Delete | Key::Backspace => steps.push(Step::Assign(id, None)),
                            _ => steps.push(Step::Assign(id, Some(combo))),
                        }
                    }
                    return false;
                }
                let binding = self.bindings.iter().find(|b| live && b.combo == Some(combo));
                let Some(binding) = binding else { return true };
                let deck = binding.side.map_or(0, |side| decks[side]);
                if repeat {
                    if binding.action.repeats() {
                        steps.push(Step::Press(deck, binding.action));
                    }
                    return false;
                }
                // macOS sends no release for keys let go while Cmd is down.
                if let Some((d, action)) = self.held.insert(key, (deck, binding.action)) {
                    steps.push(Step::Release(d, action));
                }
                steps.push(Step::Press(deck, binding.action));
                false
            }
            _ => true,
        });
        steps
    }
}

/// A key of the list: click to give it a new one.
fn key_button(ui: &mut egui::Ui, binding: &Binding, capturing: &mut Option<String>) {
    let id = binding.id();
    let waiting = capturing.as_ref() == Some(&id);
    let text = match binding.combo {
        _ if waiting => "Press a key…".to_owned(),
        Some(combo) => combo.label(),
        None => "—".to_owned(),
    };
    if ui.selectable_label(waiting, text).clicked() {
        *capturing = (!waiting).then_some(id);
    }
}

impl DeckhandApp {
    /// Mixing keys, before the library's so that Shift + arrows load tracks.
    pub(super) fn mixing_keys(&mut self, ctx: &egui::Context, performing: bool) {
        if !self.show_prefs || self.prefs_tab != PrefsTab::Keyboard {
            self.keyboard.capturing = None;
        }
        let live = performing && self.config.prefs.keyboard_mixing && !ctx.wants_keyboard_input();
        // The active deck on each side (the one shown, with two decks).
        let decks = self.active_decks;
        let steps = ctx.input_mut(|i| self.keyboard.read(&mut i.events, live, decks));
        for step in steps {
            match step {
                Step::Press(d, action) => self.key_pressed(d, action, ctx),
                Step::Release(d, KeyAction::Deck(action)) => {
                    self.apply_deck_event(d, action, Value::Pressed(false), ctx);
                }
                Step::Release(..) => {}
                Step::Assign(id, combo) => {
                    let changes = &mut self.config.prefs.key_bindings;
                    if let Some(taken) = assign(changes, &id, combo) {
                        self.status = Some(format!("{taken} no longer has a key"));
                    }
                    self.keyboard.bindings = effective(changes);
                    self.keyboard.edit = effective_edit(changes);
                    self.config.save();
                }
            }
        }
        let Some(engine) = self.engine.as_mut() else { return };
        for &(d, action) in self.keyboard.held.values() {
            if let KeyAction::Bend(direction) = action {
                self.decks[d].jog(direction * BEND_STEPS, engine);
                ctx.request_repaint();
            }
        }
    }

    fn key_pressed(&mut self, d: usize, action: KeyAction, ctx: &egui::Context) {
        match action {
            KeyAction::Deck(action) => {
                self.apply_deck_event(d, action, Value::Pressed(true), ctx);
            }
            KeyAction::Bend(_) => {}
            KeyAction::Tempo(direction) => {
                let Some(engine) = self.engine.as_mut() else { return };
                let deck = &mut self.decks[d];
                let step = TEMPO_STEP / deck.tempo_range.fraction();
                deck.set_tempo_fader(deck.tempo_fader + direction as f32 * step as f32, engine);
            }
            KeyAction::SamplerStop(pad) => {
                let slot = self.effects.sampler_slot(d, pad);
                if let Some(engine) = self.engine.as_mut()
                    && engine.sampler_playing(slot)
                {
                    engine.send(Command::SamplerStop { slot });
                }
            }
            KeyAction::Zoom(steps) => {
                self.apply_global_event(GlobalAction::Zoom, Value::Relative(steps));
            }
            KeyAction::ZoomReset => self.seconds_visible = DEFAULT_SECONDS_VISIBLE,
            KeyAction::Record => self.toggle_recording(),
            KeyAction::Marker => self.add_marker(),
            // EDIT mode reads its own keys.
            KeyAction::Edit(_) => {}
            KeyAction::Jump(eighths) => {
                let Some(engine) = self.engine.as_mut() else { return };
                self.decks[d].beat_jump(f64::from(eighths) / 8.0, engine);
            }
            KeyAction::JumpSize(up) => self.decks[d].change_jump_size(if up { 2.0 } else { 0.5 }),
            KeyAction::BeatLoop(eighths) => {
                let Some(engine) = self.engine.as_mut() else { return };
                self.decks[d].beat_loop(f64::from(eighths) / 8.0, engine);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, physical: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: Some(physical), pressed, repeat: false, modifiers }
    }

    fn press(k: Key) -> Event {
        key(k, k, true, Modifiers::NONE)
    }

    fn release(k: Key) -> Event {
        key(k, k, false, Modifiers::NONE)
    }

    fn read(mixer: &mut KeyboardMixer, events: Vec<Event>) -> (Vec<Step>, Vec<Event>) {
        let mut events = events;
        let steps = mixer.read(&mut events, true, [0, 1]);
        (steps, events)
    }

    const CUE: KeyAction = KeyAction::Deck(DeckAction::Cue);

    /// EDIT mode's keys are their own: giving one a mixing key takes
    /// nothing away from mixing.
    #[test]
    fn edit_keys_are_apart_from_mixing_keys() {
        let mut changes = BTreeMap::new();
        let hot_cue = Combo { key: Key::Num1, mods: Mods::PLAIN };
        assert!(effective(&changes).iter().any(|b| b.combo == Some(hot_cue)));
        let taken = assign(&mut changes, "EDIT: Select tool", Some(hot_cue));
        // Only the Normal tool (also on 1) lost it.
        assert_eq!(taken.as_deref(), Some("EDIT: Normal tool"));
        assert!(effective(&changes).iter().any(|b| b.combo == Some(hot_cue)));
        assert_eq!(effective_edit(&changes)[1].combo, Some(hot_cue));
    }

    #[test]
    fn no_two_bindings_share_a_key() {
        let all = bindings();
        for (i, a) in all.iter().enumerate() {
            let same = all[..i].iter().find(|b| a.combo.is_some() && b.combo == a.combo);
            assert!(same.is_none(), "{} and {:?}", a.label, same.map(|b| &b.label));
        }
    }

    /// Every beat jump length and the other pad functions are listed, off
    /// their keys at first; the old keys keep their names in the config.
    #[test]
    fn everything_is_listed() {
        let all = bindings();
        let left = |label: &str| all.iter().find(|b| b.side == Some(0) && b.label == label);
        for label in ["Jump back 1/8 beats", "Jump forward 1 beat", "Jump forward 64 beats", "Jump size ×2", "VOCAL on/off", "Pad FX P (held)", "Hot cue H", "32 beat loop"] {
            assert_eq!(left(label).map(|b| b.combo), Some(None), "{label}");
        }
        for label in ["½ beat loop", "8 beat loop", "Hot cue D", "Beat jump back"] {
            assert!(left(label).is_some_and(|b| b.combo.is_some()), "{label}");
        }
    }

    #[test]
    fn hands_mirror_each_other() {
        let all = bindings();
        let left = all.iter().filter(|b| b.side == Some(0)).count();
        assert_eq!(left, rows(&all).iter().filter(|(_, right)| right.is_some()).count());
    }

    #[test]
    fn a_held_key_is_released_once() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        let (steps, rest) = read(&mut mixer, vec![press(Key::A)]);
        assert_eq!(steps, [Step::Press(0, CUE)]);
        assert!(rest.is_empty());
        let (steps, _) = read(&mut mixer, vec![release(Key::A), release(Key::A)]);
        assert_eq!(steps, [Step::Release(0, CUE)]);
    }

    #[test]
    fn right_hand_goes_to_the_right_deck() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        let mut events = vec![press(Key::H)];
        assert_eq!(mixer.read(&mut events, true, [2, 3]), [Step::Press(3, CUE)]);
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        let shift = key(Key::A, Key::A, true, Modifiers::SHIFT);
        let (steps, _) = read(&mut mixer, vec![shift]);
        assert_eq!(steps, [Step::Press(0, KeyAction::Deck(DeckAction::JumpToStart))]);
        // Cmd+A is the library's select all.
        let (steps, rest) = read(&mut mixer, vec![key(Key::A, Key::A, true, Modifiers::COMMAND)]);
        assert!(steps.is_empty());
        assert_eq!(rest.len(), 1);
    }

    #[test]
    fn keys_go_by_position() {
        // Y on a QWERTZ keyboard sits where Z does on QWERTY.
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        let (steps, _) = read(&mut mixer, vec![key(Key::Y, Key::Z, true, Modifiers::NONE)]);
        assert_eq!(steps, [Step::Press(0, KeyAction::Deck(DeckAction::PlayPause))]);
    }

    #[test]
    fn repeats_only_count_for_stepping_keys() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        let repeat = |k| Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: true,
            modifiers: Modifiers::NONE,
        };
        let (steps, rest) = read(&mut mixer, vec![press(Key::Z), repeat(Key::Z)]);
        assert_eq!(steps.len(), 1);
        assert!(rest.is_empty());
        let (steps, _) = read(&mut mixer, vec![press(Key::Equals), repeat(Key::Equals)]);
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn typing_keeps_its_keys_but_releases_still_land() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        read(&mut mixer, vec![press(Key::A)]);
        let mut events = vec![press(Key::Z), release(Key::A)];
        let steps = mixer.read(&mut events, false, [0, 1]);
        assert_eq!(steps, [Step::Release(0, CUE)]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn leaving_the_window_lets_go() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        read(&mut mixer, vec![press(Key::A)]);
        let (steps, _) = read(&mut mixer, vec![Event::WindowFocused(false)]);
        assert_eq!(steps, [Step::Release(0, CUE)]);
        assert!(mixer.held.is_empty());
    }

    #[test]
    fn combos_survive_the_config() {
        for binding in bindings() {
            let Some(combo) = binding.combo else { continue };
            assert_eq!(Combo::parse(&combo.stored()), Some(combo), "{}", binding.label);
        }
        let all = Combo { key: Key::K, mods: Mods { alt: true, control: true, ..Mods::SHIFT } };
        assert_eq!(Combo::parse(&all.stored()), Some(all));
        assert_eq!(Combo::parse("Nonsense+K"), None);
        assert_eq!(Combo::parse("NoSuchKey"), None);
    }

    fn id_of(label: &str) -> String {
        bindings().iter().find(|b| b.label == label).unwrap().id()
    }

    #[test]
    fn capture_sets_the_next_key_without_playing_it() {
        let mut changes = BTreeMap::new();
        let mut mixer = KeyboardMixer::new(&changes);
        mixer.capturing = Some(id_of("Cue"));
        // Z is Play/Pause, and it must not start the deck.
        let shift_z = key(Key::Z, Key::Z, true, Modifiers::SHIFT);
        let (steps, rest) = read(&mut mixer, vec![shift_z]);
        assert!(rest.is_empty());
        let [Step::Assign(id, combo)] = &steps[..] else { panic!("{steps:?}") };
        assert_eq!(assign(&mut changes, id, *combo), None);
        mixer.bindings = effective(&changes);
        let (steps, _) = read(&mut mixer, vec![press(Key::A)]);
        assert!(steps.is_empty());
        let shift_z = key(Key::Z, Key::Z, true, Modifiers::SHIFT);
        assert_eq!(read(&mut mixer, vec![shift_z]).0, [Step::Press(0, CUE)]);
    }

    #[test]
    fn capture_cancels_and_clears() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        mixer.capturing = Some(id_of("Cue"));
        assert!(read(&mut mixer, vec![press(Key::Escape)]).0.is_empty());
        assert_eq!(mixer.capturing, None);
        mixer.capturing = Some(id_of("Cue"));
        let (steps, _) = read(&mut mixer, vec![press(Key::Delete)]);
        assert_eq!(steps, [Step::Assign(id_of("Cue"), None)]);
    }

    #[test]
    fn a_taken_key_moves_and_defaults_are_not_kept() {
        let mut changes = BTreeMap::new();
        let z = Combo { key: Key::Z, mods: Mods::PLAIN };
        let a = Combo { key: Key::A, mods: Mods::PLAIN };
        assert_eq!(
            assign(&mut changes, &id_of("Cue"), Some(z)).as_deref(),
            Some("Play/Pause (left)")
        );
        let all = effective(&changes);
        let of = |label| all.iter().find(|b| b.label == label && b.side == Some(0)).unwrap();
        assert_eq!(of("Play/Pause").combo, None);
        assert_eq!(of("Cue").combo, Some(z));
        // Back where they started, nothing is left to save.
        assign(&mut changes, &id_of("Cue"), Some(a));
        assign(&mut changes, &id_of("Play/Pause"), Some(z));
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn a_bad_saved_key_falls_back_to_the_default() {
        let changes = BTreeMap::from([(id_of("Cue"), "Nonsense".to_owned())]);
        let all = effective(&changes);
        assert_eq!(all[1].combo, Some(Combo { key: Key::A, mods: Mods::PLAIN }));
    }

    #[test]
    fn a_lost_release_is_made_up_for() {
        let mut mixer = KeyboardMixer::new(&BTreeMap::new());
        read(&mut mixer, vec![press(Key::A)]);
        let (steps, _) = read(&mut mixer, vec![press(Key::A)]);
        assert_eq!(steps, [Step::Release(0, CUE), Step::Press(0, CUE)]);
    }
}
