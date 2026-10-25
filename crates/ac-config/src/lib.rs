//! Configuration schemas (spec §7), validation, and atomic persistence under
//! the XDG config/state directories.

mod config;
mod devices;
mod state;
mod validate;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub use config::*;
pub use devices::{DeviceRecord, Devices};
pub use state::{AppState, BroadcastState, WindowState};
pub use validate::{ValidationError, check_target_dir, validate};

const APP_DIR: &str = "android-controller";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("{path}: {}", errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Invalid { path: PathBuf, errors: Vec<ValidationError> },
    #[error("{path} was written by a newer version (schema {found}, supported {SCHEMA_VERSION})")]
    TooNew { path: PathBuf, found: u32 },
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub devices: PathBuf,
    pub state_dir: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    /// `$XDG_CONFIG_HOME/android-controller/*` and `$XDG_STATE_HOME/android-controller`.
    pub fn xdg() -> Option<Self> {
        let base = directories::BaseDirs::new()?;
        let config_dir = base.config_dir().join(APP_DIR);
        let state_dir = base
            .state_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| base.home_dir().join(".local/state"))
            .join(APP_DIR);
        Some(Self::in_dirs(&config_dir, &state_dir))
    }

    pub fn in_dirs(config_dir: &Path, state_dir: &Path) -> Self {
        Self {
            config: config_dir.join("config.toml"),
            devices: config_dir.join("devices.toml"),
            state_dir: state_dir.to_path_buf(),
            state: state_dir.join("state.toml"),
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io { path: path.into(), source }),
    }
}

fn check_schema(path: &Path, version: u32) -> Result<(), ConfigError> {
    if version > SCHEMA_VERSION {
        return Err(ConfigError::TooNew { path: path.into(), found: version });
    }
    Ok(())
}

pub fn parse_config(text: &str, path: &Path) -> Result<Config, ConfigError> {
    let cfg: Config =
        toml::from_str(text).map_err(|source| ConfigError::Parse { path: path.into(), source })?;
    check_schema(path, cfg.schema_version)?;
    let errors = validate(&cfg);
    if !errors.is_empty() {
        return Err(ConfigError::Invalid { path: path.into(), errors });
    }
    Ok(cfg)
}

/// Loads `config.toml`; a missing file yields the defaults.
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    match read_optional(path)? {
        Some(text) => parse_config(&text, path),
        None => Ok(Config::default()),
    }
}

pub fn load_devices(path: &Path) -> Result<Devices, ConfigError> {
    let Some(text) = read_optional(path)? else {
        return Ok(Devices::default());
    };
    let mut devices: Devices =
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.into(), source })?;
    check_schema(path, devices.schema_version)?;
    devices.normalize();
    Ok(devices)
}

/// Loads `state.toml`. It is only a convenience, so anything unreadable
/// yields the defaults (logged by the caller).
pub fn load_state(path: &Path) -> Result<AppState, ConfigError> {
    let Some(text) = read_optional(path)? else {
        return Ok(AppState::default());
    };
    let state: AppState = toml::from_str(&text).map_err(|source| ConfigError::Parse { path: path.into(), source })?;
    check_schema(path, state.schema_version)?;
    Ok(state)
}

pub fn save_state(path: &Path, state: &AppState) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(state).expect("AppState serializes");
    write_atomic(path, text.as_bytes())
}

pub fn save_config(path: &Path, cfg: &Config) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(cfg).expect("Config serializes");
    write_atomic(path, text.as_bytes())
}

pub fn save_devices(path: &Path, devices: &Devices) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(devices).expect("Devices serialize");
    write_atomic(path, text.as_bytes())
}

/// Writes `<path>.tmp`, fsyncs it, then renames over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let io = |source| ConfigError::Io { path: path.into(), source };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = fs::File::create(&tmp).map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    fs::rename(&tmp, path).map_err(io)
}
