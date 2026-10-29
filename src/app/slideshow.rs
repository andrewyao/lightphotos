// SPDX-License-Identifier: MIT OR Apache-2.0

//! The slide show's settings, which the cog beside Play sets, and the order
//! a random slide show shows the Grid's photos in. The show itself is the
//! full-screen view's (`fullscreen.rs`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

const PREF: &str = "slideshow";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SlideshowSettings {
    /// How long each photo shows, in seconds, within `SECS`.
    pub(crate) secs: u32,
    /// Shuffle the photos rather than show them in grid order.
    pub(crate) random: bool,
    /// Zoom each photo to fill the screen and pan across what overflows,
    /// rather than fit the whole photo.
    pub(crate) fill: bool,
}

impl Default for SlideshowSettings {
    fn default() -> Self {
        SlideshowSettings {
            secs: 5,
            random: false,
            fill: true,
        }
    }
}

impl SlideshowSettings {
    pub(crate) const SECS: std::ops::RangeInclusive<u32> = 1..=60;

    pub(super) fn slide(self) -> Duration {
        Duration::from_secs(
            self.secs
                .clamp(*Self::SECS.start(), *Self::SECS.end())
                .into(),
        )
    }

    /// The saved settings, or the defaults when none are saved or they
    /// can't be read.
    #[cfg_attr(test, allow(dead_code, reason = "tests never load the saved value"))]
    pub(super) fn load() -> SlideshowSettings {
        let saved: SlideshowSettings = crate::persist::prefs::load(PREF)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        SlideshowSettings {
            secs: saved.secs.clamp(*Self::SECS.start(), *Self::SECS.end()),
            ..saved
        }
    }

    /// Remember the settings for the next launch. A storage failure costs
    /// one relaunch's settings, so it is logged, not surfaced.
    pub(super) fn save(self) {
        // A test must never write the developer's own settings.
        if cfg!(test) {
            return;
        }
        let saved = serde_json::to_string(&self)
            .map_err(|e| e.to_string())
            .and_then(|json| crate::persist::prefs::save(PREF, &json));
        if let Err(e) = saved {
            eprintln!("[lightphotos] could not save the slide show settings: {e}");
        }
    }
}

impl super::App {
    pub(crate) fn slideshow_settings(&self) -> SlideshowSettings {
        self.slideshow_settings
    }

    pub(super) fn set_slideshow_settings(&mut self, settings: SlideshowSettings) {
        if settings != self.slideshow_settings {
            self.slideshow_settings = settings;
            settings.save();
        }
    }
}

/// The Grid positions a random slide show has still to show, the next one
/// last. Every photo shows once before any shows again.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Deck {
    left: Vec<usize>,
    /// xorshift64 state. Shuffling photos needs no better randomness.
    rng: u64,
    /// Not shuffled yet.
    fresh: bool,
}

/// The clock's nanoseconds, to seed a shuffle or a coin flip.
pub(super) fn clock_seed() -> u64 {
    let nanos = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    u64::from(nanos)
}

/// The next number of an xorshift64 `state`, which must not be zero.
pub(super) fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

impl Deck {
    pub(super) fn new() -> Deck {
        Deck::seeded(clock_seed())
    }

    fn seeded(seed: u64) -> Deck {
        Deck {
            left: Vec::new(),
            // xorshift never leaves zero.
            rng: seed | 1,
            fresh: true,
        }
    }

    /// The photo after `cur` among `n`, without taking it, so it can load
    /// ahead.
    pub(super) fn peek(&self, cur: Option<usize>, n: usize) -> Option<usize> {
        self.left
            .iter()
            .rev()
            .copied()
            .find(|&p| p < n && Some(p) != cur)
    }

    /// Takes the photo after `cur` among `n`. The first shuffle leaves
    /// out the photo the show starts on; when the deck runs out, every
    /// photo is shuffled in again, the one just taken not first, so none
    /// shows twice in a row. `None` when there is no other photo.
    pub(super) fn take(&mut self, cur: Option<usize>, n: usize) -> Option<usize> {
        if self.peek(cur, n).is_none() {
            let start = self.fresh;
            self.shuffle(cur, n);
            if start {
                self.left.retain(|&p| Some(p) != cur);
            }
        }
        let next = self.peek(cur, n)?;
        while let Some(p) = self.left.pop() {
            if p == next {
                break;
            }
        }
        if self.left.is_empty() {
            self.shuffle(Some(next), n);
        }
        Some(next)
    }

    /// Fills the deck with every position in random order, `skip` not
    /// next.
    fn shuffle(&mut self, skip: Option<usize>, n: usize) {
        self.fresh = false;
        self.left = (0..n).collect();
        for i in (1..n).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            self.left.swap(i, j);
        }
        if n > 1 && self.left.last().copied() == skip {
            self.left.swap(0, n - 1);
        }
    }

    fn next_u64(&mut self) -> u64 {
        xorshift(&mut self.rng)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deck_shows_every_photo_once_before_any_again() {
        let mut deck = Deck::seeded(42);
        let n = 7;
        let mut cur = Some(3);
        for round in 0..4 {
            let mut seen = Vec::new();
            // The first round starts on photo 3, so it has one fewer to show.
            let count = if round == 0 { n - 1 } else { n };
            for _ in 0..count {
                let next = deck.take(cur, n);
                assert_ne!(next, cur, "a photo showed twice in a row");
                assert!(next.is_some_and(|p| p < n));
                seen.extend(next);
                cur = next;
            }
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), count, "round {round} repeated a photo");
        }
    }

    #[test]
    fn a_deck_follows_the_grid_shrinking_and_stops_on_one_photo() {
        let mut deck = Deck::seeded(7);
        assert!(deck.take(Some(0), 10).is_some());
        for _ in 0..20 {
            assert!(deck.take(None, 3).is_some_and(|p| p < 3));
        }
        assert_eq!(deck.take(Some(0), 1), None);
        assert_eq!(deck.take(None, 0), None);
    }

    #[test]
    fn hostile_saved_settings_load_within_range() {
        let s: SlideshowSettings = serde_json::from_str(r#"{"secs":0}"#).unwrap_or_default();
        assert_eq!(s.slide(), Duration::from_secs(1));
        let s: SlideshowSettings =
            serde_json::from_str(r#"{"secs":4000000000}"#).unwrap_or_default();
        assert_eq!(s.slide(), Duration::from_secs(60));
        let s: SlideshowSettings = serde_json::from_str("{}").unwrap_or_default();
        assert_eq!(s, SlideshowSettings::default());
    }
}
