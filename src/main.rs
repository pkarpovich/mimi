// build.rs owns the only non-test call site, reaching this file through include! rather than the
// module tree, so the binary itself never links it.
#[cfg(test)]
mod plist;

mod activity;
mod capture;
mod config;
mod macos;
mod session;
mod writer;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use argh::FromArgs;
use chrono::Local;

use crate::activity::poller::{self, CoreAudio};
use crate::activity::{ActivityEvent, AudioProcess, DeviceSource, Devices};
use crate::capture::{Capture, CaptureConfig, DEFAULT_FRAMES_PER_BLOCK, DEFAULT_SLOTS, Tap};
use crate::config::Config;
use crate::session::{Decider, SessionCommand, SessionStart};
use crate::writer::{Writer, WriterSettings};

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

    let Config {
        output_dir,
        meeting_bundle_prefixes,
        sample_rate,
        bit_rate,
        stop_grace_seconds,
        poll_interval_ms,
    } = config;

    let interval = Duration::from_millis(poll_interval_ms.into());
    let grace = Duration::from_secs(stop_grace_seconds.into());
    let mut decider = Decider::new(meeting_bundle_prefixes, grace);
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
                    recording = start_recording(
                        &mut capture,
                        &devices,
                        &Output {
                            dir: &output_dir,
                            label: &label,
                            sample_rate,
                            bit_rate,
                        },
                    );
                }
                SessionCommand::Stop => {
                    capture.stop();
                    match recording.take() {
                        Some(Recording { path, writer }) => match writer.finish() {
                            Some(error) => {
                                eprintln!("mimi: writing {} failed: {error}", path.display());
                            }
                            None => println!("session stop: wrote {}", path.display()),
                        },
                        None => println!("session stop"),
                    }
                }
            }
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
    path: PathBuf,
    writer: Writer,
}

struct Output<'a> {
    dir: &'a Path,
    label: &'a str,
    sample_rate: u32,
    bit_rate: u32,
}

fn start_recording(capture: &mut Tap, devices: &Devices, output: &Output<'_>) -> Option<Recording> {
    let Output {
        dir,
        label,
        sample_rate,
        bit_rate,
    } = output;
    if let Err(error) = fs::create_dir_all(dir) {
        eprintln!("mimi: {} is not usable: {error}", dir.display());
        return None;
    }
    let path = dir.join(format!(
        "{}-{label}.aac.partial",
        Local::now().format("%Y-%m-%dT%H-%M-%S")
    ));

    let (producer, consumer) = capture::ring(DEFAULT_SLOTS, DEFAULT_FRAMES_PER_BLOCK);
    let Err(error) = capture.start(devices, producer) else {
        println!(
            "capture started: {:?} Hz, tracks {:?}",
            capture.sample_rate(),
            capture.tracks()
        );
        let writer = writer::spawn(
            WriterSettings {
                path: path.clone(),
                sample_rate: *sample_rate,
                bit_rate: *bit_rate,
            },
            consumer,
            capture.formats(),
        );
        return Some(Recording { path, writer });
    };
    eprintln!("mimi: capture did not start: {error}");
    None
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
