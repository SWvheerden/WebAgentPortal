//! `config.toml` load/save plus the tiny bits of path handling it needs.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::state::PermissionMode;

pub const DEFAULT_PORT: u16 = 7717;
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// User-editable server configuration, persisted as `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    /// The address to listen on. Loopback by default; anything else is the
    /// opt-in remote case of §12 and needs a paired device key.
    ///
    /// Not editable through the Settings panel: a control-plane client that can
    /// widen its own listening address is a privilege escalation with extra
    /// steps, and it is precisely the move a client that had got hold of a
    /// credential would make.
    pub bind: String,
    /// Extra names the `Host` header may carry — a tailnet name, usually.
    /// Also not editable through Settings, and for the same reason.
    pub hostnames: Vec<String>,
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
            bind: DEFAULT_BIND.to_string(),
            hostnames: Vec::new(),
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

    /// Load without the deployment checks of §12.
    ///
    /// `claude-web pair` is the command that *makes* a non-loopback bind valid,
    /// so it cannot be gated on that bind already being valid — otherwise a
    /// hand-edited `bind` with no key file locks the operator out of the one
    /// command that fixes it.
    pub fn load_lenient(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&text)
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
        self.validate_with_key_at(&crate::remote::key_path())
    }

    /// [`Config::validate`], against a nominated key file — so the deployment
    /// rules of §12 can be tested without a real `~/.claude-web`.
    pub fn validate_with_key_at(&self, key_file: &Path) -> Result<()> {
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
        self.validate_bind(key_file)
    }

    /// The deployment shape of §12, enforced rather than documented.
    ///
    /// This binary terminates no TLS and ships no certificate machinery:
    /// encryption and device authentication come from WireGuard, in practice
    /// Tailscale, and the device key sits behind that. A self-signed
    /// certificate cannot authenticate the server, so its only durable effect
    /// is teaching you to click through a browser warning — which is exactly
    /// the reflex an interception attack needs. A public bind is therefore not
    /// a warning: it does not start.
    fn validate_bind(&self, key_file: &Path) -> Result<()> {
        let ip = self.bind_ip()?;
        if !bind_allowed(ip) {
            anyhow::bail!(
                "bind = \"{}\" would put the control plane on a public address, and this server \
                 terminates no TLS. Bind loopback, a private address (10/8, 172.16/12, \
                 192.168/16) or a tailnet address (100.64/10), and reach it over the VPN.",
                self.bind
            );
        }
        if !ip.is_loopback() && !key_file.exists() {
            anyhow::bail!(
                "bind = \"{}\" is not loopback, so a paired device key is required and there is \
                 none at {}. Run `claude-web pair` to make one, or set bind = \"127.0.0.1\".",
                self.bind,
                key_file.display()
            );
        }
        Ok(())
    }

    /// The address to listen on.
    pub fn bind_ip(&self) -> Result<IpAddr> {
        self.bind.trim().parse::<IpAddr>().map_err(|_| {
            anyhow::anyhow!(
                "bind = \"{}\" is not an IP address; it must be one this machine already has",
                self.bind
            )
        })
    }

    /// Repo roots with `~` expanded.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.repo_roots.iter().map(|r| expand_tilde(r)).collect()
    }
}

/// May the server listen here?
///
/// Loopback, the RFC1918 private ranges, and the carrier-grade NAT range
/// `100.64.0.0/10`, which is where tailnet addresses live. Everything else —
/// `0.0.0.0` included, since it covers whatever public address the machine
/// happens to have — is refused.
pub fn bind_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || is_cgnat(v4),
        // No IPv6 tailnet or private range is offered: the one shape this
        // supports is a v6 loopback bind, which is still local-only.
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// `100.64.0.0/10` — where a tailnet hands out addresses.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Resolve a caller-supplied path and require it to sit inside one of the
/// configured repo roots.
///
/// Every path that reaches git, a spawn, or a clone comes from the repo picker,
/// which only ever offers what is under a root. Nothing else is accepted:
/// without this, `path=~/Downloads/evil` reaches `git`, and a clone `root` can
/// write anywhere on disk. Both sides are canonicalised, so a symlink inside a
/// root cannot be used to step outside it.
pub fn confine_to_roots(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let resolved =
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    for root in roots {
        let Ok(root) = std::fs::canonicalize(root) else {
            continue;
        };
        if resolved == root || resolved.starts_with(&root) {
            return Ok(resolved);
        }
    }
    anyhow::bail!(
        "{} is outside the configured repo roots",
        resolved.display()
    )
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
    fn a_public_bind_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("remote-key");
        std::fs::write(&key, "0".repeat(64)).expect("write");
        for bind in [
            "8.8.8.8",
            "0.0.0.0",
            "203.0.113.4",
            "100.128.0.1",
            "169.254.1.1",
            "::",
            "not-an-ip",
            "example.com",
        ] {
            let cfg = Config {
                bind: bind.to_string(),
                ..Config::default()
            };
            assert!(
                cfg.validate_with_key_at(&key).is_err(),
                "{bind} must not be bindable"
            );
        }
        for bind in [
            "127.0.0.1",
            "::1",
            "10.0.0.4",
            "172.16.9.9",
            "172.31.0.1",
            "192.168.1.5",
            "100.64.0.7",
            "100.127.255.254",
        ] {
            let cfg = Config {
                bind: bind.to_string(),
                ..Config::default()
            };
            assert!(
                cfg.validate_with_key_at(&key).is_ok(),
                "{bind} must be allowed"
            );
        }
    }

    #[test]
    fn a_non_loopback_bind_will_not_start_without_a_paired_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("remote-key");
        let cfg = Config {
            bind: "100.64.0.7".to_string(),
            ..Config::default()
        };
        let err = cfg.validate_with_key_at(&key).expect_err("must be refused");
        let text = format!("{err:#}");
        assert!(text.contains("claude-web pair"), "{text}");

        std::fs::write(&key, "0".repeat(64)).expect("write");
        assert!(cfg.validate_with_key_at(&key).is_ok());

        // Loopback never needs one: the default config runs with no key at all.
        assert!(
            Config::default()
                .validate_with_key_at(&dir.path().join("nothing-here"))
                .is_ok()
        );
    }

    #[test]
    fn a_hand_edited_unsafe_config_fails_to_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "branch_prefix = \"--upload-pack=x\"\n").expect("write");
        assert!(Config::load_or_create(&path).is_err());
    }

    #[test]
    fn paths_outside_the_configured_roots_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("Code");
        let repo = root.join("thing");
        let outside = dir.path().join("Downloads").join("evil");
        std::fs::create_dir_all(&repo).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        let roots = vec![root.clone()];

        assert!(confine_to_roots(&repo, &roots).is_ok());
        assert!(
            confine_to_roots(&root, &roots).is_ok(),
            "a root itself is fine"
        );
        assert!(
            confine_to_roots(&repo.join("nested"), &roots).is_err(),
            "must exist"
        );
        let err = confine_to_roots(&outside, &roots).expect_err("must be refused");
        assert!(format!("{err:#}").contains("outside the configured repo roots"));

        // A sibling whose name merely starts with the root's is not inside it.
        let sibling = dir.path().join("Code-evil");
        std::fs::create_dir_all(&sibling).expect("mkdir");
        assert!(confine_to_roots(&sibling, &roots).is_err());

        // Neither is a symlink that points out of the root.
        #[cfg(unix)]
        {
            let link = root.join("escape");
            std::os::unix::fs::symlink(&outside, &link).expect("symlink");
            assert!(
                confine_to_roots(&link, &roots).is_err(),
                "a symlink out of the root must not smuggle a path back in"
            );
        }

        assert!(
            confine_to_roots(&repo, &[]).is_err(),
            "no roots means nothing is allowed"
        );
    }
}
