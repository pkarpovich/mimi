use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

const LAUNCHCTL: &str = "/bin/launchctl";

/// LABEL is the launchd label mimi's user agent is addressed by.
pub const LABEL: &str = "dev.pkarpovich.mimi";

/// Layout is where the launchd agent and its logs live under a home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub agent: PathBuf,
    pub out_log: PathBuf,
    pub err_log: PathBuf,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("the running executable could not be located: {0}")]
    Program(io::Error),
    #[error("creating {path}: {source}")]
    Directory { path: PathBuf, source: io::Error },
    #[error("writing {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("removing {path}: {source}")]
    Remove { path: PathBuf, source: io::Error },
    #[error("launchctl {arguments} could not run: {source}")]
    Unavailable {
        arguments: String,
        source: io::Error,
    },
    #[error("launchctl {arguments} refused: {reason}")]
    Refused { arguments: String, reason: String },
}

/// layout resolves the agent plist and both log files under a home directory.
pub fn layout(home: &Path) -> Layout {
    Layout {
        agent: home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")),
        out_log: home.join("Library").join("Logs").join("mimi.log"),
        err_log: home.join("Library").join("Logs").join("mimi.err.log"),
    }
}

/// render describes the LaunchAgent that keeps the given executable running for this user.
pub fn render(program: &Path, layout: &Layout) -> String {
    let Layout {
        agent: _,
        out_log,
        err_log,
    } = layout;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>run</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{out}</string>
	<key>StandardErrorPath</key>
	<string>{err}</string>
</dict>
</plist>
"#,
        label = escape(LABEL),
        program = escape(&program.display().to_string()),
        out = escape(&out_log.display().to_string()),
        err = escape(&err_log.display().to_string()),
    )
}

/// install writes the agent for the running executable and asks launchd to load it.
pub fn install(home: &Path, user: u32) -> Result<PathBuf, ServiceError> {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(source) => return Err(ServiceError::Program(source)),
    };
    let layout = layout(home);
    let Layout {
        agent,
        out_log,
        err_log,
    } = &layout;

    create_parent(agent)?;
    create_parent(out_log)?;
    create_parent(err_log)?;
    if let Err(source) = fs::write(agent, render(&program, &layout)) {
        return Err(ServiceError::Write {
            path: agent.clone(),
            source,
        });
    }

    let _ = launchctl(&["bootout".to_owned(), service_target(user)]);
    launchctl(&[
        "bootstrap".to_owned(),
        domain_target(user),
        agent.display().to_string(),
    ])?;
    Ok(agent.clone())
}

/// uninstall unloads the agent and removes its plist, leaving recordings and logs alone.
pub fn uninstall(home: &Path, user: u32) -> Result<PathBuf, ServiceError> {
    let layout = layout(home);
    let Layout {
        agent,
        out_log: _,
        err_log: _,
    } = &layout;

    let _ = launchctl(&["bootout".to_owned(), service_target(user)]);
    if !agent.exists() {
        return Ok(agent.clone());
    }
    if let Err(source) = fs::remove_file(agent) {
        return Err(ServiceError::Remove {
            path: agent.clone(),
            source,
        });
    }
    Ok(agent.clone())
}

fn domain_target(user: u32) -> String {
    format!("gui/{user}")
}

fn service_target(user: u32) -> String {
    format!("gui/{user}/{LABEL}")
}

fn create_parent(path: &Path) -> Result<(), ServiceError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Err(source) = fs::create_dir_all(parent) else {
        return Ok(());
    };
    Err(ServiceError::Directory {
        path: parent.to_path_buf(),
        source,
    })
}

fn launchctl(arguments: &[String]) -> Result<(), ServiceError> {
    let described = arguments.join(" ");
    let outcome = match Command::new(LAUNCHCTL).args(arguments).output() {
        Ok(outcome) => outcome,
        Err(source) => {
            return Err(ServiceError::Unavailable {
                arguments: described,
                source,
            });
        }
    };
    if outcome.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&outcome.stderr).trim().to_owned();
    let reason = match reason.is_empty() {
        true => outcome.status.to_string(),
        false => reason,
    };
    Err(ServiceError::Refused {
        arguments: described,
        reason,
    })
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/pavel.karpovich")
    }

    #[test]
    fn the_layout_puts_the_agent_and_the_logs_under_the_given_home() {
        let Layout {
            agent,
            out_log,
            err_log,
        } = layout(&home());
        assert_eq!(
            agent,
            home().join("Library/LaunchAgents/dev.pkarpovich.mimi.plist")
        );
        assert_eq!(out_log, home().join("Library/Logs/mimi.log"));
        assert_eq!(err_log, home().join("Library/Logs/mimi.err.log"));
    }

    #[test]
    fn a_different_home_moves_every_path_with_it() {
        let home = PathBuf::from("/var/empty");
        let Layout {
            agent,
            out_log,
            err_log,
        } = layout(&home);
        assert!(agent.starts_with(&home));
        assert!(out_log.starts_with(&home));
        assert!(err_log.starts_with(&home));
    }

    #[test]
    fn the_agent_carries_the_program_path_the_label_and_both_logs() {
        let layout = layout(&home());
        let program = home().join(".cargo/bin/mimi");
        let rendered = render(&program, &layout);

        assert!(
            rendered.contains("<string>/Users/pavel.karpovich/.cargo/bin/mimi</string>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<key>Label</key>\n\t<string>dev.pkarpovich.mimi</string>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<string>/Users/pavel.karpovich/Library/Logs/mimi.log</string>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<string>/Users/pavel.karpovich/Library/Logs/mimi.err.log</string>"),
            "{rendered}"
        );
    }

    #[test]
    fn the_agent_runs_at_load_keeps_alive_and_asks_for_the_run_subcommand() {
        let rendered = render(Path::new("/usr/local/bin/mimi"), &layout(&home()));
        assert!(
            rendered.contains("<key>RunAtLoad</key>\n\t<true/>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("<key>KeepAlive</key>\n\t<true/>"),
            "{rendered}"
        );
        assert!(rendered.contains("<string>run</string>"), "{rendered}");
    }

    #[test]
    fn a_path_needing_xml_escaping_is_escaped() {
        let program = Path::new("/Users/a&b/<mimi>/\"quoted\"/'single'");
        let rendered = render(program, &layout(Path::new("/Users/a&b")));

        assert!(
            rendered.contains(
                "<string>/Users/a&amp;b/&lt;mimi&gt;/&quot;quoted&quot;/&apos;single&apos;</string>"
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("/Users/a&b/"),
            "no raw ampersand survives: {rendered}"
        );
        assert!(
            rendered.contains("<string>/Users/a&amp;b/Library/Logs/mimi.log</string>"),
            "the log paths are escaped too: {rendered}"
        );
    }

    #[test]
    fn the_launchd_targets_name_the_user_domain_and_the_service_inside_it() {
        assert_eq!(domain_target(501), "gui/501");
        assert_eq!(service_target(501), "gui/501/dev.pkarpovich.mimi");
    }

    #[test]
    fn a_launchctl_that_is_not_there_is_reported_rather_than_panicking() {
        let failure = launchctl(&["print".to_owned()]).expect_err("launchctl refuses a bare print");
        match failure {
            ServiceError::Refused { arguments, reason } => {
                assert_eq!(arguments, "print");
                assert!(!reason.is_empty());
            }
            ServiceError::Unavailable {
                arguments: _,
                source: _,
            } => {}
            ServiceError::Program(_)
            | ServiceError::Directory { path: _, source: _ }
            | ServiceError::Write { path: _, source: _ }
            | ServiceError::Remove { path: _, source: _ } => panic!("{failure}"),
        }
    }

    #[test]
    fn uninstalling_an_agent_that_was_never_installed_is_not_a_failure() {
        let home = std::env::temp_dir().join(format!("mimi-service-test-{}", std::process::id()));
        fs::create_dir_all(&home).expect("create the fake home");
        let agent = uninstall(&home, 0).expect("nothing to remove");
        assert!(!agent.exists());
        let _ = fs::remove_dir_all(&home);
    }
}
