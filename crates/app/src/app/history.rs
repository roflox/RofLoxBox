//! History: what was played, in one playlist per day inside the HISTORY
//! folder, like rekordbox's. A collection track goes in once it has played
//! for `PLAYED_SECONDS` with its channel fader up; that also counts the play.

use std::path::PathBuf;
use std::time::Instant;

use super::DeckhandApp;

/// Playing time (with the channel fader up) that makes a track played.
const PLAYED_SECONDS: f64 = 30.0;

/// How long the track on a deck has played since it was loaded.
#[derive(Default)]
pub(super) struct Played {
    /// The loaded file: a new one starts the count again.
    path: Option<PathBuf>,
    seconds: f64,
    logged: bool,
}

impl DeckhandApp {
    pub(super) fn track_history(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.history_tick).as_secs_f64().min(0.5);
        self.history_tick = now;
        let Some(engine) = self.engine.as_ref() else { return };
        let mut done = Vec::new();
        for (deck, played) in self.decks.iter().zip(self.played.iter_mut()) {
            let Some(track) = deck.track.as_ref() else {
                *played = Played::default();
                continue;
            };
            if played.path.as_ref() != Some(&track.path) {
                *played = Played { path: Some(track.path.clone()), ..Played::default() };
            }
            if played.logged || !engine.is_playing(deck.index) || deck.volume <= 0.0 {
                continue;
            }
            played.seconds += dt;
            if played.seconds >= PLAYED_SECONDS {
                played.logged = true;
                // USB tracks aren't in the collection.
                if track.from_collection {
                    done.push(track.id);
                }
            }
        }
        for id in done {
            let name = format!("HISTORY {}", chrono::Local::now().format("%Y-%m-%d"));
            let playlist = self.collection.history_playlist(&name);
            // A track reloaded and played again right away is one play.
            if self.collection.node(playlist).and_then(|n| n.tracks.last()) == Some(&id) {
                continue;
            }
            self.collection.add_to_playlist(playlist, &[id]);
            self.collection.update_track(id, |t| {
                t.play_count += 1;
                t.last_played = Some(super::collection_view::deckhand_now());
            });
        }
    }
}
