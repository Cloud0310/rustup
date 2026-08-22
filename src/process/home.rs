//! Category-home resolution for callers that have enabled category mode.
//!
//! Rustup category homes resolve a non-empty category override, then a
//! non-empty Rustup override, then the platform default. The Cargo bin home
//! resolves non-empty `CARGO_BIN_HOME`, then non-empty `CARGO_HOME` plus
//! `bin`, then the platform default.
//!
//! Category overrides and `CARGO_BIN_HOME` remain verbatim, including when
//! relative. Relative `RUSTUP_HOME` and `CARGO_HOME` resolve under the current
//! directory.

use std::{io, path::PathBuf};

use home::env::Env;

mod platform_dir;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct HomeDirs {
    pub(crate) cache: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) state: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HomeCategory {
    Cache,
    Config,
    Data,
    State,
}

impl HomeCategory {
    const fn override_env_var(self) -> &'static str {
        match self {
            Self::Cache => "RUSTUP_CACHE_HOME",
            Self::Config => "RUSTUP_CONFIG_HOME",
            Self::Data => "RUSTUP_DATA_HOME",
            Self::State => "RUSTUP_STATE_HOME",
        }
    }

    #[cfg(unix)]
    const fn xdg_env_var(self) -> &'static str {
        match self {
            Self::Cache => "XDG_CACHE_HOME",
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::State => "XDG_STATE_HOME",
        }
    }

    #[cfg(unix)]
    const fn fallback_subdir(self) -> &'static str {
        match self {
            Self::Cache => ".cache",
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::State => ".local/state",
        }
    }
}

pub(super) fn category_home(category: HomeCategory, env: &impl Env) -> io::Result<PathBuf> {
    if let Some(path) = path_from_env(category.override_env_var(), env) {
        return Ok(path);
    }
    if let Some(path) = path_from_env("RUSTUP_HOME", env) {
        if path.is_absolute() {
            return Ok(path);
        }
        let mut cwd = env.current_dir()?;
        cwd.push(path);
        return Ok(cwd);
    }
    let mut path = platform_dir::category_home_with_env(category, env)?;
    path.push("rustup");
    Ok(path)
}

pub(crate) mod env {
    pub(crate) use home::env::{
        Env, OS_ENV, cargo_home_with_env, home_dir_with_env, rustup_home_with_env,
    };

    use super::path_from_env;
    use std::{io::Result, path::PathBuf};

    pub(crate) fn cargo_bin_with_env(env: &impl Env) -> Result<PathBuf> {
        if let Some(path) = path_from_env("CARGO_BIN_HOME", env) {
            return Ok(path);
        }
        if let Some(path) = path_from_env("CARGO_HOME", env) {
            let mut path = if path.is_absolute() {
                path
            } else {
                env.current_dir()?.join(path)
            };
            path.push("bin");
            return Ok(path);
        }
        super::platform_dir::bin_home_with_env(env)
    }
}

fn path_from_env(key: &str, env: &impl Env) -> Option<PathBuf> {
    env.var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
