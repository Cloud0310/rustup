use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use tracing::{error, warn};

use super::{
    install_bins,
    shell::{Posix, Shell, UnixShell},
};
use crate::{process::Process, utils};

// If the user is trying to install with sudo, on some systems this will
// result in writing root-owned files to the user's home directory, because
// sudo is configured not to change $HOME. Don't let that bogosity happen.
pub(crate) fn do_anti_sudo_check(
    no_prompt: bool,
    process: &Process,
) -> anyhow::Result<utils::ExitCode> {
    pub(crate) fn home_mismatch(process: &Process) -> (bool, PathBuf, PathBuf) {
        let fallback = || (false, PathBuf::new(), PathBuf::new());
        // test runner should set this, nothing else
        if process
            .var_os("RUSTUP_INIT_SKIP_SUDO_CHECK")
            .is_some_and(|s| s == "yes")
        {
            return fallback();
        }

        match (utils::home_dir_from_passwd(), process.var_os("HOME")) {
            (Some(pw), Some(eh)) if eh != pw => return (true, PathBuf::from(eh), pw),
            (None, _) => warn!("getpwuid_r: couldn't get user data"),
            _ => {}
        }
        fallback()
    }

    match home_mismatch(process) {
        (false, _, _) => {}
        (true, env_home, euid_home) => {
            error!("$HOME differs from euid-obtained home directory: you may be using sudo");
            error!("$HOME directory: {}", env_home.display());
            error!("euid-obtained home directory: {}", euid_home.display());
            if !no_prompt {
                error!("if this is what you want, restart the installation with `-y'");
                return Ok(utils::ExitCode(1));
            }
        }
    }

    Ok(utils::ExitCode(0))
}

/// Removes the first exact line matching `command` followed by a newline from each existing rcfile.
pub(crate) fn remove_path_setup_from_rcfiles(
    command: &str,
    rcfiles: &[PathBuf],
) -> anyhow::Result<()> {
    let command_bytes = format!("{command}\n").into_bytes();
    for rc in rcfiles.iter().filter(|rc| rc.is_file()) {
        let file = utils::read_file("rcfile", rc)?;
        let file_bytes = file.into_bytes();
        // FIXME: This is whitespace sensitive where it should not be.
        if let Some(idx) = find_exact_line(&file_bytes, &command_bytes) {
            // Here we rewrite the file without the offending line.
            let mut new_bytes = file_bytes[..idx].to_vec();
            new_bytes.extend(&file_bytes[idx + command_bytes.len()..]);
            let new_file = String::from_utf8(new_bytes).unwrap();
            utils::write_file("rcfile", rc, &new_file)?;
        }
    }
    Ok(())
}

pub(crate) fn add_path_setup_to_rcfiles(
    source_cmd: &str,
    rcfiles: &[PathBuf],
) -> anyhow::Result<()> {
    let source_cmd_with_newline = format!("\n{source_cmd}");

    for rc in rcfiles {
        let cmd_to_write = match utils::read_file("rcfile", rc) {
            Ok(contents) if contents.contains(source_cmd) => continue,
            Ok(contents) if !contents.ends_with('\n') => &source_cmd_with_newline,
            _ => source_cmd,
        };

        let rc_dir = rc.parent().with_context(|| {
            format!(
                "parent directory doesn't exist for rcfile path: `{}`",
                rc.display()
            )
        })?;
        utils::ensure_dir_exists("rcfile dir", rc_dir)?;
        utils::append_file("rcfile", rc, cmd_to_write)
            .with_context(|| format!("could not amend shell profile: '{}'", rc.display()))?;
    }

    Ok(())
}

pub(crate) fn do_write_env_files(
    cargo_home: &Path,
    home_dir: Option<&Path>,
    shells: &[Shell],
) -> anyhow::Result<()> {
    let mut written = vec![];

    for sh in shells {
        let script = sh.env_script();
        // Only write each possible script once.
        if !written.contains(&script) {
            sh.write_script(&script, cargo_home, home_dir)?;
            written.push(script);
        }
    }

    Ok(())
}

/// Tell the upgrader to replace the rustup bins, then delete
/// itself.
pub(crate) fn run_update(setup_path: &Path, _process: &Process) -> anyhow::Result<utils::ExitCode> {
    let status = Command::new(setup_path)
        .arg("--self-replace")
        .status()
        .context(format!("unable to run updater ({})", setup_path.display()))?;

    if !status.success() {
        bail!("self-updated failed to replace rustup executable");
    }

    Ok(utils::ExitCode(0))
}

/// This function is as the final step of a self-upgrade. It replaces
/// `$CARGO_HOME/bin/rustup` with the running exe, and updates the
/// links to it.
pub(crate) fn self_replace(process: &Process) -> anyhow::Result<utils::ExitCode> {
    install_bins(
        &process.cargo_home()?.join("bin"),
        super::force_hard_links(process),
    )?;

    Ok(utils::ExitCode(0))
}

fn remove_legacy_source_command(source_cmd: String, rcfiles: &[PathBuf]) -> anyhow::Result<()> {
    let cmd_bytes = source_cmd.into_bytes();
    for rc in rcfiles.iter().filter(|rc| rc.is_file()) {
        let file = utils::read_file("rcfile", rc)?;
        let file_bytes = file.into_bytes();
        // FIXME: This is whitespace sensitive where it should not be.
        if let Some(idx) = find_exact_line(&file_bytes, &cmd_bytes) {
            // Here we rewrite the file without the offending line.
            let mut new_bytes = file_bytes[..idx].to_vec();
            new_bytes.extend(&file_bytes[idx + cmd_bytes.len()..]);
            let new_file = String::from_utf8(new_bytes).unwrap();
            utils::write_file("rcfile", rc, &new_file)?;
        }
    }
    Ok(())
}

fn find_exact_line(file: &[u8], line: &[u8]) -> Option<usize> {
    // The trailing newline enforces the end boundary; check the start boundary here.
    assert!(line.ends_with(b"\n"));
    file.windows(line.len())
        .enumerate()
        .find_map(|(idx, candidate)| {
            (candidate == line && (idx == 0 || file[idx - 1] == b'\n')).then_some(idx)
        })
}

pub(crate) fn remove_legacy_paths(
    cargo_home: &Path,
    home_dir: Option<&Path>,
    rcfiles: &[PathBuf],
) -> anyhow::Result<()> {
    let cargo_home = Posix.cargo_home_str(cargo_home, home_dir)?;
    // Before the work to support more kinds of shells, which was released in
    // version 1.23.0 of Rustup, we always inserted this line instead, which is
    // now considered legacy
    remove_legacy_source_command(format!("export PATH=\"{cargo_home}/bin:$PATH\"\n"), rcfiles)?;
    // Unfortunately in 1.23, we accidentally used `source` rather than `.`
    // which, while widely supported, isn't actually POSIX, so we also
    // clean that up here.  This issue was filed as #2623.
    remove_legacy_source_command(format!("source \"{cargo_home}/env\"\n"), rcfiles)?;

    Ok(())
}
