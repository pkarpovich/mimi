// build.rs owns the only non-test call site, reaching this file through include! rather than the
// module tree, so the binary itself never links it.
#[cfg(test)]
mod plist;

mod activity;
mod config;
mod macos;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use argh::FromArgs;

use crate::activity::poller::{self, CoreAudio};
use crate::activity::{ActivityEvent, AudioProcess};

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
    let (events, incoming) = mpsc::channel();
    let (_stop, stopped) = mpsc::channel();
    thread::spawn(move || poller::poll(&CoreAudio::live(), interval, stopped, events));

    for event in incoming {
        match event {
            ActivityEvent::InputTaken(process) => println!("input taken by {}", describe(&process)),
            ActivityEvent::InputReleased(process) => {
                println!("input released by {}", describe(&process));
            }
            ActivityEvent::DevicesChanged(devices) => println!("devices changed: {devices:?}"),
            ActivityEvent::Tick => {}
        }
    }

    ExitCode::SUCCESS
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
