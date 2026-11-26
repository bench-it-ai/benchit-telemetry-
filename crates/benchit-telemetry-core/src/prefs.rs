//! Panel preferences shared by `benchit-panel` and the CLI `panel` command.
//!
//! Stored as `~/.config/benchit-telemetry/panel.toml` (or `$XDG_CONFIG_HOME/...`).
//! A leftover `~/.config/benchit-panel.json` (x/y/compact only) is migrated on first load.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Config directory name under XDG_CONFIG_HOME (or `~/.config`).
pub const CONFIG_DIR_NAME: &str = "benchit-telemetry";
/// File name inside the config directory.
pub const CONFIG_FILE_NAME: &str = "panel.toml";
/// Pre-toml window settings file (`~/.config/benchit-panel.json`).
pub const LEGACY_JSON_NAME: &str = "benchit-panel.json";

/// Display unit for temperatures that are stored internally in Celsius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TempUnit {
    /// Degrees Celsius (default).
    #[default]
    C,
    /// Degrees Fahrenheit.
    F,
}

impl TempUnit {
    /// Unit suffix used in the panel and the CLI text view.
    pub fn label(self) -> &'static str {
        match self {
            Self::C => "°C",
            Self::F => "°F",
        }
    }

    /// Convert a Celsius reading into this unit.
    pub fn from_celsius(self, c: f64) -> f64 {
        match self {
            Self::C => c,
            Self::F => c * 9.0 / 5.0 + 32.0,
        }
    }

    /// Format a Celsius reading (`"--"` when missing).
    pub fn format(self, c: Option<f64>, digits: usize) -> String {
        match c {
            None => "--".into(),
            Some(v) => format!("{:.*}", digits, self.from_celsius(v)),
        }
    }
}

fn default_interval() -> f64 {
    1.0
}
fn default_opacity() -> f64 {
    0.90
}
fn default_always_on_top() -> bool {
    true
}

/// Persisted desktop-panel settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelPrefs {
    pub x: Option<f32>,
    pub y: Option<f32>,
    #[serde(default)]
    pub compact: bool,
    #[serde(default)]
    pub temp_unit: TempUnit,
    /// Panel refresh period in seconds (clamped to 0.25..=30).
    #[serde(default = "default_interval")]
    pub update_interval_s: f64,
    /// Window background opacity (clamped to 0.15..=1.0).
    #[serde(default = "default_opacity")]
    pub opacity: f64,
    #[serde(default = "default_always_on_top")]
    pub always_on_top: bool,
    /// When true the panel process starts with the window hidden (logger is separate).
    #[serde(default)]
    pub hidden: bool,
}

impl Default for PanelPrefs {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            compact: false,
            temp_unit: TempUnit::C,
            update_interval_s: default_interval(),
            opacity: default_opacity(),
            always_on_top: true,
            hidden: false,
        }
    }
}

impl PanelPrefs {
    /// Clamp interval and opacity to the supported ranges.
    pub fn clamp(&mut self) {
        self.update_interval_s = self.update_interval_s.clamp(0.25, 30.0);
        self.opacity = self.opacity.clamp(0.15, 1.0);
    }

    /// `<config_home>/benchit-telemetry`.
    pub fn config_dir(xdg_config: Option<&Path>, home: Option<&Path>) -> PathBuf {
        match xdg_config {
            Some(x) if !x.as_os_str().is_empty() => x.join(CONFIG_DIR_NAME),
            _ => home.unwrap_or_else(|| Path::new(".")).join(".config").join(CONFIG_DIR_NAME),
        }
    }

    /// Path of `panel.toml` on this machine.
    pub fn path() -> PathBuf {
        Self::config_dir(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).as_deref(),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )
        .join(CONFIG_FILE_NAME)
    }

    /// Path of the pre-toml JSON file on this machine.
    pub fn legacy_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(LEGACY_JSON_NAME)
    }

    /// Load from the default locations, migrating the legacy JSON if needed.
    pub fn load() -> Self {
        Self::load_from(&Self::path(), &Self::legacy_path())
    }

    /// Load `toml_path`, or migrate `legacy_json` into it, or return defaults.
    pub fn load_from(toml_path: &Path, legacy_json: &Path) -> Self {
        if let Ok(s) = fs::read_to_string(toml_path) {
            if let Ok(mut p) = toml::from_str::<PanelPrefs>(&s) {
                p.clamp();
                return p;
            }
        }
        if let Ok(s) = fs::read_to_string(legacy_json) {
            if let Ok(legacy) = serde_json::from_str::<LegacyJson>(&s) {
                let mut p = PanelPrefs { x: legacy.x, y: legacy.y, compact: legacy.compact, ..Default::default() };
                p.clamp();
                let _ = p.save_to(toml_path);
                return p;
            }
        }
        PanelPrefs::default()
    }

    /// Write to the default path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Write `panel.toml` (0600) atomically.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut p = self.clone();
        p.clamp();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let body = toml::to_string_pretty(&p).context("serialize panel prefs")?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct LegacyJson {
    x: Option<f32>,
    y: Option<f32>,
    #[serde(default)]
    compact: bool,
}

/// File format for a history export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Csv,
    Json,
}

impl ExportKind {
    /// Filename extension without a dot.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

/// Rolling window offered in the panel export menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportRange {
    Hours(u32),
    Days(u32),
}

impl ExportRange {
    /// Menu / filename tag (`1h`, `24h`, `7d`, `30d`).
    pub fn tag(self) -> String {
        match self {
            Self::Hours(h) => format!("{h}h"),
            Self::Days(d) => format!("{d}d"),
        }
    }

    /// Length of the window in seconds.
    pub fn seconds(self) -> i64 {
        match self {
            Self::Hours(h) => i64::from(h) * 3600,
            Self::Days(d) => i64::from(d) * 86_400,
        }
    }

    /// The four ranges offered in the panel and tray menus.
    pub const MENU: [ExportRange; 4] =
        [ExportRange::Hours(1), ExportRange::Hours(24), ExportRange::Days(7), ExportRange::Days(30)];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fahrenheit_conversion() {
        assert_eq!(TempUnit::C.from_celsius(0.0), 0.0);
        assert_eq!(TempUnit::F.from_celsius(0.0), 32.0);
        assert_eq!(TempUnit::F.from_celsius(100.0), 212.0);
        assert_eq!(TempUnit::F.from_celsius(40.0), 104.0);
        assert_eq!(TempUnit::C.format(None, 0), "--");
        assert_eq!(TempUnit::F.format(Some(100.0), 0), "212");
        assert_eq!(TempUnit::C.label(), "°C");
        assert_eq!(TempUnit::F.label(), "°F");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let toml_path = d.path().join("panel.toml");
        let p = PanelPrefs {
            x: Some(12.5),
            y: Some(48.0),
            compact: true,
            temp_unit: TempUnit::F,
            update_interval_s: 5.0,
            opacity: 0.5,
            always_on_top: false,
            hidden: true,
        };
        p.save_to(&toml_path).unwrap();
        let mode = std::fs::metadata(&toml_path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
        let loaded = PanelPrefs::load_from(&toml_path, &d.path().join("missing.json"));
        assert_eq!(loaded, p);
    }

    #[test]
    fn migrates_legacy_json_and_clamps() {
        let d = tempfile::tempdir().unwrap();
        let json = d.path().join("benchit-panel.json");
        let toml_path = d.path().join("cfg").join("panel.toml");
        fs::write(&json, r#"{"x": 100.0, "y": 200.0, "compact": true}"#).unwrap();
        let p = PanelPrefs::load_from(&toml_path, &json);
        assert_eq!((p.x, p.y, p.compact), (Some(100.0), Some(200.0), true));
        assert_eq!(p.temp_unit, TempUnit::C);
        assert!(toml_path.is_file());
        let again = PanelPrefs::load_from(&toml_path, &json);
        assert_eq!(again, p);

        fs::write(&toml_path, "update_interval_s = 99\nopacity = 0.01\n").unwrap();
        let c = PanelPrefs::load_from(&toml_path, &json);
        assert_eq!(c.update_interval_s, 30.0);
        assert_eq!(c.opacity, 0.15);
    }

    #[test]
    fn missing_files_are_defaults() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(PanelPrefs::load_from(&d.path().join("no.toml"), &d.path().join("no.json")), PanelPrefs::default());
    }

    #[test]
    fn export_range_tags() {
        assert_eq!(ExportRange::Hours(1).tag(), "1h");
        assert_eq!(ExportRange::Hours(24).seconds(), 86_400);
        assert_eq!(ExportRange::Days(7).seconds(), 7 * 86_400);
        assert_eq!(ExportRange::Days(30).tag(), "30d");
    }
}
