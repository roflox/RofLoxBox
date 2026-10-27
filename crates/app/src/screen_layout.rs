//! Where the big pieces of the main screen go: the deck columns, the
//! vertical waveforms and the mixer, as columns of stacked blocks. Saved in
//! the config; rearranged by dragging in layout-editing mode.

use serde::{Deserialize, Serialize};

/// One piece of the main screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScreenBlock {
    /// Deck 1 (and 3 with four decks).
    LeftDecks,
    /// Vertical waveforms of the mixer's left channels.
    LeftWaves,
    Mixer,
    /// Vertical waveforms of the mixer's right channels.
    RightWaves,
    /// Deck 2 (and 4).
    RightDecks,
}

impl ScreenBlock {
    pub const ALL: [ScreenBlock; 5] =
        [Self::LeftDecks, Self::LeftWaves, Self::Mixer, Self::RightWaves, Self::RightDecks];

    pub fn label(self) -> &'static str {
        match self {
            Self::LeftDecks => "Left decks",
            Self::LeftWaves => "Left waveforms",
            Self::Mixer => "Mixer",
            Self::RightWaves => "Right waveforms",
            Self::RightDecks => "Right decks",
        }
    }

    pub fn is_waves(self) -> bool {
        matches!(self, Self::LeftWaves | Self::RightWaves)
    }
}

/// Which side of a block another one is dropped on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// A new column in front of the block's column.
    Left,
    /// A new column after it.
    Right,
    /// Stacked above the block, in its column.
    Above,
    /// Stacked below it.
    Below,
}

/// Columns left to right, each a stack of blocks top to bottom. Waveform
/// blocks next to each other in a column share a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenLayout {
    pub columns: Vec<Vec<ScreenBlock>>,
}

impl Default for ScreenLayout {
    /// Every block in a column of its own, the mixer in the middle.
    fn default() -> Self {
        Self { columns: ScreenBlock::ALL.iter().map(|&b| vec![b]).collect() }
    }
}

impl ScreenLayout {
    /// Ready-made arrangements: (name, layout).
    pub fn built_in() -> [(&'static str, ScreenLayout); 3] {
        use ScreenBlock as B;
        [
            ("Mixer in the middle", Self::default()),
            (
                "Mixer under the waveforms",
                Self {
                    columns: vec![
                        vec![B::LeftDecks],
                        vec![B::LeftWaves, B::RightWaves, B::Mixer],
                        vec![B::RightDecks],
                    ],
                },
            ),
            (
                "Mixer on the right",
                Self {
                    columns: vec![
                        vec![B::LeftDecks],
                        vec![B::LeftWaves],
                        vec![B::RightWaves],
                        vec![B::RightDecks],
                        vec![B::Mixer],
                    ],
                },
            ),
        ]
    }

    /// Puts `block` on an edge of `target`.
    pub fn move_block(&mut self, block: ScreenBlock, target: ScreenBlock, edge: Edge) {
        if block == target {
            return;
        }
        for column in &mut self.columns {
            column.retain(|b| *b != block);
        }
        // Where the target is, after taking the block out.
        let found = self
            .columns
            .iter()
            .enumerate()
            .find_map(|(c, column)| column.iter().position(|b| *b == target).map(|r| (c, r)));
        let Some((c, r)) = found else {
            self.columns.push(vec![block]);
            self.repair();
            return;
        };
        match edge {
            Edge::Left => self.columns.insert(c, vec![block]),
            Edge::Right => self.columns.insert(c + 1, vec![block]),
            Edge::Above => self.columns[c].insert(r, block),
            Edge::Below => self.columns[c].insert(r + 1, block),
        }
        self.repair();
    }

    /// Drops duplicates and empty columns, and adds blocks a saved layout
    /// lacks.
    pub fn repair(&mut self) {
        let mut seen = Vec::new();
        for column in &mut self.columns {
            column.retain(|b| {
                let new = !seen.contains(b);
                seen.push(*b);
                new
            });
        }
        self.columns.retain(|column| !column.is_empty());
        for block in ScreenBlock::ALL {
            if !seen.contains(&block) {
                self.columns.push(vec![block]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ScreenBlock as B;

    #[test]
    fn moving_blocks() {
        let mut layout = ScreenLayout::default();
        layout.move_block(B::Mixer, B::LeftWaves, Edge::Below);
        assert_eq!(layout.columns[1], vec![B::LeftWaves, B::Mixer]);
        assert_eq!(layout.columns.len(), 4, "the mixer's own column is gone");
        layout.move_block(B::RightWaves, B::LeftWaves, Edge::Below);
        assert_eq!(layout, ScreenLayout::built_in()[1].1);
        layout.move_block(B::Mixer, B::RightDecks, Edge::Right);
        assert_eq!(layout.columns.last(), Some(&vec![B::Mixer]));
        layout.move_block(B::Mixer, B::LeftDecks, Edge::Left);
        assert_eq!(layout.columns[0], vec![B::Mixer]);
    }

    #[test]
    fn repair_keeps_every_block_once() {
        let mut layout = ScreenLayout { columns: vec![vec![B::Mixer, B::Mixer], vec![]] };
        layout.repair();
        assert_eq!(layout.columns[0], vec![B::Mixer]);
        let count: usize = layout.columns.iter().map(Vec::len).sum();
        assert_eq!(count, ScreenBlock::ALL.len());
    }
}
