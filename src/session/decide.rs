use std::time::{Duration, Instant};

use objc2_core_audio::AudioObjectID;

use crate::activity::{ActivityEvent, AudioProcess, BundleId};
use crate::config::BundlePrefix;

/// SessionStart carries what a starting session knows about the process that triggered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStart {
    pub bundle_id: BundleId,
    pub label: String,
}

/// SessionCommand is what the run loop must do after an activity event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Start(SessionStart),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Recording,
    PendingStop { since: Instant },
}

/// Decider turns activity events into session start and stop commands.
pub struct Decider {
    prefixes: Vec<BundlePrefix>,
    grace: Duration,
    holders: Vec<AudioObjectID>,
    state: State,
}

impl Decider {
    pub fn new(prefixes: Vec<BundlePrefix>, grace: Duration) -> Self {
        Self {
            prefixes,
            grace,
            holders: Vec::new(),
            state: State::Idle,
        }
    }

    /// observe advances the session state and reports what the run loop must do.
    pub fn observe(&mut self, event: &ActivityEvent, now: Instant) -> Vec<SessionCommand> {
        let mut commands = Vec::new();
        if self.grace_elapsed(now) {
            self.state = State::Idle;
            self.holders.clear();
            commands.push(SessionCommand::Stop);
        }
        match event {
            ActivityEvent::InputTaken(process) => self.take(process, &mut commands),
            ActivityEvent::InputReleased(process) => self.release(process, now),
            ActivityEvent::DevicesChanged(_) => {}
            ActivityEvent::Tick => {}
        }
        commands
    }

    fn grace_elapsed(&self, now: Instant) -> bool {
        let State::PendingStop { since } = self.state else {
            return false;
        };
        now.duration_since(since) >= self.grace
    }

    fn take(&mut self, process: &AudioProcess, commands: &mut Vec<SessionCommand>) {
        let AudioProcess {
            object,
            bundle_id,
            pid: _,
            input: _,
            output: _,
        } = process;
        let Some(bundle_id) = bundle_id else {
            return;
        };
        let Some(prefix) = matches(&self.prefixes, bundle_id) else {
            return;
        };
        if !self.holders.contains(object) {
            self.holders.push(*object);
        }
        match self.state {
            State::Recording => {}
            State::PendingStop { since: _ } => self.state = State::Recording,
            State::Idle => {
                self.state = State::Recording;
                commands.push(SessionCommand::Start(SessionStart {
                    bundle_id: bundle_id.clone(),
                    label: label(&prefix),
                }));
            }
        }
    }

    fn release(&mut self, process: &AudioProcess, now: Instant) {
        let AudioProcess {
            object,
            bundle_id: _,
            pid: _,
            input: _,
            output: _,
        } = process;
        let mut kept = Vec::with_capacity(self.holders.len());
        for held in &self.holders {
            if held == object {
                continue;
            }
            kept.push(*held);
        }
        self.holders = kept;
        if !self.holders.is_empty() {
            return;
        }
        match self.state {
            State::Idle => {}
            State::PendingStop { since: _ } => {}
            State::Recording => self.state = State::PendingStop { since: now },
        }
    }
}

/// matches reports which allow-list prefix a bundle id starts with.
pub fn matches(prefixes: &[BundlePrefix], bundle: &BundleId) -> Option<BundlePrefix> {
    let mut found = None;
    for prefix in prefixes {
        if bundle.as_str().starts_with(prefix.as_str()) {
            found = Some(prefix.clone());
            break;
        }
    }
    found
}

/// label reduces an allow-list prefix to the word a recording's file name carries.
pub fn label(prefix: &BundlePrefix) -> String {
    let prefix = prefix.as_str().to_lowercase();
    let prefix = prefix.trim_end_matches('.');
    let Some(dot) = prefix.rfind('.') else {
        return prefix.to_owned();
    };
    prefix[dot + 1..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{DeviceUid, Devices, InputState, OutputState};

    const GRACE: Duration = Duration::from_secs(15);

    fn prefixes(values: &[&str]) -> Vec<BundlePrefix> {
        let mut wrapped = Vec::new();
        for value in values {
            wrapped.push(BundlePrefix::new(*value));
        }
        wrapped
    }

    fn default_prefixes() -> Vec<BundlePrefix> {
        prefixes(&[
            "company.thebrowser.",
            "us.zoom.",
            "com.microsoft.teams2",
            "com.tinyspeck.slackmacgap",
            "com.google.Chrome",
        ])
    }

    fn process(object: AudioObjectID, bundle: &str, input: InputState) -> AudioProcess {
        AudioProcess {
            object,
            bundle_id: Some(BundleId::new(bundle)),
            pid: 900 + object as i32,
            input,
            output: OutputState::Idle,
        }
    }

    fn taken(object: AudioObjectID, bundle: &str) -> ActivityEvent {
        ActivityEvent::InputTaken(process(object, bundle, InputState::Running))
    }

    fn released(object: AudioObjectID, bundle: &str) -> ActivityEvent {
        ActivityEvent::InputReleased(process(object, bundle, InputState::Idle))
    }

    fn start(bundle: &str, label: &str) -> SessionCommand {
        SessionCommand::Start(SessionStart {
            bundle_id: BundleId::new(bundle),
            label: label.to_owned(),
        })
    }

    fn decider() -> Decider {
        Decider::new(default_prefixes(), GRACE)
    }

    #[test]
    fn an_exact_bundle_id_matches_its_prefix() {
        let matched = matches(&default_prefixes(), &BundleId::new("com.microsoft.teams2"));
        assert_eq!(matched, Some(BundlePrefix::new("com.microsoft.teams2")));
    }

    #[test]
    fn a_helper_process_matches_the_prefix_of_its_app() {
        let matched = matches(
            &default_prefixes(),
            &BundleId::new("company.thebrowser.browser.helper"),
        );
        assert_eq!(matched, Some(BundlePrefix::new("company.thebrowser.")));
    }

    #[test]
    fn a_foreign_bundle_id_matches_nothing() {
        assert_eq!(
            matches(&default_prefixes(), &BundleId::new("com.apple.CoreSpeech")),
            None
        );
    }

    #[test]
    fn an_empty_prefix_list_matches_nothing() {
        assert_eq!(matches(&[], &BundleId::new("us.zoom.xos")), None);
    }

    #[test]
    fn the_first_matching_prefix_is_reported() {
        let prefixes = prefixes(&["com.google.Chrome", "com."]);
        assert_eq!(
            matches(&prefixes, &BundleId::new("com.google.Chrome.helper")),
            Some(BundlePrefix::new("com.google.Chrome"))
        );
    }

    #[test]
    fn every_default_prefix_reduces_to_its_last_component() {
        let expected = ["thebrowser", "zoom", "teams2", "slackmacgap", "chrome"];
        let mut labels = Vec::new();
        for prefix in default_prefixes() {
            labels.push(label(&prefix));
        }
        assert_eq!(labels, expected);
    }

    #[test]
    fn a_prefix_without_a_dot_is_its_own_label() {
        assert_eq!(label(&BundlePrefix::new("Zoom")), "zoom");
    }

    #[test]
    fn a_matching_take_starts_a_session() {
        let mut decider = decider();
        let now = Instant::now();
        assert_eq!(
            decider.observe(&taken(1, "company.thebrowser.browser.helper"), now),
            vec![start("company.thebrowser.browser.helper", "thebrowser")]
        );
    }

    #[test]
    fn a_second_holder_of_a_running_session_does_not_start_a_second_one() {
        let mut decider = decider();
        let now = Instant::now();
        assert_eq!(
            decider.observe(&taken(1, "us.zoom.xos"), now),
            vec![start("us.zoom.xos", "zoom")]
        );
        assert_eq!(decider.observe(&taken(2, "us.zoom.caphost"), now), vec![]);
    }

    #[test]
    fn a_session_stops_once_the_grace_period_elapses() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(1, "us.zoom.xos"), now);
        assert_eq!(
            decider.observe(&released(1, "us.zoom.xos"), now + Duration::from_secs(60)),
            Vec::new()
        );
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(74)),
            Vec::new()
        );
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(75)),
            vec![SessionCommand::Stop]
        );
    }

    #[test]
    fn a_re_take_inside_the_grace_period_keeps_one_session() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(1, "us.zoom.xos"), now);
        decider.observe(&released(1, "us.zoom.xos"), now + Duration::from_secs(10));
        assert_eq!(
            decider.observe(&taken(1, "us.zoom.xos"), now + Duration::from_secs(20)),
            Vec::new()
        );
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(120)),
            Vec::new()
        );
    }

    #[test]
    fn a_take_after_the_grace_period_stops_the_old_session_and_starts_a_new_one() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(1, "us.zoom.xos"), now);
        decider.observe(&released(1, "us.zoom.xos"), now + Duration::from_secs(10));
        assert_eq!(
            decider.observe(&taken(1, "us.zoom.xos"), now + Duration::from_secs(40)),
            vec![SessionCommand::Stop, start("us.zoom.xos", "zoom")]
        );
    }

    #[test]
    fn a_synthesized_take_starts_a_session_for_a_daemon_launched_mid_meeting() {
        let mut decider = decider();
        let now = Instant::now();
        assert_eq!(
            decider.observe(&taken(4, "com.microsoft.teams2.modulehost"), now),
            vec![start("com.microsoft.teams2.modulehost", "teams2")]
        );
    }

    #[test]
    fn a_synthesized_release_ends_the_session_of_a_crashed_app() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(4, "com.tinyspeck.slackmacgap"), now);
        decider.observe(
            &ActivityEvent::InputReleased(process(
                4,
                "com.tinyspeck.slackmacgap",
                InputState::Running,
            )),
            now,
        );
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + GRACE),
            vec![SessionCommand::Stop]
        );
    }

    #[test]
    fn two_overlapping_meeting_apps_produce_one_session() {
        let mut decider = decider();
        let now = Instant::now();
        assert_eq!(
            decider.observe(&taken(1, "company.thebrowser.browser.helper"), now),
            vec![start("company.thebrowser.browser.helper", "thebrowser")]
        );
        assert_eq!(
            decider.observe(&taken(2, "us.zoom.xos"), now + Duration::from_secs(1)),
            Vec::new()
        );
        decider.observe(
            &released(1, "company.thebrowser.browser.helper"),
            now + Duration::from_secs(2),
        );
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(600)),
            Vec::new()
        );
        decider.observe(&released(2, "us.zoom.xos"), now + Duration::from_secs(601));
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(616)),
            vec![SessionCommand::Stop]
        );
    }

    #[test]
    fn a_non_matching_process_is_ignored_throughout() {
        let mut decider = decider();
        let now = Instant::now();
        assert_eq!(
            decider.observe(&taken(9, "com.apple.CoreSpeech"), now),
            Vec::new()
        );
        assert_eq!(
            decider.observe(&released(9, "com.apple.CoreSpeech"), now),
            Vec::new()
        );
        decider.observe(&taken(1, "us.zoom.xos"), now);
        decider.observe(&released(9, "com.apple.CoreSpeech"), now);
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(600)),
            Vec::new()
        );
    }

    #[test]
    fn a_process_without_a_bundle_id_never_starts_a_session() {
        let mut decider = decider();
        let anonymous = AudioProcess {
            object: 3,
            bundle_id: None,
            pid: 77,
            input: InputState::Running,
            output: OutputState::Idle,
        };
        assert_eq!(
            decider.observe(&ActivityEvent::InputTaken(anonymous), Instant::now()),
            Vec::new()
        );
    }

    #[test]
    fn a_devices_changed_event_leaves_the_session_alone() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(1, "us.zoom.xos"), now);
        let devices = ActivityEvent::DevicesChanged(Devices {
            input: Some(DeviceUid::new("20-F4-D4-60-E4-29:input")),
            output: Some(DeviceUid::new("20-F4-D4-60-E4-29:output")),
            sample_rate: Some(24_000.0),
        });
        assert_eq!(decider.observe(&devices, now), Vec::new());
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + Duration::from_secs(600)),
            Vec::new()
        );
    }

    #[test]
    fn a_repeated_take_from_one_process_holds_the_session_once() {
        let mut decider = decider();
        let now = Instant::now();
        decider.observe(&taken(1, "us.zoom.xos"), now);
        decider.observe(&taken(1, "us.zoom.xos"), now);
        decider.observe(&released(1, "us.zoom.xos"), now);
        assert_eq!(
            decider.observe(&ActivityEvent::Tick, now + GRACE),
            vec![SessionCommand::Stop]
        );
    }
}
