//! Optical desired state and process ownership. Hardware transitions belong to the PLOAM owner.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;

pub const OWNER_LOCK: &str = "/var/run/fttr-optical.lock";

/// Held for the daemon lifetime; an offline shutdown must never race a live owner.
pub fn claim_owner() -> io::Result<File> {
    claim_owner_at(std::path::Path::new(OWNER_LOCK))
}

fn claim_owner_at(path: &std::path::Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "optical hardware is owned by fttrd; shutdown outcome unknown",
        ));
    }
    Ok(file)
}

const CONFIG: &str = "/etc/config/fttr";

pub fn settings() -> fttr_api::OpticalSettings {
    cache().read().unwrap().settings
}

pub fn fec_generation() -> u64 {
    cache().read().unwrap().generation
}

struct Configuration {
    settings: fttr_api::OpticalSettings,
    generation: u64,
}

pub fn fec_enabled() -> bool {
    let settings = settings();
    settings.enabled && settings.downstream_tx_enabled && settings.fec_enabled
}

fn cache() -> &'static std::sync::RwLock<Configuration> {
    static CACHE: std::sync::OnceLock<std::sync::RwLock<Configuration>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        std::sync::RwLock::new(Configuration {
            generation: 0,
            settings: fttr_api::OpticalSettings {
                enabled: false,
                downstream_tx_enabled: false,
                fec_enabled: false,
            },
        })
    })
}

pub fn refresh() -> io::Result<fttr_api::OpticalSettings> {
    let settings = load_settings_from(std::path::Path::new(CONFIG));
    let next = settings
        .as_ref()
        .copied()
        .unwrap_or(fttr_api::OpticalSettings {
            enabled: false,
            downstream_tx_enabled: false,
            fec_enabled: false,
        });
    let mut state = cache().write().unwrap();
    if state.settings != next {
        state.generation = state.generation.wrapping_add(1);
        state.settings = next;
    }
    settings
}

struct UciSession {
    dir: PathBuf,
    conf_dir: PathBuf,
    package: String,
    original: Vec<u8>,
    uci: openwrt_uci::Uci,
}
impl UciSession {
    fn new(config: &std::path::Path) -> io::Result<Self> {
        // A private snapshot with a unique package name cannot consume fttr's pending edits.
        let original = fs::read(config)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for nonce in 0..100 {
            let path =
                std::env::temp_dir().join(format!("fttr-optical-{}-{nonce}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let setup = (|| -> io::Result<Self> {
                        let conf_dir = path.join("config");
                        let delta_dir = path.join("delta");
                        fs::create_dir(&conf_dir)?;
                        fs::create_dir(&delta_dir)?;
                        let package = format!("optical_{}_{stamp}_{nonce}", std::process::id());
                        fs::write(conf_dir.join(&package), &original)?;
                        let mut uci = openwrt_uci::Uci::new()
                            .map_err(|e| io::Error::other(format!("Uci::new: {e}")))?;
                        uci.set_confdir(&conf_dir)
                            .map_err(|e| io::Error::other(format!("set_confdir: {e}")))?;
                        uci.set_savedir(&delta_dir)
                            .map_err(|e| io::Error::other(format!("set_savedir: {e}")))?;
                        Ok(Self {
                            dir: path.clone(),
                            conf_dir,
                            package,
                            original,
                            uci,
                        })
                    })();
                    match setup {
                        Ok(session) => return Ok(session),
                        Err(e) => {
                            let _ = fs::remove_dir_all(&path);
                            eprintln!("[-] UciSession setup failed at {path:?}: {e}");
                            return Err(e);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("cannot allocate optical UCI savedir"))
    }

    fn load(&mut self) -> io::Result<fttr_api::OpticalSettings> {
        let text = self
            .uci
            .export_package(&self.package)
            .map_err(|e| io::Error::other(e.to_string()))?;
        parse_settings(&text)
    }

    fn set_optical(&mut self, settings: fttr_api::OpticalSettings) -> io::Result<()> {
        self.uci
            .ensure_section(&self.package, "optical", "optical")
            .map_err(|e| io::Error::other(e.to_string()))?;
        for (key, value) in [
            ("enabled", settings.enabled),
            ("downstream_tx_enabled", settings.downstream_tx_enabled),
            ("fec_enabled", settings.fec_enabled),
        ] {
            self.uci
                .set(&self.package, "optical", key, if value { "1" } else { "0" })
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        self.uci
            .commit(&self.package)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(())
    }
}
impl Drop for UciSession {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn load_enabled() -> io::Result<bool> {
    refresh().map(|settings| settings.enabled)
}

pub fn load_enabled_from(config_path: &std::path::Path) -> io::Result<bool> {
    load_settings_from(config_path).map(|settings| settings.enabled)
}

pub fn load_settings_from(config_path: &std::path::Path) -> io::Result<fttr_api::OpticalSettings> {
    if !config_path.exists() {
        return parse_settings("");
    }
    UciSession::new(config_path)?.load()
}

pub fn save_enabled(enabled: bool) -> io::Result<()> {
    save_at(std::path::Path::new(CONFIG), enabled)
}

pub fn save(settings: fttr_api::OpticalSettings) -> io::Result<()> {
    save_settings_at(std::path::Path::new(CONFIG), Some(settings), None)
}

fn save_at(config: &std::path::Path, enabled: bool) -> io::Result<()> {
    save_settings_at(config, None, Some(enabled))
}

fn save_settings_at(
    config: &std::path::Path,
    settings: Option<fttr_api::OpticalSettings>,
    enabled: Option<bool>,
) -> io::Result<()> {
    let source = OpenOptions::new().read(true).write(true).open(config)?;
    if unsafe { libc::flock(source.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "FTTR configuration is being edited; retry",
        ));
    }
    let mut session = UciSession::new(config)?;
    let current = session.load()?;
    let mut settings = settings.unwrap_or(current);
    if let Some(enabled) = enabled {
        settings.enabled = enabled;
    }
    if current == settings {
        return Ok(());
    }
    session.set_optical(settings)?;
    if session.load()? != settings {
        return Err(io::Error::other("optical configuration readback mismatch"));
    }
    let bytes = fs::read(session.conf_dir.join(&session.package))?;
    replace_config(config, &source, &session.original, &bytes, &session.package)
}

fn replace_config(
    config: &std::path::Path,
    source: &File,
    original: &[u8],
    bytes: &[u8],
    token: &str,
) -> io::Result<()> {
    let staged = config.with_file_name(format!(".{token}.tmp"));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)?;
    let result = (|| {
        let meta = source.metadata()?;
        if unsafe { libc::fchown(output.as_raw_fd(), meta.uid(), meta.gid()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        output.set_permissions(meta.permissions())?;
        output.write_all(bytes)?;
        output.sync_all()?;
        if fs::read(config)? != original {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "FTTR configuration changed during optical save; retry",
            ));
        }
        fs::rename(&staged, config)?;
        File::open(
            config
                .parent()
                .ok_or_else(|| io::Error::other("configuration has no parent"))?,
        )?
        .sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(staged);
    result?;
    Ok(())
}

pub(crate) fn parse_export(text: &str) -> io::Result<bool> {
    parse_settings(text).map(|settings| settings.enabled)
}

pub(crate) fn parse_settings(text: &str) -> io::Result<fttr_api::OpticalSettings> {
    let mut optical = false;
    let mut provisioning = false;
    let mut seen = false;
    let mut settings = fttr_api::OpticalSettings {
        enabled: true,
        downstream_tx_enabled: true,
        fec_enabled: false,
    };
    for line in text.lines() {
        let words = crate::provisioning::uci_words(line).map_err(io::Error::other)?;
        match words.first().map(String::as_str) {
            Some("config") => {
                let kind = words.get(1).map(String::as_str);
                let name = words.get(2).map(String::as_str);
                optical = kind == Some("optical");
                provisioning = kind == Some("provisioning");
                if optical {
                    if seen || name != Some("optical") {
                        return Err(io::Error::other(
                            "expected one named config optical 'optical' section",
                        ));
                    }
                    seen = true;
                } else if name == Some("optical") {
                    return Err(io::Error::other("fttr.optical has the wrong section type"));
                }
            }
            Some("option" | "list")
                if provisioning
                    && words.get(1).is_some_and(|key| {
                        matches!(
                            key.as_str(),
                            "fec_enabled" | "downstream_tx_enabled" | "downstream_fec_enabled"
                        )
                    }) =>
            {
                return Err(io::Error::other(
                    "optical controls must be configured in fttr.optical, not provisioning",
                ));
            }
            Some("option" | "list") if optical => {
                let key = words.get(1).map(String::as_str).unwrap_or("");
                let value = match (words[0].as_str(), words.get(2).map(String::as_str)) {
                    ("option", Some("1")) => true,
                    ("option", Some("0")) => false,
                    _ => {
                        return Err(io::Error::other(format!(
                            "fttr.optical.{key} must be 0 or 1"
                        )))
                    }
                };
                match key {
                    "enabled" => settings.enabled = value,
                    "downstream_tx_enabled" => settings.downstream_tx_enabled = value,
                    "fec_enabled" => settings.fec_enabled = value,
                    _ => {
                        return Err(io::Error::other(format!(
                            "unsupported optical option {key}"
                        )))
                    }
                }
            }
            _ => (),
        }
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optical_defaults_and_strict_settings() {
        assert!(parse_export("package fttr\nconfig acs 'acs'\n option enabled '0'\n").unwrap());
        assert!(!parse_export("config optical 'optical'\n option enabled '0'\n").unwrap());
        assert!(parse_export("config optical 'optical'\n option enabled '1'\n").unwrap());
        for text in [
            "config optical 'other'",
            "config acs 'optical'",
            "config optical 'optical'\noption enabled 'yes'",
            "config optical 'optical'\nlist enabled '1'",
            "config optical 'optical'\nconfig optical 'optical'",
            "config optical 'optical'\noption downstream_fec_enabled '1'",
            "config provisioning 'provisioning'\noption fec_enabled '1'",
            "config provisioning 'provisioning'\noption downstream_tx_enabled '0'",
            "config provisioning 'provisioning'\noption downstream_fec_enabled '1'",
        ] {
            assert!(parse_export(text).is_err(), "{text}");
        }
    }

    #[test]
    fn grouped_optical_controls_keep_subordinate_intent_and_defaults() {
        let defaults = parse_settings("").unwrap();
        assert!(defaults.enabled && defaults.downstream_tx_enabled && !defaults.fec_enabled);
        for mask in 0..8 {
            let text = format!("config optical 'optical'\noption enabled '{}'\noption downstream_tx_enabled '{}'\noption fec_enabled '{}'\n", mask & 1, (mask >> 1) & 1, (mask >> 2) & 1);
            let settings = parse_settings(&text).unwrap();
            assert_eq!(settings.enabled, mask & 1 != 0);
            assert_eq!(settings.downstream_tx_enabled, mask & 2 != 0);
            assert_eq!(settings.fec_enabled, mask & 4 != 0);
        }
        for key in ["enabled", "downstream_tx_enabled", "fec_enabled"] {
            assert!(
                parse_settings(&format!("config optical 'optical'\noption {key} 'yes'\n")).is_err()
            );
        }
    }

    #[test]
    fn atomic_config_write_preserves_metadata_and_rejects_concurrent_edits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fttr-optical-atomic-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let config = dir.join("fttr");
        let original = b"config acs 'acs'\n option password 'unchanged'\n";
        fs::write(&config, original).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let source = File::open(&config).unwrap();
        let next = b"config acs 'acs'\n option password 'unchanged'\nconfig optical 'optical'\n option enabled '0'\n";
        replace_config(&config, &source, original, next, "optical-test").unwrap();
        assert_eq!(fs::read(&config).unwrap(), next);
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            replace_config(&config, &source, original, b"stale writer", "optical-test").is_err()
        );
        assert_eq!(fs::read(&config).unwrap(), next);
        assert!(!dir.join(".optical-test.tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn offline_shutdown_cannot_claim_live_owner_and_drop_releases_lock() {
        let path =
            std::env::temp_dir().join(format!("fttr-optical-lock-test-{}", std::process::id()));
        let owner = claim_owner_at(&path).unwrap();
        assert_eq!(
            claim_owner_at(&path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(owner);
        drop(claim_owner_at(&path).unwrap());
        fs::remove_file(path).unwrap();
    }
}
