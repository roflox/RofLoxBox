//! Parts of the mixer and the FX strip that can be hidden one by one or a
//! group at a time (View menu). Saved in the config as the hidden ones.

use serde::{Deserialize, Serialize};

/// A piece of the on-screen mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MixerPart {
    Trim,
    High,
    Mid,
    Low,
    Filter,
    ChannelCue,
    Fader,
    ChannelMeter,
    CrossfaderAssign,
    MasterLevel,
    MasterMeter,
    PhonesMix,
    PhonesLevel,
    Crossfader,
    ChannelOrder,
}

impl MixerPart {
    /// Groups for the View menu: (name, parts).
    pub const GROUPS: [(&'static str, &'static [MixerPart]); 4] = [
        (
            "Channels",
            &[
                Self::Trim,
                Self::High,
                Self::Mid,
                Self::Low,
                Self::Filter,
                Self::ChannelCue,
                Self::Fader,
                Self::ChannelMeter,
                Self::CrossfaderAssign,
            ],
        ),
        ("Master", &[Self::MasterLevel, Self::MasterMeter]),
        ("Headphones", &[Self::PhonesMix, Self::PhonesLevel]),
        ("Crossfader", &[Self::Crossfader, Self::ChannelOrder]),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Trim => "TRIM",
            Self::High => "HI",
            Self::Mid => "MID",
            Self::Low => "LOW",
            Self::Filter => "FILTER",
            Self::ChannelCue => "CUE (headphones)",
            Self::Fader => "Channel fader",
            Self::ChannelMeter => "Channel level meter",
            Self::CrossfaderAssign => "Crossfader assign A / T / B",
            Self::MasterLevel => "Master level",
            Self::MasterMeter => "Master level meter",
            Self::PhonesMix => "Phones CUE/MASTER mix",
            Self::PhonesLevel => "Phones level",
            Self::Crossfader => "Crossfader",
            Self::ChannelOrder => "Channel order (4 decks)",
        }
    }
}

/// A section of the FX and sampler strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FxPart {
    BeatFx,
    ColorFx,
    MasterCue,
    Sampler,
}

impl FxPart {
    pub const GROUPS: [(&'static str, &'static [FxPart]); 1] =
        [("FX and sampler", &[Self::BeatFx, Self::ColorFx, Self::MasterCue, Self::Sampler])];

    pub fn label(self) -> &'static str {
        match self {
            Self::BeatFx => "Beat FX",
            Self::ColorFx => "Color FX",
            Self::MasterCue => "MASTER CUE",
            Self::Sampler => "Sampler",
        }
    }
}

/// Checkboxes for grouped parts: a group's box switches all of its parts
/// (half-ticked when only some are shown), each part has its own below it.
/// `hidden` lists what is not shown. Returns whether anything changed.
pub fn grouped_checkboxes<T: Copy + PartialEq>(
    ui: &mut eframe::egui::Ui,
    groups: &[(&str, &[T])],
    label: impl Fn(T) -> &'static str,
    hidden: &mut Vec<T>,
) -> bool {
    let mut changed = false;
    for &(name, parts) in groups {
        let shown = parts.iter().filter(|p| !hidden.contains(p)).count();
        let mut all = shown == parts.len();
        let some = shown > 0 && !all;
        let group =
            eframe::egui::Checkbox::new(&mut all, eframe::egui::RichText::new(name).strong())
                .indeterminate(some);
        if ui.add(group).clicked() {
            hidden.retain(|h| !parts.contains(h));
            if !all {
                hidden.extend_from_slice(parts);
            }
            changed = true;
        }
        ui.indent(name, |ui| {
            for &part in parts {
                let mut on = !hidden.contains(&part);
                if ui.checkbox(&mut on, label(part)).changed() {
                    hidden.retain(|h| *h != part);
                    if !on {
                        hidden.push(part);
                    }
                    changed = true;
                }
            }
        });
    }
    changed
}
