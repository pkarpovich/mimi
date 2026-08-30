// build.rs owns the only non-test call site, reaching this file through include! rather than the
// module tree, so the binary itself never links it.
#[cfg(test)]
mod plist;

use argh::FromArgs;

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

fn main() {
    let Cli {
        check_config,
        command,
    } = argh::from_env();

    if check_config {
        return;
    }

    match command {
        None => run(),
        Some(Command::Run(RunCommand {})) => run(),
        Some(Command::Install(InstallCommand {})) => install(),
        Some(Command::Uninstall(UninstallCommand {})) => uninstall(),
    }
}

fn run() {}

fn install() {}

fn uninstall() {}
