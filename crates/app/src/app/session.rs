//! Remembering deck, mixer and FX settings between runs.

use std::time::{Duration, Instant};

use deckhand_audio::{BeatFxKind, ColorFxKind, CrossfaderSide, FxTarget};

use super::DeckhandApp;
use crate::config::{DeckSession, FxSession, Session, Side};

/// How often changed settings are written while running.
const SAVE_EVERY: Duration = Duration::from_secs(3);

impl DeckhandApp {
    /// The current settings, as saved.
    fn capture_session(&self) -> Session {
        let decks = self
            .decks
            .iter()
            .map(|d| DeckSession {
                side: Some(match d.side {
                    CrossfaderSide::A => Side::A,
                    CrossfaderSide::Thru => Side::Thru,
                    CrossfaderSide::B => Side::B,
                }),
                tempo_range: Some(d.tempo_range),
                quantize: d.quantize,
                vinyl: d.vinyl,
                key_lock: d.key_lock,
                slip: d.slip,
                hot_cue_gate: d.hot_cue_gate,
                pad_mode: d.pad_mode,
                jump_size: d.jump_size,
                jump_range: d.jump_range,
                loop_scale: d.loop_scale,
                volume: d.volume,
                trim: d.trim,
                eq: d.eq,
                filter: d.filter,
                headphone_cue: d.headphone_cue,
            })
            .collect();
        let fx = &self.effects;
        Session {
            decks,
            crossfader: self.crossfader,
            master_volume: Some(self.master_volume),
            headphone_mix: self.headphone_mix,
            headphone_volume: Some(self.headphone_volume),
            zoom: Some(self.seconds_visible),
            fx: FxSession {
                kind: BeatFxKind::ALL.iter().position(|k| *k == fx.kind),
                beat: Some(fx.beat),
                depth: Some(fx.depth),
                target: Some(match fx.target {
                    FxTarget::Channel(d) => format!("ch{}", d + 1),
                    FxTarget::Master => "master".into(),
                    FxTarget::Sampler => "sampler".into(),
                }),
                color: ColorFxKind::ALL.iter().position(|k| *k == fx.color),
                color_on: Some(fx.color_on),
            },
        }
    }

    /// Puts saved settings back; call before the audio output starts, which
    /// sends them to the engine.
    pub(super) fn restore_session(&mut self) {
        let session = self.config.session.clone();
        for (deck, saved) in self.decks.iter_mut().zip(&session.decks) {
            if let Some(side) = saved.side {
                deck.side = match side {
                    Side::A => CrossfaderSide::A,
                    Side::Thru => CrossfaderSide::Thru,
                    Side::B => CrossfaderSide::B,
                };
            }
            if let Some(range) = saved.tempo_range {
                deck.tempo_range = range;
            }
            deck.quantize = saved.quantize;
            deck.vinyl = saved.vinyl;
            deck.key_lock = saved.key_lock;
            deck.slip = saved.slip;
            deck.hot_cue_gate = saved.hot_cue_gate;
            deck.set_pad_mode(saved.pad_mode);
            deck.jump_size = saved.jump_size.clamp(1.0 / 8.0, 64.0);
            deck.jump_range = saved.jump_range.clamp(1.0 / 32.0, 64.0);
            deck.loop_scale = saved.loop_scale.clamp(1.0 / 16.0, 16.0);
            deck.volume = saved.volume.clamp(0.0, 1.0);
            deck.trim = saved.trim.clamp(0.0, 1.0);
            deck.eq = saved.eq.map(|v| v.clamp(0.0, 1.0));
            deck.filter = saved.filter.clamp(-1.0, 1.0);
            deck.headphone_cue = saved.headphone_cue;
        }
        self.crossfader = session.crossfader.clamp(-1.0, 1.0);
        if let Some(level) = session.master_volume {
            self.master_volume = level.clamp(0.0, 1.0);
        }
        self.headphone_mix = session.headphone_mix.clamp(0.0, 1.0);
        if let Some(level) = session.headphone_volume {
            self.headphone_volume = level.clamp(0.0, 1.0);
        }
        if let Some(zoom) = session.zoom {
            self.seconds_visible = zoom.clamp(2.0, 30.0);
        }
        let fx = &mut self.effects;
        if let Some(kind) = session.fx.kind.and_then(|i| BeatFxKind::ALL.get(i)) {
            fx.kind = *kind;
        }
        if let Some(beat) = session.fx.beat {
            fx.beat = beat.min(crate::effects::FX_BEATS.len() - 1);
        }
        if let Some(depth) = session.fx.depth {
            fx.depth = depth.clamp(0.0, 1.0);
        }
        fx.target = match session.fx.target.as_deref() {
            Some("master") | None => FxTarget::Master,
            Some("sampler") => FxTarget::Sampler,
            Some(ch) => ch
                .strip_prefix("ch")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| (1..=deckhand_audio::DECKS).contains(n))
                .map_or(FxTarget::Master, |n| FxTarget::Channel(n - 1)),
        };
        if let Some(color) = session.fx.color.and_then(|i| ColorFxKind::ALL.get(i)) {
            fx.color = *color;
        }
        if let Some(on) = session.fx.color_on {
            fx.color_on = on;
        }
    }

    /// Writes the settings when they changed; `now` forces it (on quit).
    pub(super) fn save_session(&mut self, now: bool) {
        if !now && self.session_checked.elapsed() < SAVE_EVERY {
            return;
        }
        self.session_checked = Instant::now();
        let session = self.capture_session();
        if session != self.config.session {
            self.config.session = session;
            self.config.save();
        }
    }
}
