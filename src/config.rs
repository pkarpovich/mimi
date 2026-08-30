use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const CONFIG_RELATIVE_PATH: &str = ".config/mimi/config.toml";
const DEFAULT_OUTPUT_DIR: &str = "Recordings/mimi";
const DEFAULT_SAMPLE_RATE: u32 = 24_000;
const DEFAULT_BIT_RATE: u32 = 96_000;
const DEFAULT_STOP_GRACE_SECONDS: u32 = 15;
const DEFAULT_POLL_INTERVAL_MS: u32 = 1_000;
const DEFAULT_BUNDLE_PREFIXES: [&str; 5] = [
    "company.thebrowser.",
    "us.zoom.",
    "com.microsoft.teams2",
    "com.tinyspeck.slackmacgap",
    "com.google.Chrome",
];

/// BundlePrefix is a prefix a process bundle id must start with to be treated as a meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePrefix(String);

impl BundlePrefix {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub output_dir: PathBuf,
    pub meeting_bundle_prefixes: Vec<BundlePrefix>,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub stop_grace_seconds: u32,
    pub poll_interval_ms: u32,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("malformed configuration: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("{field} must be positive, got {value}")]
    NotPositive { field: &'static str, value: i64 },
    #[error("{field} is too large, got {value}")]
    TooLarge { field: &'static str, value: i64 },
    #[error("meeting_bundle_prefixes must not be empty")]
    NoBundlePrefixes,
    #[error("meeting_bundle_prefixes must not carry a blank entry, which matches every process")]
    BlankBundlePrefix,
    #[error("output_dir must not be empty")]
    EmptyOutputDir,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    output_dir: Option<String>,
    meeting_bundle_prefixes: Option<Vec<String>>,
    sample_rate: Option<i64>,
    bit_rate: Option<i64>,
    stop_grace_seconds: Option<i64>,
    poll_interval_ms: Option<i64>,
}

/// load reads `{home}/.config/mimi/config.toml`, falling back to the defaults when it is absent.
pub fn load(home: &Path) -> Result<Config, ConfigError> {
    let path = home.join(CONFIG_RELATIVE_PATH);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(ConfigError::Read {
                path,
                source: error,
            });
        }
    };
    from_toml(&contents, home)
}

fn from_toml(contents: &str, home: &Path) -> Result<Config, ConfigError> {
    let file = match toml::from_str::<ConfigFile>(contents) {
        Ok(file) => file,
        Err(error) => return Err(ConfigError::Parse(error)),
    };
    let ConfigFile {
        output_dir,
        meeting_bundle_prefixes,
        sample_rate,
        bit_rate,
        stop_grace_seconds,
        poll_interval_ms,
    } = file;

    let output_dir = match output_dir {
        Some(output_dir) => output_dir,
        None => DEFAULT_OUTPUT_DIR.to_owned(),
    };
    if output_dir.trim().is_empty() {
        return Err(ConfigError::EmptyOutputDir);
    }
    let output_dir = absolute(&output_dir, home);

    let meeting_bundle_prefixes = match meeting_bundle_prefixes {
        Some(prefixes) => {
            let mut wrapped = Vec::with_capacity(prefixes.len());
            for prefix in prefixes {
                if prefix.trim().is_empty() {
                    return Err(ConfigError::BlankBundlePrefix);
                }
                wrapped.push(BundlePrefix::new(prefix));
            }
            wrapped
        }
        None => default_bundle_prefixes(),
    };
    if meeting_bundle_prefixes.is_empty() {
        return Err(ConfigError::NoBundlePrefixes);
    }

    Ok(Config {
        output_dir,
        meeting_bundle_prefixes,
        sample_rate: positive("sample_rate", sample_rate, DEFAULT_SAMPLE_RATE)?,
        bit_rate: positive("bit_rate", bit_rate, DEFAULT_BIT_RATE)?,
        stop_grace_seconds: positive(
            "stop_grace_seconds",
            stop_grace_seconds,
            DEFAULT_STOP_GRACE_SECONDS,
        )?,
        poll_interval_ms: positive(
            "poll_interval_ms",
            poll_interval_ms,
            DEFAULT_POLL_INTERVAL_MS,
        )?,
    })
}

fn default_bundle_prefixes() -> Vec<BundlePrefix> {
    let mut prefixes = Vec::with_capacity(DEFAULT_BUNDLE_PREFIXES.len());
    for prefix in DEFAULT_BUNDLE_PREFIXES {
        prefixes.push(BundlePrefix::new(prefix));
    }
    prefixes
}

fn positive(field: &'static str, value: Option<i64>, default: u32) -> Result<u32, ConfigError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value <= 0 {
        return Err(ConfigError::NotPositive { field, value });
    }
    let Ok(value) = u32::try_from(value) else {
        return Err(ConfigError::TooLarge { field, value });
    };
    Ok(value)
}

fn absolute(value: &str, home: &Path) -> PathBuf {
    let value = value.trim();
    if value == "~" {
        return home.to_path_buf();
    }
    let value = match value.strip_prefix("~/") {
        Some(value) => value,
        None => value,
    };
    let value = Path::new(value);
    if value.is_absolute() {
        return value.to_path_buf();
    }
    home.join(value)
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Config {
            output_dir,
            meeting_bundle_prefixes,
            sample_rate,
            bit_rate,
            stop_grace_seconds,
            poll_interval_ms,
        } = self;
        writeln!(f, "output_dir = {:?}", output_dir.display().to_string())?;
        write!(f, "meeting_bundle_prefixes = [")?;
        let mut separator = "";
        for prefix in meeting_bundle_prefixes {
            write!(f, "{separator}{:?}", prefix.as_str())?;
            separator = ", ";
        }
        writeln!(f, "]")?;
        writeln!(f, "sample_rate = {sample_rate}")?;
        writeln!(f, "bit_rate = {bit_rate}")?;
        writeln!(f, "stop_grace_seconds = {stop_grace_seconds}")?;
        write!(f, "poll_interval_ms = {poll_interval_ms}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mimi-config-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp home");
            Self(path)
        }

        fn write_config(&self, contents: &str) {
            let Self(path) = self;
            let path = path.join(CONFIG_RELATIVE_PATH);
            fs::create_dir_all(path.parent().expect("config parent")).expect("create config dir");
            fs::write(path, contents).expect("write config");
        }

        fn path(&self) -> &Path {
            let Self(path) = self;
            path
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_dir_all(path);
        }
    }

    fn prefixes(values: &[&str]) -> Vec<BundlePrefix> {
        let mut wrapped = Vec::new();
        for value in values {
            wrapped.push(BundlePrefix::new(*value));
        }
        wrapped
    }

    #[test]
    fn missing_file_yields_defaults() {
        let home = TempHome::new();
        let config = load(home.path()).expect("defaults");
        let Config {
            output_dir,
            meeting_bundle_prefixes,
            sample_rate,
            bit_rate,
            stop_grace_seconds,
            poll_interval_ms,
        } = config;
        assert_eq!(output_dir, home.path().join("Recordings/mimi"));
        assert_eq!(
            meeting_bundle_prefixes,
            prefixes(&[
                "company.thebrowser.",
                "us.zoom.",
                "com.microsoft.teams2",
                "com.tinyspeck.slackmacgap",
                "com.google.Chrome",
            ])
        );
        assert_eq!(sample_rate, 24_000);
        assert_eq!(bit_rate, 96_000);
        assert_eq!(stop_grace_seconds, 15);
        assert_eq!(poll_interval_ms, 1_000);
    }

    #[test]
    fn full_file_overrides_every_field() {
        let home = TempHome::new();
        home.write_config(
            r#"
output_dir = "/tmp/mimi-recordings"
meeting_bundle_prefixes = ["com.example.one", "com.example.two"]
sample_rate = 48000
bit_rate = 128000
stop_grace_seconds = 30
poll_interval_ms = 500
"#,
        );
        let config = load(home.path()).expect("full file");
        assert_eq!(
            config,
            Config {
                output_dir: PathBuf::from("/tmp/mimi-recordings"),
                meeting_bundle_prefixes: prefixes(&["com.example.one", "com.example.two"]),
                sample_rate: 48_000,
                bit_rate: 128_000,
                stop_grace_seconds: 30,
                poll_interval_ms: 500,
            }
        );
    }

    #[test]
    fn partial_file_keeps_defaults_for_absent_fields() {
        let home = Path::new("/Users/tester");
        let config = from_toml("sample_rate = 16000\n", home).expect("partial file");
        assert_eq!(config.sample_rate, 16_000);
        assert_eq!(config.bit_rate, 96_000);
        assert_eq!(config.stop_grace_seconds, 15);
        assert_eq!(config.poll_interval_ms, 1_000);
        assert_eq!(config.output_dir, home.join("Recordings/mimi"));
        assert_eq!(config.meeting_bundle_prefixes, default_bundle_prefixes());
    }

    #[test]
    fn empty_file_yields_defaults() {
        let home = Path::new("/Users/tester");
        assert_eq!(
            from_toml("", home).expect("empty file"),
            from_toml("\n\n", home).expect("blank file")
        );
    }

    #[test]
    fn output_dir_is_stored_absolute() {
        let home = Path::new("/Users/tester");
        let config = from_toml("output_dir = \"~/Meetings\"\n", home).expect("tilde");
        assert_eq!(config.output_dir, PathBuf::from("/Users/tester/Meetings"));

        let config = from_toml("output_dir = \"Meetings/raw\"\n", home).expect("relative");
        assert_eq!(
            config.output_dir,
            PathBuf::from("/Users/tester/Meetings/raw")
        );

        let config = from_toml("output_dir = \"/data/meetings\"\n", home).expect("absolute");
        assert_eq!(config.output_dir, PathBuf::from("/data/meetings"));

        let config = from_toml("output_dir = \"~\"\n", home).expect("home itself");
        assert_eq!(config.output_dir, PathBuf::from("/Users/tester"));
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let home = Path::new("/Users/tester");
        let error = from_toml("sample_rate = ", home).expect_err("malformed");
        match error {
            ConfigError::Parse(_) => {}
            ConfigError::Read { .. }
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected a parse error, got {error}"),
        }
    }

    #[test]
    fn wrong_type_is_a_parse_error() {
        let home = Path::new("/Users/tester");
        let error = from_toml("sample_rate = \"48000\"\n", home).expect_err("wrong type");
        match error {
            ConfigError::Parse(_) => {}
            ConfigError::Read { .. }
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected a parse error, got {error}"),
        }
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        let home = Path::new("/Users/tester");
        let error = from_toml("sample_rat = 48000\n", home).expect_err("unknown key");
        match error {
            ConfigError::Parse(_) => {}
            ConfigError::Read { .. }
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected a parse error, got {error}"),
        }
    }

    #[test]
    fn non_positive_numbers_are_rejected() {
        let home = Path::new("/Users/tester");
        let cases = [
            ("sample_rate = 0\n", "sample_rate", 0),
            ("bit_rate = -1\n", "bit_rate", -1),
            ("stop_grace_seconds = 0\n", "stop_grace_seconds", 0),
            ("poll_interval_ms = -5\n", "poll_interval_ms", -5),
        ];
        for (contents, field, value) in cases {
            let error = from_toml(contents, home).expect_err(contents);
            match error {
                ConfigError::NotPositive {
                    field: actual,
                    value: reported,
                } => {
                    assert_eq!(actual, field);
                    assert_eq!(reported, value);
                }
                ConfigError::Read { .. }
                | ConfigError::Parse(_)
                | ConfigError::TooLarge { .. }
                | ConfigError::NoBundlePrefixes
                | ConfigError::BlankBundlePrefix
                | ConfigError::EmptyOutputDir => panic!("expected {field} to be rejected"),
            }
        }
    }

    #[test]
    fn oversized_numbers_are_rejected() {
        let home = Path::new("/Users/tester");
        let error = from_toml("sample_rate = 5000000000\n", home).expect_err("too large");
        match error {
            ConfigError::TooLarge { field, value } => {
                assert_eq!(field, "sample_rate");
                assert_eq!(value, 5_000_000_000);
            }
            ConfigError::Read { .. }
            | ConfigError::Parse(_)
            | ConfigError::NotPositive { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected an overflow error, got {error}"),
        }
    }

    #[test]
    fn empty_prefix_list_is_rejected() {
        let home = Path::new("/Users/tester");
        let error = from_toml("meeting_bundle_prefixes = []\n", home).expect_err("no prefixes");
        match error {
            ConfigError::NoBundlePrefixes => {}
            ConfigError::Read { .. }
            | ConfigError::Parse(_)
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected an empty-list error, got {error}"),
        }
    }

    #[test]
    fn a_blank_prefix_is_rejected_rather_than_matching_every_process() {
        let home = Path::new("/Users/tester");
        for contents in [
            "meeting_bundle_prefixes = [\"\"]\n",
            "meeting_bundle_prefixes = [\"us.zoom.\", \"  \"]\n",
        ] {
            let error = from_toml(contents, home).expect_err("a blank prefix matches everything");
            match error {
                ConfigError::BlankBundlePrefix => {}
                ConfigError::Read { .. }
                | ConfigError::Parse(_)
                | ConfigError::NotPositive { .. }
                | ConfigError::TooLarge { .. }
                | ConfigError::NoBundlePrefixes
                | ConfigError::EmptyOutputDir => {
                    panic!("expected a blank-prefix error, got {error}")
                }
            }
        }
    }

    #[test]
    fn empty_output_dir_is_rejected() {
        let home = Path::new("/Users/tester");
        let error = from_toml("output_dir = \"  \"\n", home).expect_err("empty output dir");
        match error {
            ConfigError::EmptyOutputDir => {}
            ConfigError::Read { .. }
            | ConfigError::Parse(_)
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix => panic!("expected an empty-dir error, got {error}"),
        }
    }

    #[test]
    fn unreadable_config_path_is_a_read_error() {
        let home = TempHome::new();
        let path = home.path().join(CONFIG_RELATIVE_PATH);
        fs::create_dir_all(&path).expect("create a directory where the file belongs");
        let error = load(home.path()).expect_err("read error");
        match error {
            ConfigError::Read { path: reported, .. } => assert_eq!(reported, path),
            ConfigError::Parse(_)
            | ConfigError::NotPositive { .. }
            | ConfigError::TooLarge { .. }
            | ConfigError::NoBundlePrefixes
            | ConfigError::BlankBundlePrefix
            | ConfigError::EmptyOutputDir => panic!("expected a read error, got {error}"),
        }
    }

    #[test]
    fn display_reports_every_field() {
        let home = Path::new("/Users/tester");
        let config = from_toml("meeting_bundle_prefixes = [\"com.example.one\"]\n", home)
            .expect("effective config");
        let printed = config.to_string();
        assert!(printed.contains("output_dir = \"/Users/tester/Recordings/mimi\""));
        assert!(printed.contains("meeting_bundle_prefixes = [\"com.example.one\"]"));
        assert!(printed.contains("sample_rate = 24000"));
        assert!(printed.contains("bit_rate = 96000"));
        assert!(printed.contains("stop_grace_seconds = 15"));
        assert!(printed.contains("poll_interval_ms = 1000"));
    }
}
