//! Detects a streamed reply stuck repeating one n-gram (T13.10, SPEC §5.5).

use std::collections::VecDeque;

/// Words kept for the check: about 400 tokens of English-like text.
const WINDOW_WORDS: usize = 400;
/// Longest repeated unit, in words.
const MAX_PERIOD: usize = 50;
/// Repeats of one unit, in a row, that count as a runaway.
const REPEATS: usize = 8;

/// Feeds on each streamed delta and says when the last words repeat one unit too often.
#[derive(Default)]
pub struct RunawayDetector {
    words: VecDeque<String>,
    partial: String,
}

impl RunawayDetector {
    /// Returns true once the window ends in a unit of up to `MAX_PERIOD` words repeated `REPEATS` times.
    pub fn push(&mut self, delta: &str) -> bool {
        for c in delta.chars() {
            if !c.is_whitespace() {
                self.partial.push(c);
            } else if !self.partial.is_empty() {
                let word = std::mem::take(&mut self.partial);
                self.words.push_back(word);
                if self.words.len() > WINDOW_WORDS {
                    self.words.pop_front();
                }
                if repeats(&self.words) {
                    return true;
                }
            }
        }
        false
    }
}

/// Checks each period from the shortest. A mismatch ends that period's check early, so the usual
/// case is far less than the full window.
// shortcut: words are whitespace-split, so a loop without spaces is one long word and goes unseen, upgrade when a model is seen doing that.
fn repeats(words: &VecDeque<String>) -> bool {
    let len = words.len();
    (1..=MAX_PERIOD).any(|period| {
        let span = period * REPEATS;
        len >= span && (len - span..len - period).all(|i| words[i] == words[i + period])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(detector: &mut RunawayDetector, text: &str) -> bool {
        text.split_inclusive(' ').any(|piece| detector.push(piece))
    }

    #[test]
    fn detects_one_word_repeated_eight_times() {
        let mut detector = RunawayDetector::default();
        assert!(feed(&mut detector, &"the ".repeat(8)));
    }

    #[test]
    fn ignores_seven_repeats() {
        let mut detector = RunawayDetector::default();
        assert!(!feed(&mut detector, &"the ".repeat(7)));
    }

    #[test]
    fn detects_a_multi_word_unit_across_chunk_boundaries() {
        let mut detector = RunawayDetector::default();
        let text = "let x = 1; ".repeat(8);
        // Split mid-word, so a word spans two deltas.
        let pieces: Vec<&str> = text
            .as_bytes()
            .chunks(3)
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert!(pieces.iter().any(|piece| detector.push(piece)));
    }

    #[test]
    fn ignores_ordinary_prose() {
        let mut detector = RunawayDetector::default();
        let prose = "The parser reads each token once, then builds the tree from the \
            remaining input and reports the first error it meets with its position.";
        assert!(!feed(&mut detector, &prose.repeat(3)));
    }

    #[test]
    fn detects_a_loop_of_a_long_unit() {
        let mut detector = RunawayDetector::default();
        let unit: String = (0..30).map(|i| format!("w{i} ")).collect();
        assert!(feed(&mut detector, &unit.repeat(8)));
    }
}
