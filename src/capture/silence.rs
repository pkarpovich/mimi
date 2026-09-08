use std::time::Duration;

/// SILENCE_WINDOW is how much of a capture has to be seen before all-zero samples count as silence.
pub const SILENCE_WINDOW: Duration = Duration::from_secs(3);

/// Verdict is what the detector can say about the capture it has been fed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Undecided,
    Silent,
    AudioPresent,
}

/// Silence watches a whole capture for the all-zero samples a dead tap delivers.
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

    /// feed adds one block of samples and the span of time they cover to what was examined.
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

    /// reset restarts the judgement so a rebuilt capture is examined on its own.
    pub fn reset(&mut self) {
        *self = Self::new(self.window);
    }
}

/// settled folds a capture's verdict into the one a session reports; audio anywhere is audio.
pub fn settled(session: Verdict, capture: Verdict) -> Verdict {
    match (session, capture) {
        (Verdict::Silent, Verdict::Silent) => Verdict::Silent,
        (Verdict::Silent, Verdict::AudioPresent) => Verdict::AudioPresent,
        (Verdict::Silent, Verdict::Undecided) => Verdict::Silent,
        (Verdict::AudioPresent, Verdict::Silent) => Verdict::AudioPresent,
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
    fn audio_arriving_after_the_window_closed_still_flips_the_verdict() {
        let mut silence = Silence::new(WINDOW);
        silence.feed(&[0.0; 64], WINDOW);
        assert_eq!(silence.verdict(), Verdict::Silent);
        silence.feed(&[0.5; 64], HALF);
        assert_eq!(
            silence.verdict(),
            Verdict::AudioPresent,
            "a capture whose devices took seconds to deliver is not a silent one"
        );
    }

    #[test]
    fn reset_restarts_the_judgement() {
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
    fn audio_in_any_capture_is_what_the_session_reports() {
        assert_eq!(
            settled(Verdict::Silent, Verdict::AudioPresent),
            Verdict::AudioPresent,
            "a capture that heard audio is not unsaid by a dead one before it"
        );
        assert_eq!(
            settled(Verdict::AudioPresent, Verdict::Silent),
            Verdict::AudioPresent,
            "a rebuild that came back dead does not erase the audio already recorded"
        );
        assert_eq!(
            settled(Verdict::Silent, Verdict::Undecided),
            Verdict::Silent
        );
        assert_eq!(
            settled(Verdict::AudioPresent, Verdict::Undecided),
            Verdict::AudioPresent,
            "a rebuild too short to judge does not erase what was already judged"
        );
        assert_eq!(
            settled(Verdict::Silent, Verdict::Silent),
            Verdict::Silent,
            "a session that never heard anything is the one silence is for"
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
    fn the_silence_window_is_bounded() {
        let silence = Silence::new(SILENCE_WINDOW);
        assert_eq!(silence.verdict(), Verdict::Undecided);
        assert_eq!(SILENCE_WINDOW, Duration::from_secs(3));
    }

    #[test]
    fn a_capture_that_only_starts_delivering_after_the_window_is_not_silent() {
        let mut silence = Silence::new(SILENCE_WINDOW);
        for _ in 0..8 {
            silence.feed(&[0.0; 1024], Duration::from_millis(500));
        }
        assert_eq!(silence.verdict(), Verdict::Silent);

        silence.feed(&[0.0, 0.0, 0.25], Duration::from_millis(500));
        assert_eq!(
            silence.verdict(),
            Verdict::AudioPresent,
            "a meeting joined after the recording started is audio, not digital silence"
        );
    }
}
