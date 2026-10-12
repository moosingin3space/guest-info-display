//! Which clip plays next. Pure bookkeeping, no GStreamer, so it can be tested
//! without a pipeline.

/// Play order over `len` items, handed out one at a time.
#[derive(Debug)]
pub struct Playlist {
    looping: bool,
    shuffle: bool,
    order: Vec<usize>,
    /// Position in `order` of the next item to hand out.
    next: usize,
    /// Consecutive items that failed without producing a frame. Reaching the
    /// playlist length means nothing in it can play, so stop rather than spin.
    failed_in_a_row: usize,
    rng: fastrand::Rng,
}

impl Playlist {
    pub fn new(len: usize, looping: bool, shuffle: bool) -> Self {
        Self::with_rng(len, looping, shuffle, fastrand::Rng::new())
    }

    fn with_rng(len: usize, looping: bool, shuffle: bool, rng: fastrand::Rng) -> Self {
        let mut playlist = Self {
            looping,
            shuffle,
            order: (0..len).collect(),
            next: 0,
            failed_in_a_row: 0,
            rng,
        };
        if shuffle {
            playlist.rng.shuffle(&mut playlist.order);
        }
        playlist
    }

    /// The next item to play, or `None` once a non-looping list is done.
    pub fn advance(&mut self) -> Option<usize> {
        if self.order.is_empty() {
            return None;
        }
        if self.next == self.order.len() {
            if !self.looping {
                return None;
            }
            self.start_new_pass();
        }
        let item = self.order[self.next];
        self.next += 1;
        Some(item)
    }

    /// The current item put up a frame, so the list is not wholly broken.
    pub fn played(&mut self) {
        self.failed_in_a_row = 0;
    }

    /// The current item failed. `false` once every item has failed in a row:
    /// there is nothing left worth trying.
    pub fn failed(&mut self) -> bool {
        self.failed_in_a_row += 1;
        self.failed_in_a_row < self.order.len()
    }

    /// Reshuffle for the next pass, without opening it on the clip that just
    /// ended — back-to-back repeats are what make shuffle look broken.
    fn start_new_pass(&mut self) {
        self.next = 0;
        if !self.shuffle || self.order.len() < 2 {
            return;
        }
        let last = *self.order.last().expect("non-empty");
        self.rng.shuffle(&mut self.order);
        if self.order[0] == last {
            self.order.swap(0, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(p: &mut Playlist, n: usize) -> Vec<Option<usize>> {
        (0..n).map(|_| p.advance()).collect()
    }

    #[test]
    fn plays_in_order_and_stops_without_loop() {
        let mut p = Playlist::new(3, false, false);
        assert_eq!(take(&mut p, 5), [Some(0), Some(1), Some(2), None, None]);
    }

    #[test]
    fn loops_back_to_the_start() {
        let mut p = Playlist::new(2, true, false);
        assert_eq!(
            take(&mut p, 5),
            [Some(0), Some(1), Some(0), Some(1), Some(0)]
        );
    }

    #[test]
    fn a_single_looping_clip_repeats_forever() {
        let mut p = Playlist::new(1, true, true);
        assert_eq!(take(&mut p, 3), [Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn shuffle_plays_every_item_once_per_pass_and_never_repeats_across_passes() {
        let mut p = Playlist::with_rng(5, true, true, fastrand::Rng::with_seed(7));
        let mut previous = None;
        for _ in 0..20 {
            let pass: Vec<usize> = (0..5).map(|_| p.advance().expect("looping")).collect();
            let mut sorted = pass.clone();
            sorted.sort();
            assert_eq!(sorted, [0, 1, 2, 3, 4], "each pass is a permutation");
            if let Some(last) = previous {
                assert_ne!(
                    pass[0], last,
                    "a pass must not open on the clip that ended the last"
                );
            }
            previous = pass.last().copied();
        }
    }

    #[test]
    fn gives_up_once_every_item_has_failed_in_a_row() {
        let mut p = Playlist::new(3, true, false);
        p.advance();
        assert!(p.failed());
        p.advance();
        assert!(p.failed());
        p.advance();
        assert!(!p.failed(), "three of three failed");
    }

    #[test]
    fn one_good_clip_resets_the_failure_count() {
        let mut p = Playlist::new(2, true, false);
        p.advance();
        assert!(p.failed());
        p.advance();
        p.played();
        p.advance();
        assert!(p.failed(), "the good clip proved the list can still play");
    }

    #[test]
    fn an_empty_list_has_nothing_to_play() {
        let mut p = Playlist::new(0, true, true);
        assert_eq!(p.advance(), None);
    }
}
