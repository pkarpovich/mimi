// build.rs owns the only non-test call site, reaching this file through include! rather than the
// module tree, so the binary itself never links it.
#[cfg(test)]
mod plist;

mod activity;
mod capture;
mod config;
mod macos;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use argh::FromArgs;

use crate::activity::poller::{self, CoreAudio};
use crate::activity::{ActivityEvent, AudioProcess, DeviceSource, Devices};
use crate::capture::{
    Capture, CaptureConfig, Consumer, DEFAULT_FRAMES_PER_BLOCK, DEFAULT_SLOTS, Drained, Tap,
};
use crate::session::{Decider, SessionCommand, SessionStart};

const AGGREGATE_NAME: &str = "mimi";
const AGGREGATE_UID: &str = "dev.pkarpovich.mimi.aggregate";

/// mimi records meetings while a meeting application holds the microphone.
#[derive(FromArgs)]
struct Cli {
    /// load the configuration, print it, and exit
    #[argh(switch)]
    check_config: bool,

    #[argh(subcommand)]
    command: Option<Command>,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Command {
    Run(RunCommand),
    Install(InstallCommand),
    Uninstall(UninstallCommand),
}

/// watch for meetings and record them
#[derive(FromArgs)]
#[argh(subcommand, name = "run")]
struct RunCommand {}

/// install the launchd agent
#[derive(FromArgs)]
#[argh(subcommand, name = "install")]
struct InstallCommand {}

/// remove the launchd agent
#[derive(FromArgs)]
#[argh(subcommand, name = "uninstall")]
struct UninstallCommand {}

fn main() -> ExitCode {
    let Cli {
        check_config,
        command,
    } = argh::from_env();

    if check_config {
        return print_config();
    }

    match command {
        None => run(),
        Some(Command::Run(RunCommand {})) => run(),
        Some(Command::Install(InstallCommand {})) => install(),
        Some(Command::Uninstall(UninstallCommand {})) => uninstall(),
    }
}

fn print_config() -> ExitCode {
    let Some(home) = home_dir() else {
        eprintln!("mimi: HOME is not set");
        return ExitCode::FAILURE;
    };
    let config = match config::load(&home) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mimi: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("{config}");
    ExitCode::SUCCESS
}

fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home))
}

fn run() -> ExitCode {
    let Some(home) = home_dir() else {
        eprintln!("mimi: HOME is not set");
        return ExitCode::FAILURE;
    };
    let config = match config::load(&home) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("mimi: {error}");
            return ExitCode::FAILURE;
        }
    };

    let interval = Duration::from_millis(config.poll_interval_ms.into());
    let grace = Duration::from_secs(config.stop_grace_seconds.into());
    let mut decider = Decider::new(config.meeting_bundle_prefixes, grace);
    let sampler = CoreAudio::live();
    let mut devices = sampler.devices();
    let mut capture = Tap::new(CaptureConfig::new(AGGREGATE_NAME, AGGREGATE_UID));
    let mut recording: Option<Recording> = None;
    let (events, incoming) = mpsc::channel();
    let (_stop, stopped) = mpsc::channel();
    thread::spawn(move || poller::poll(&CoreAudio::live(), interval, stopped, events));

    for event in incoming {
        for command in decider.observe(&event, Instant::now()) {
            match command {
                SessionCommand::Start(SessionStart { bundle_id, label }) => {
                    println!("session start: {label} ({})", bundle_id.as_str());
                    recording = start_capture(&mut capture, &devices);
                }
                SessionCommand::Stop => {
                    capture.stop();
                    match &mut recording {
                        Some(recording) => {
                            drain(recording);
                            let Recording {
                                consumer: _,
                                blocks,
                                dropped,
                            } = recording;
                            println!("session stop: {blocks} blocks, {dropped} dropped");
                        }
                        None => println!("session stop"),
                    }
                    recording = None;
                }
            }
        }
        if let Some(recording) = &mut recording {
            drain(recording);
        }

        match event {
            ActivityEvent::InputTaken(process) => println!("input taken by {}", describe(&process)),
            ActivityEvent::InputReleased(process) => {
                println!("input released by {}", describe(&process));
            }
            ActivityEvent::DevicesChanged(sampled) => {
                println!("devices changed: {sampled:?}");
                devices = sampled;
                if recording.is_some() {
                    rebuild_capture(&mut capture, &devices);
                }
            }
            ActivityEvent::Tick => {}
        }
    }

    ExitCode::SUCCESS
}

struct Recording {
    consumer: Consumer,
    blocks: u64,
    dropped: u64,
}

fn start_capture(capture: &mut Tap, devices: &Devices) -> Option<Recording> {
    let (producer, consumer) = capture::ring(DEFAULT_SLOTS, DEFAULT_FRAMES_PER_BLOCK);
    let Err(error) = capture.start(devices, producer) else {
        println!(
            "capture started: {:?} Hz, tracks {:?}",
            capture.sample_rate(),
            capture.tracks()
        );
        return Some(Recording {
            consumer,
            blocks: 0,
            dropped: 0,
        });
    };
    eprintln!("mimi: capture did not start: {error}");
    None
}

fn drain(recording: &mut Recording) {
    let Recording {
        consumer,
        blocks,
        dropped,
    } = recording;
    let Drained {
        blocks: drained,
        dropped: missed,
    } = consumer.drain();
    *blocks += drained.len() as u64;
    *dropped += missed;
}

fn rebuild_capture(capture: &mut Tap, devices: &Devices) {
    let Err(error) = capture.rebuild(devices) else {
        println!("capture rebuilt: {:?} Hz", capture.sample_rate());
        return;
    };
    eprintln!("mimi: capture did not rebuild: {error}");
}

fn describe(process: &AudioProcess) -> String {
    let AudioProcess {
        object,
        bundle_id,
        pid,
        input: _,
        output: _,
    } = process;
    let bundle_id = match bundle_id {
        Some(bundle_id) => bundle_id.as_str(),
        None => "<unknown>",
    };
    format!("{object} pid={pid} {bundle_id}")
}

fn install() -> ExitCode {
    ExitCode::SUCCESS
}

fn uninstall() -> ExitCode {
    ExitCode::SUCCESS
}
