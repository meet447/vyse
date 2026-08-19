use std::path::Path;

use anyhow::{Context, Result, bail};
use self_update::update::ReleaseUpdate;
use vyse_core::{GITHUB_OWNER, GITHUB_REPO};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// True when the executable lives under Cargo's `target/debug` or `target/release`.
pub fn is_cargo_target_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/target/debug/")
        || s.contains("/target/release/")
        || s.contains("\\target\\debug\\")
        || s.contains("\\target\\release\\")
}

fn build_updater(show_progress: bool) -> Result<Box<dyn ReleaseUpdate>> {
    self_update::backends::github::Update::configure()
        .repo_owner(GITHUB_OWNER)
        .repo_name(GITHUB_REPO)
        .bin_name("vyse")
        .identifier("vyse")
        .current_version(CURRENT_VERSION)
        .no_confirm(true)
        .show_download_progress(show_progress)
        .show_output(show_progress)
        .build()
        .context("configure GitHub updater")
}

/// Returns `Some("vX.Y.Z")` when a newer release exists on GitHub.
pub fn check_update_available() -> Result<Option<String>> {
    let updater = build_updater(false)?;
    let release = updater.get_latest_release()?;
    if self_update::version::bump_is_greater(CURRENT_VERSION, &release.version)? {
        Ok(Some(format!("v{}", release.version)))
    } else {
        Ok(None)
    }
}

pub fn run_update(check_only: bool) -> Result<()> {
    if check_only {
        return print_check_status();
    }

    let exe = std::env::current_exe().context("resolve current executable path")?;
    if is_cargo_target_path(&exe) {
        bail!(
            "running from a Cargo build ({}) — install the release binary (~/.vyse/bin/vyse or on PATH) and run `vyse update` there",
            exe.display()
        );
    }

    let updater = build_updater(true)?;
    let status = updater
        .update()
        .context("download and install latest Vyse release")?;
    if status.uptodate() {
        println!("Vyse is up to date (v{CURRENT_VERSION}).");
    } else {
        println!("Updated Vyse to {}.", status.version());
    }
    Ok(())
}

fn print_check_status() -> Result<()> {
    match check_update_available()? {
        Some(version) => {
            println!("A new Vyse release is available ({version}).");
            println!("Current version: v{CURRENT_VERSION}");
            println!("Run: vyse update");
        }
        None => println!("Vyse is up to date (v{CURRENT_VERSION})."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_target_path_detects_debug_and_release_builds() {
        assert!(is_cargo_target_path(Path::new(
            "/home/user/project/target/debug/vyse"
        )));
        assert!(is_cargo_target_path(Path::new(
            "/home/user/project/target/release/vyse"
        )));
        assert!(is_cargo_target_path(Path::new(
            r"C:\proj\target\debug\vyse.exe"
        )));
    }

    #[test]
    fn cargo_target_path_allows_installed_binary() {
        assert!(!is_cargo_target_path(Path::new("/home/user/.vyse/bin/vyse")));
        assert!(!is_cargo_target_path(Path::new("/usr/local/bin/vyse")));
    }
}
