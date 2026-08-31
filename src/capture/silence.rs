use std::time::Duration;

/// OPENING_WINDOW is how much of a capture's opening is examined before a verdict is reached.
pub const OPENING_WINDOW: Duration = Duration::from_secs(3);

/// Verdict is what the detector can say about the window it has been fed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Undecided,
    Silent,
    AudioPresent,
}

/// Silence watches the opening window of a capture for the all-zero samples a dead tap delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Silence {
    window: Duration,
    elapsed: Duration,
    heard: Heard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heard {
    Nothing,
    Audio,
}

impl Silence {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            elapsed: Duration::ZERO,
            heard: Heard::Nothing,
        }
    }

    /// feed adds one block of samples and the span of time they cover to the window.
    pub fn feed(&mut self, samples: &[f32], duration: Duration) {
        let Self {
            window,
            elapsed,
            heard,
        } = self;
        match heard {
            Heard::Audio => return,
            Heard::Nothing => {}
        }
        if elapsed >= window {
            return;
        }
        for sample in samples {
            if *sample != 0.0 {
                *heard = Heard::Audio;
                break;
            }
        }
        *elapsed = (*elapsed + duration).min(*window);
    }

    pub fn verdict(&self) -> Verdict {
        let Self {
            window,
            elapsed,
            heard,
        } = self;
        match heard {
            Heard::Audio => Verdict::AudioPresent,
            Heard::Nothing => {
                if elapsed >= window {
                    Verdict::Silent
                } else {
                    Verdict::Undecided
                }
            }
        }
    }

    /// reset restarts the window so the opening of a rebuilt capture is examined again.
    pub fn reset(&mut self) {
        *self = Self::new(self.window);
    }
}

/// settled folds a closed window's verdict into the one a session reports; silence is sticky.
pub fn settled(session: Verdict, window: Verdict) -> Verdict {
    match (session, window) {
        (Verdict::Silent, Verdict::Silent) => Verdict::Silent,
        (Verdict::Silent, Verdict::AudioPresent) => Verdict::Silent,
        (Verdict::Silent, Verdict::Undecided) => Verdict::Silent,
        (Verdict::AudioPresent, Verdict::Silent) => Verdict::Silent,
        (Verdict::AudioPresent, Verdict::AudioPresent) => Verdict::AudioPresent,
        (Verdict::AudioPresent, Verdict::Undecided) => Verdict::AudioPresent,
        (Verdict::Undecided, Verdict::Silent) => Verdict::Silent,
        (Verdict::Undecided, Verdict::AudioPresent) => Verdict::AudioPresent,
        (Verdict::Undecided, Verdict::Undecided) => Verdict::Undecided,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(2);
    const HALF: Duration = Duration::from_secs(1);

    #[test]
    fn a_full_window_of_zero_samples_is_silence() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[0.0; 64], HALF);
        silence.feed(&[0.0; 64], HALF);
        assert_eq!(silence.verdict(), Verdict::Silent);
    }

    #[test]
    fn an_incomplete_window_of_zero_samples_stays_undecided() {
        let mut silence = Silence::new(WINDOW);
        assert_eq!(silence.verdict(), Verdict::Undecided);
        silence.feed(&[0.0; 64], HALF);
        assert_eq!(silence.verdict(), Verdict::Undecided);
    }

    #[test]
    fn a_single_non_zero_sample_flips_the_verdict() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[0.0, 0.0, 0.0, -0.000_1], HALF);
        assert_eq!(silence.verdict(), Verdict::AudioPresent);
        silence.feed(&[0.0; 64], HALF);
        assert_eq!(
            silence.verdict(),
            Verdict::AudioPresent,
            "audio once heard is not unheard by later zeros"
        );
    }

    #[test]
    fn samples_arriving_after_the_window_closed_do_not_change_the_verdict() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[0.0; 64], WINDOW);
        assert_eq!(silence.verdict(), Verdict::Silent);
        silence.feed(&[0.5; 64], HALF);
        assert_eq!(silence.verdict(), Verdict::Silent);
    }

    #[test]
    fn reset_restarts_the_window() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[0.5; 64], WINDOW);
        assert_eq!(silence.verdict(), Verdict::AudioPresent);

        silence.reset();
        assert_eq!(silence.verdict(), Verdict::Undecided);
        silence.feed(&[0.0; 64], HALF);
        assert_eq!(silence.verdict(), Verdict::Undecided);
        silence.feed(&[0.0; 64], HALF);
        assert_eq!(silence.verdict(), Verdict::Silent);
    }

    #[test]
    fn an_empty_block_still_advances_the_window() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[], WINDOW);
        assert_eq!(silence.verdict(), Verdict::Silent);
    }

    #[test]
    fn a_window_that_fired_is_what_the_session_keeps_reporting() {
        assert_eq!(
            settled(Verdict::Silent, Verdict::AudioPresent),
            Verdict::Silent,
            "a window that opened on silence is not unsaid by a later one that heard audio"
        );
        assert_eq!(
            settled(Verdict::AudioPresent, Verdict::Silent),
            Verdict::Silent
        );
        assert_eq!(
            settled(Verdict::Silent, Verdict::Undecided),
            Verdict::Silent
        );
        assert_eq!(
            settled(Verdict::AudioPresent, Verdict::Undecided),
            Verdict::AudioPresent,
            "a rebuild too late to fill its window does not erase what was already judged"
        );
    }

    #[test]
    fn a_session_that_judged_nothing_stays_undecided() {
        assert_eq!(
            settled(Verdict::Undecided, Verdict::Undecided),
            Verdict::Undecided
        );
        assert_eq!(
            settled(Verdict::Undecided, Verdict::AudioPresent),
            Verdict::AudioPresent
        );
        assert_eq!(
            settled(Verdict::Undecided, Verdict::Silent),
            Verdict::Silent
        );
    }

    #[test]
    fn the_opening_window_is_bounded() {
        let silence = Silence::new(OPENING_WINDOW);
        assert_eq!(silence.verdict(), Verdict::Undecided);
        assert_eq!(OPENING_WINDOW, Duration::from_secs(3));
    }
}
