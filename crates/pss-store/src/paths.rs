//! Where the app keeps its local files, following each OS's convention:
//!
//! | what | Linux | macOS | Windows |
//! |---|---|---|---|
//! | data (battles DB, crew plan, `ca/`) | `~/.local/share/pss-battlestats` | `~/Library/Application Support/pss-battlestats` | `%LOCALAPPDATA%\pss-battlestats\data` |
//! | config | `~/.config/pss-battlestats` | `~/Library/Application Support/pss-battlestats` | `%APPDATA%\pss-battlestats\config` |
//! | cache (designs DB, re-downloadable) | `~/.cache/pss-battlestats` | `~/Library/Caches/pss-battlestats` | `%LOCALAPPDATA%\pss-battlestats\cache` |
//!
//! Setting `PSS_BATTLESTATS_DIR` puts everything in that one folder instead (portable install,
//! backups, tests).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use directories::ProjectDirs;

const APP_NAME: &str = "pss-battlestats";
pub const DIR_ENV: &str = "PSS_BATTLESTATS_DIR";

const BATTLES_DB: &str = "pss-battlestats.db";
const DESIGNS_DB: &str = "pss-designs-cache.db";
const CONFIG_FILE: &str = "pss-gui-config.json";
const CREW_PLAN: &str = "pss-crew-plan.json";
const APPEARANCE_FILE: &str = "appearance.toml";
#[cfg(windows)]
const CA_CERT: &str = "pss-capture-ca.crt";
#[cfg(not(windows))]
const CA_CERT: &str = "pss-capture-ca.pem";
const CA_KEY: &str = "pss-capture-ca-key.pem";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl AppPaths {
    /// Everything in one folder.
    pub fn in_single_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            config: dir.clone(),
            data: dir.clone(),
            cache: dir,
        }
    }

    pub fn discover() -> Self {
        if let Some(dir) = std::env::var_os(DIR_ENV).filter(|v| !v.is_empty()) {
            return Self::in_single_dir(dir);
        }

        match ProjectDirs::from("", "", APP_NAME) {
            Some(dirs) => Self {
                config: dirs.config_dir().to_path_buf(),
                data: dirs.data_local_dir().to_path_buf(),
                cache: dirs.cache_dir().to_path_buf(),
            },
            // No home directory could be determined: fall back to the working directory
            None => Self::in_single_dir("."),
        }
    }

    pub fn battles_db(&self) -> PathBuf {
        self.data.join(BATTLES_DB)
    }

    pub fn crew_plan(&self) -> PathBuf {
        self.data.join(CREW_PLAN)
    }

    pub fn ca_dir(&self) -> PathBuf {
        self.data.join("ca")
    }

    pub fn default_ca_cert(&self) -> PathBuf {
        self.ca_dir().join(CA_CERT)
    }

    pub fn default_ca_key(&self) -> PathBuf {
        self.ca_dir().join(CA_KEY)
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join(CONFIG_FILE)
    }

    pub fn appearance_file(&self) -> PathBuf {
        self.config.join(APPEARANCE_FILE)
    }

    pub fn designs_db(&self) -> PathBuf {
        self.cache.join(DESIGNS_DB)
    }

    pub fn ensure_dirs(&self) -> io::Result<()> {
        let ca = self.ca_dir();

        for dir in [&self.config, &self.data, &self.cache, &ca] {
            std::fs::create_dir_all(dir)?;
        }

        Ok(())
    }

    /// Copies the files an older version left in `legacy_dir` (the working directory) to their new
    /// place. Never overwrites a file that already exists at the destination and never deletes the
    /// originals. Returns the (from, to) pairs actually copied.
    pub fn migrate_legacy(&self, legacy_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
        let moves = [
            (BATTLES_DB, self.battles_db()),
            (DESIGNS_DB, self.designs_db()),
            (CONFIG_FILE, self.config_file()),
            (CREW_PLAN, self.crew_plan()),
            (CA_CERT, self.default_ca_cert()),
            (CA_KEY, self.default_ca_key()),
        ];
        let mut copied = Vec::new();

        for (name, to) in moves {
            let from = legacy_dir.join(name);
            if !from.is_file() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::copy(&from, &to).is_ok() {
                copied.push((from, to));
            }
        }

        copied
    }
}

/// The app's paths, resolved once
pub fn app_paths() -> &'static AppPaths {
    static PATHS: OnceLock<AppPaths> = OnceLock::new();
    PATHS.get_or_init(AppPaths::discover)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("pss-paths-{tag}-{}-{nanos}", std::process::id()));

        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn single_dir_mode_puts_every_file_in_that_folder() {
        let root = Path::new("some-root");
        let p = AppPaths::in_single_dir(root);

        assert_eq!(p.battles_db(), root.join("pss-battlestats.db"));
        assert_eq!(p.designs_db(), root.join("pss-designs-cache.db"));
        assert_eq!(p.config_file(), root.join("pss-gui-config.json"));
        assert_eq!(p.crew_plan(), root.join("pss-crew-plan.json"));
        assert_eq!(p.default_ca_cert(), root.join("ca").join(CA_CERT));
        assert_eq!(
            p.default_ca_key(),
            root.join("ca").join("pss-capture-ca-key.pem")
        );
    }

    #[test]
    fn migration_copies_legacy_files_and_never_overwrites_or_deletes() {
        let legacy = temp_dir("legacy");
        let target = temp_dir("target");
        std::fs::write(legacy.join("pss-battlestats.db"), b"old battles").unwrap();
        std::fs::write(legacy.join("pss-crew-plan.json"), b"old plan").unwrap();

        let paths = AppPaths::in_single_dir(&target);
        paths.ensure_dirs().unwrap();
        std::fs::write(paths.crew_plan(), b"new plan").unwrap(); // already there : must survive

        let copied = paths.migrate_legacy(&legacy);
        assert_eq!(copied.len(), 1);
        assert_eq!(std::fs::read(paths.battles_db()).unwrap(), b"old battles");
        assert_eq!(std::fs::read(paths.crew_plan()).unwrap(), b"new plan");
        assert!(legacy.join("pss-battlestats.db").exists()); // originals are kept

        let _ = std::fs::remove_dir_all(&legacy);
        let _ = std::fs::remove_dir_all(&target);
    }
}
