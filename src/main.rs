// build.rs owns the only non-test call site, reaching this file through include! rather than the
// module tree, so the binary itself never links it.
#[cfg(test)]
mod plist;

mod activity;
mod capture;
mod config;
mod macos;
mod session;
mod sink;
mod writer;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use argh::FromArgs;

use crate::activity::DeviceSource;
use crate::activity::poller::{self, CoreAudio};
use crate::capture::{CaptureConfig, Tap};
use crate::config::Config;
use crate::session::{Settings, Shutdown};
use crate::sink::LocalFolder;

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

    let shutdown = Shutdown::new();
    if let Err(error) = shutdown.install() {
        eprintln!("mimi: the signal handlers could not be installed: {error}");
        return ExitCode::FAILURE;
    }

    let interval = Duration::from_millis(poll_interval_ms.into());
    let devices = CoreAudio::live().devices();
    let mut capture = Tap::new(CaptureConfig::new(AGGREGATE_NAME, AGGREGATE_UID));
    let (events, incoming) = mpsc::channel();
    let (stop, stopped) = mpsc::channel();
    thread::spawn(move || poller::poll(&CoreAudio::live(), interval, stopped, events));

    session::run(
        Settings {
            output_dir,
            prefixes: meeting_bundle_prefixes,
            sample_rate,
            bit_rate,
            grace: Duration::from_secs(stop_grace_seconds.into()),
        },
        devices,
        &mut capture,
        &LocalFolder,
        incoming,
        &shutdown,
    );

    drop(stop);
    ExitCode::SUCCESS
}

fn install() -> ExitCode {
    ExitCode::SUCCESS
}

fn uninstall() -> ExitCode {
    ExitCode::SUCCESS
}
