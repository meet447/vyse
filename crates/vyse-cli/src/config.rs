use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vyse_core::HOSTED_DOMAIN;
use vyse_core::protocol::validate_subdomain;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ConfigFile {
    subdomain: Option<String>,
    machine_id: Option<String>,
    /// Unix timestamp of the last background update check.
    #[serde(default)]
    last_update_check: Option<i64>,
}

pub struct Config {
    path: PathBuf,
    data: ConfigFile,
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("dev", "Vyse", "vyse")
            .context("resolve Vyse config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let data = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read config at {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parse config at {}", path.display()))?
        } else {
            ConfigFile::default()
        };
        Ok(Self {
            path: path.to_path_buf(),
            data,
        })
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&self.path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(&self.data).context("serialize config")?;
        std::fs::write(path, raw).with_context(|| format!("write config to {}", path.display()))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub fn subdomain(&self) -> Option<&str> {
        self.data.subdomain.as_deref()
    }

    pub fn machine_id(&mut self) -> Result<String> {
        if let Some(id) = &self.data.machine_id {
            return Ok(id.clone());
        }
        let id = derive_machine_id();
        self.data.machine_id = Some(id.clone());
        self.save()?;
        Ok(id)
    }

    pub fn ensure_subdomain(&mut self, explicit: Option<String>) -> Result<String> {
        if let Some(subdomain) = explicit {
            let subdomain = subdomain.trim().to_ascii_lowercase();
            validate_subdomain(&subdomain).map_err(|err| anyhow::anyhow!(err))?;
            self.data.subdomain = Some(subdomain.clone());
            self.save()?;
            return Ok(subdomain);
        }

        if let Some(subdomain) = self.data.subdomain.clone() {
            return Ok(subdomain);
        }

        if !io::stdin().is_terminal() {
            bail!(
                "no subdomain configured; pass --subdomain or run `vyse serve` from a terminal to set one interactively"
            );
        }

        println!("Welcome to Vyse!");
        println!("Claim a public URL for your local server.");
        print!(
            "What subdomain would you like? (e.g. my-app → https://my-app.{HOSTED_DOMAIN}): "
        );
        io::stdout().flush()?;

        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let subdomain = line.trim().to_ascii_lowercase();
        validate_subdomain(&subdomain).map_err(|err| anyhow::anyhow!(err))?;
        self.data.subdomain = Some(subdomain.clone());
        self.save()?;
        Ok(subdomain)
    }

    const UPDATE_CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;

    pub fn should_check_updates(&self) -> bool {
        let Some(last) = self.data.last_update_check else {
            return true;
        };
        let now = unix_now();
        now.saturating_sub(last) >= Self::UPDATE_CHECK_INTERVAL_SECS
    }

    pub fn record_update_check(&mut self) -> Result<()> {
        self.data.last_update_check = Some(unix_now());
        self.save()
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn derive_machine_id() -> String {
    let raw = raw_hardware_id().unwrap_or_else(generate_random_raw);
    hash_id(&raw)
}

fn hash_id(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn generate_random_raw() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn raw_hardware_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return read_macos_platform_uuid();
    }
    #[cfg(target_os = "linux")]
    {
        return read_linux_machine_id();
    }
    #[cfg(target_os = "windows")]
    {
        return read_windows_machine_guid();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn read_macos_platform_uuid() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("IOPlatformUUID") {
            let parts: Vec<&str> = line.split('"').collect();
            if parts.len() >= 4 {
                let uuid = parts[3].trim();
                if !uuid.is_empty() {
                    return Some(uuid.to_string());
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_linux_machine_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn read_windows_machine_guid() -> Option<String> {
    let output = std::process::Command::new("REG")
        .args([
            "QUERY",
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("MachineGuid") {
            let guid = line
                .split_whitespace()
                .last()
                .filter(|value| !value.is_empty());
            if let Some(guid) = guid {
                return Some(guid.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = Config::load_from(&path).unwrap();
        assert_eq!(config.subdomain(), None);

        config.data.subdomain = Some("my-app".into());
        config.data.machine_id = Some("abc123".into());
        config.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.subdomain(), Some("my-app"));
        assert_eq!(loaded.data.machine_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn ensure_subdomain_explicit_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();
        config.data.subdomain = Some("old-name".into());
        config.save_to(&path).unwrap();

        let mut config = Config::load_from(&path).unwrap();
        let subdomain = config
            .ensure_subdomain(Some("new-name".into()))
            .unwrap();
        assert_eq!(subdomain, "new-name");
        assert_eq!(Config::load_from(&path).unwrap().subdomain(), Some("new-name"));
    }

    #[test]
    fn ensure_subdomain_omitted_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();
        config.data.subdomain = Some("saved-name".into());
        config.save_to(&path).unwrap();

        let mut config = Config::load_from(&path).unwrap();
        let subdomain = config.ensure_subdomain(None).unwrap();
        assert_eq!(subdomain, "saved-name");
        assert_eq!(Config::load_from(&path).unwrap().subdomain(), Some("saved-name"));
    }

    #[test]
    fn last_update_check_defaults_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config::load_from(&path).unwrap();
        assert!(config.should_check_updates());
        assert_eq!(config.data.last_update_check, None);

        let mut config = Config::load_from(&path).unwrap();
        config.record_update_check().unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert!(loaded.data.last_update_check.is_some());
        assert!(!loaded.should_check_updates());
    }

    #[test]
    fn ensure_subdomain_explicit_validates_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();

        let subdomain = config
            .ensure_subdomain(Some("My-App".into()))
            .unwrap();
        assert_eq!(subdomain, "my-app");

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.subdomain(), Some("my-app"));
    }

    #[test]
    fn ensure_subdomain_rejects_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();

        let err = config.ensure_subdomain(Some("-bad".into())).unwrap_err();
        assert!(err.to_string().contains("hyphen"));
    }

    #[test]
    fn ensure_subdomain_non_tty_without_config_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();

        // Stdin is not a TTY in unit tests.
        let err = config.ensure_subdomain(None).unwrap_err();
        assert!(err.to_string().contains("--subdomain"));
    }

    #[test]
    fn machine_id_persists_generated_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::load_from(&path).unwrap();

        let first = config.machine_id().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));

        let mut reloaded = Config::load_from(&path).unwrap();
        let second = reloaded.machine_id().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn hash_id_is_stable_and_lowercase() {
        let a = hash_id("test-hardware-id");
        let b = hash_id("test-hardware-id");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(!a.chars().any(|c| c.is_ascii_uppercase()));
    }
}
