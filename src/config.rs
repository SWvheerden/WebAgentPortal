//! `config.toml` load/save plus the tiny bits of path handling it needs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::state::PermissionMode;

pub const DEFAULT_PORT: u16 = 7717;

/// User-editable server configuration, persisted as `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub open_browser: bool,
    pub repo_roots: Vec<String>,
    pub branch_prefix: String,
    pub max_agents: usize,
    pub default_model: String,
    pub default_permission_mode: PermissionMode,
    pub claude_bin: String,
    pub pinned_cli_version: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            open_browser: true,
            repo_roots: vec!["~/Code".to_string()],
            branch_prefix: "sw_".to_string(),
            max_agents: 8,
            default_model: "opus".to_string(),
            default_permission_mode: PermissionMode::Ask,
            claude_bin: "claude".to_string(),
            pinned_cli_version: "2.1.241".to_string(),
        }
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).context("parsing config.toml")
    }

    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising config.toml")
    }

    /// Load the config at `path`, writing out a default file if none exists.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let cfg = Self::from_toml_str(&text)?;
            cfg.validate()
                .with_context(|| format!("in {}", path.display()))?;
            Ok(cfg)
        } else {
            let cfg = Self::default();
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, self.to_toml_string()?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Reject a configuration that would be unsafe or unusable.
    ///
    /// The branch prefix ends up as a positional argument to git, so it is held
    /// to the same rules as a ref name.
    pub fn validate(&self) -> Result<()> {
        crate::repo::git::validate_branch_prefix(&self.branch_prefix)?;
        if self.repo_roots.is_empty() {
            anyhow::bail!("at least one repo root is required");
        }
        if self.max_agents == 0 {
            anyhow::bail!("max_agents must be at least 1");
        }
        if self.claude_bin.trim().is_empty() {
            anyhow::bail!("claude_bin cannot be empty");
        }
        Ok(())
    }

    /// Repo roots with `~` expanded.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.repo_roots.iter().map(|r| expand_tilde(r)).collect()
    }
}

/// Expand a leading `~` using `$HOME`. Everything else is passed through.
pub fn expand_tilde(raw: &str) -> PathBuf {
    expand_tilde_with(raw, home_dir())
}

/// Pure core of [`expand_tilde`], so it can be tested without touching the
/// process environment.
fn expand_tilde_with(raw: &str, home: Option<PathBuf>) -> PathBuf {
    match home {
        Some(home) if raw == "~" => home,
        Some(home) if raw.starts_with("~/") => home.join(&raw[2..]),
        _ => PathBuf::from(raw),
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/.claude-web` — where the config file and the database live.
pub fn state_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude-web")
}

pub fn default_config_path() -> PathBuf {
    state_dir().join("config.toml")
}

pub fn default_db_path() -> PathBuf {
    state_dir().join("agents.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml_string().expect("serialise");
        let back = Config::from_toml_str(&text).expect("parse");
        assert_eq!(cfg, back);
    }

    #[test]
    fn spec_example_parses() {
        let text = r#"
port            = 7717
open_browser    = true
repo_roots      = ["~/Code"]
branch_prefix   = "sw_"
max_agents      = 8
default_model   = "opus"
default_permission_mode = "ask"
claude_bin      = "claude"
pinned_cli_version = "2.1.241"
"#;
        let cfg = Config::from_toml_str(text).expect("parse");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg = Config::from_toml_str("port = 9000\n").expect("parse");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.branch_prefix, "sw_");
        assert_eq!(cfg.default_permission_mode, PermissionMode::Ask);
    }

    #[test]
    fn permission_mode_serialises_as_documented() {
        let text = Config {
            default_permission_mode: PermissionMode::AcceptEdits,
            ..Config::default()
        }
        .to_toml_string()
        .expect("serialise");
        assert!(
            text.contains("default_permission_mode = \"acceptEdits\""),
            "{text}"
        );
    }

    #[test]
    fn load_or_create_writes_then_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let created = Config::load_or_create(&path).expect("create");
        assert!(path.exists());
        let loaded = Config::load_or_create(&path).expect("load");
        assert_eq!(created, loaded);
    }

    #[test]
    fn tilde_expands_against_home() {
        let home = Some(PathBuf::from("/home/tester"));
        assert_eq!(
            expand_tilde_with("~/Code", home.clone()),
            PathBuf::from("/home/tester/Code")
        );
        assert_eq!(
            expand_tilde_with("~", home.clone()),
            PathBuf::from("/home/tester")
        );
        assert_eq!(
            expand_tilde_with("/abs/path", home.clone()),
            PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_tilde_with("~notuser", home),
            PathBuf::from("~notuser")
        );
        assert_eq!(expand_tilde_with("~/Code", None), PathBuf::from("~/Code"));
    }

    #[test]
    fn an_option_shaped_branch_prefix_is_refused() {
        let cfg = Config {
            branch_prefix: "--force".to_string(),
            ..Config::default()
        };
        let err = cfg.validate().expect_err("must be refused");
        assert!(
            format!("{err:#}").contains("would read as an option"),
            "{err:#}"
        );
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn an_unusable_config_is_refused() {
        assert!(
            Config {
                repo_roots: vec![],
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                max_agents: 0,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                claude_bin: "  ".to_string(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_hand_edited_unsafe_config_fails_to_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "branch_prefix = \"--upload-pack=x\"\n").expect("write");
        assert!(Config::load_or_create(&path).is_err());
    }
}
