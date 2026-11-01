//! Manager config (TOML), as described in docs/ops-manager-contract.md.

use std::{io, path::PathBuf};

use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/mcsv_manager/config.toml";

/// `/admin/ops/website` and `/admin/ops/bot` are fixed pages on the website.
pub const RESERVED_IDS: [&str; 2] = ["website", "bot"];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default, rename = "service")]
    pub services: Vec<ServiceConfig>,
    pub backups: Option<BackupsConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Systemd,
    Docker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub id: String,
    pub name: String,
    pub backend: Backend,
    /// systemd only.
    pub unit: Option<String>,
    /// docker only.
    pub container: Option<String>,
    pub compose_file: Option<String>,
    pub compose_service: Option<String>,
    /// Defaults to all three.
    pub actions: Option<Vec<Action>>,
    /// Defaults to true for systemd and false for docker.
    pub console: Option<bool>,
}

impl ServiceConfig {
    pub fn actions(&self) -> Vec<Action> {
        self.actions
            .clone()
            .unwrap_or_else(|| vec![Action::Start, Action::Stop, Action::Restart])
    }

    pub fn console(&self) -> bool {
        self.console.unwrap_or(self.backend == Backend::Systemd)
    }

    pub fn updatable(&self) -> bool {
        self.compose_file.is_some() && self.compose_service.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupsConfig {
    /// `HH:MM`, host local time.
    pub nightly_at: Option<String>,
    /// Daily archives kept per target. `keep` is the deprecated name.
    #[serde(default = "default_keep_daily", alias = "keep")]
    pub keep_daily: u32,
    /// Weekly archives kept per target.
    #[serde(default = "default_keep_weekly")]
    pub keep_weekly: u32,
    pub world_dir: Option<PathBuf>,
    pub supabase_dump_cmd: Option<String>,
    /// Instance names (`minecraft@<name>`) whose worlds the `world` target covers.
    #[serde(default)]
    pub world_instances: Vec<String>,
}

fn default_keep_daily() -> u32 {
    7
}

fn default_keep_weekly() -> u32 {
    4
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'-'))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, io::Error> {
        let config: Config = toml::from_str(text).map_err(|e| invalid(format!("config: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// A missing file is an empty config; a broken one is an error.
    pub fn load(path: &str) -> Result<Self, io::Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn from_env() -> Result<Self, io::Error> {
        Self::load(&std::env::var("MCSV_CONFIG").unwrap_or_else(|_| DEFAULT_PATH.into()))
    }

    fn validate(&self) -> Result<(), io::Error> {
        for (i, s) in self.services.iter().enumerate() {
            if !valid_id(&s.id) {
                return Err(invalid(format!("config: bad service id {:?}", s.id)));
            }
            if RESERVED_IDS.contains(&s.id.as_str()) {
                return Err(invalid(format!(
                    "config: service id {:?} is reserved",
                    s.id
                )));
            }
            if self.services[..i].iter().any(|o| o.id == s.id) {
                return Err(invalid(format!("config: duplicate service id {:?}", s.id)));
            }
            let (needed, field) = match s.backend {
                Backend::Systemd => (&s.unit, "unit"),
                Backend::Docker => (&s.container, "container"),
            };
            if needed.is_none() {
                return Err(invalid(format!(
                    "config: service {:?} needs `{field}`",
                    s.id
                )));
            }
            if s.compose_file.is_some() != s.compose_service.is_some() {
                return Err(invalid(format!(
                    "config: service {:?} needs both compose_file and compose_service, or neither",
                    s.id
                )));
            }
            if s.updatable() && s.backend != Backend::Docker {
                return Err(invalid(format!(
                    "config: service {:?}: only docker services can be updatable",
                    s.id
                )));
            }
        }
        if let Some(b) = &self.backups {
            for (name, n) in [("keep_daily", b.keep_daily), ("keep_weekly", b.keep_weekly)] {
                if n == 0 {
                    return Err(invalid(format!(
                        "config: backups.{name} must be at least 1"
                    )));
                }
            }
            if let Some(t) = &b.nightly_at {
                let ok = t.split_once(':').is_some_and(|(h, m)| {
                    h.len() == 2
                        && m.len() == 2
                        && h.parse::<u8>().is_ok_and(|h| h < 24)
                        && m.parse::<u8>().is_ok_and(|m| m < 60)
                });
                if !ok {
                    return Err(invalid(format!(
                        "config: backups.nightly_at {t:?} is not HH:MM"
                    )));
                }
            }
            if let Some(bad) = b.world_instances.iter().find(|i| !valid_id(i)) {
                return Err(invalid(format!(
                    "config: bad backups.world_instances entry {bad:?}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT_EXAMPLE: &str = r#"
[[service]]
id = "survival"
name = "Survival"
backend = "systemd"
unit = "minecraft@survival.service"

[[service]]
id = "discord-bot"
name = "Discord bot"
backend = "docker"
container = "chulacraft-discord"
compose_file = "/srv/chulacraft/discord/compose.yaml"
compose_service = "bot"

[backups]
nightly_at = "04:00"
keep_daily = 7
keep_weekly = 4
world_dir = "/srv/minecraft/data"
supabase_dump_cmd = "pg_dump ..."
world_instances = ["survival"]
"#;

    fn err(text: &str) -> String {
        Config::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn parses_the_contract_example() {
        let c = Config::parse(CONTRACT_EXAMPLE).unwrap();
        assert_eq!(c.services.len(), 2);
        let (sys, dock) = (&c.services[0], &c.services[1]);
        assert!(sys.console() && !sys.updatable());
        assert!(!dock.console() && dock.updatable());
        assert_eq!(
            sys.actions(),
            vec![Action::Start, Action::Stop, Action::Restart]
        );
        let b = c.backups.unwrap();
        assert_eq!(
            (b.keep_daily, b.keep_weekly, b.nightly_at.as_deref()),
            (7, 4, Some("04:00"))
        );
        assert_eq!(b.world_instances, vec!["survival"]);
    }

    #[test]
    fn the_shipped_example_config_parses() {
        let c = Config::parse(include_str!("../config/mcsv_manager/config.example.toml")).unwrap();
        assert_eq!(c.services.len(), 2);
        assert_eq!(c.backups.unwrap().keep_daily, 7);
    }

    #[test]
    fn empty_and_missing_configs_have_no_services() {
        assert!(Config::parse("").unwrap().services.is_empty());
        assert!(
            Config::load("/nonexistent/mcsv.toml")
                .unwrap()
                .services
                .is_empty()
        );
    }

    #[test]
    fn explicit_actions_and_console_override_the_defaults() {
        let c = Config::parse(
            "[[service]]\nid='a'\nname='A'\nbackend='docker'\ncontainer='c'\nactions=['restart']\nconsole=true\n",
        )
        .unwrap();
        assert_eq!(c.services[0].actions(), vec![Action::Restart]);
        assert!(c.services[0].console());
    }

    #[test]
    fn reserved_bad_and_repeated_ids_are_refused() {
        for id in ["website", "bot"] {
            let text = format!("[[service]]\nid='{id}'\nname='x'\nbackend='systemd'\nunit='u'\n");
            assert!(err(&text).contains("reserved"), "{id}");
        }
        assert!(
            err("[[service]]\nid='a b'\nname='x'\nbackend='systemd'\nunit='u'\n")
                .contains("bad service id")
        );
        assert!(
            err("[[service]]\nid=''\nname='x'\nbackend='systemd'\nunit='u'\n")
                .contains("bad service id")
        );
        let twice = "[[service]]\nid='a'\nname='x'\nbackend='systemd'\nunit='u'\n".repeat(2);
        assert!(err(&twice).contains("duplicate"));
    }

    #[test]
    fn each_backend_needs_its_own_target() {
        assert!(err("[[service]]\nid='a'\nname='x'\nbackend='systemd'\n").contains("`unit`"));
        assert!(err("[[service]]\nid='a'\nname='x'\nbackend='docker'\n").contains("`container`"));
    }

    #[test]
    fn updatable_needs_both_compose_fields_and_docker() {
        assert!(err("[[service]]\nid='a'\nname='x'\nbackend='docker'\ncontainer='c'\ncompose_file='/f'\n").contains("both"));
        assert!(err("[[service]]\nid='a'\nname='x'\nbackend='systemd'\nunit='u'\ncompose_file='/f'\ncompose_service='s'\n").contains("only docker"));
    }

    #[test]
    fn backup_settings_are_checked() {
        let d = Config::parse("[backups]\n").unwrap().backups.unwrap();
        assert_eq!((d.keep_daily, d.keep_weekly), (7, 4));
        assert!(err("[backups]\nkeep_daily=0\n").contains("keep_daily"));
        assert!(err("[backups]\nkeep_weekly=0\n").contains("keep_weekly"));
        // deprecated alias for keep_daily; both at once is a duplicate field
        let old = Config::parse("[backups]\nkeep=3\n")
            .unwrap()
            .backups
            .unwrap();
        assert_eq!(old.keep_daily, 3);
        assert!(err("[backups]\nkeep=0\n").contains("keep"));
        assert!(err("[backups]\nkeep=3\nkeep_daily=3\n").contains("duplicate"));
        assert!(err("[backups]\nkeep_dayly=3\n").contains("keep_dayly"));
        for bad in ["4:00", "24:00", "04:60", "0400", "ab:cd"] {
            assert!(
                err(&format!("[backups]\nnightly_at='{bad}'\n")).contains("HH:MM"),
                "{bad}"
            );
        }
        assert!(err("[backups]\nworld_instances=['a b']\n").contains("world_instances"));
    }

    #[test]
    fn typos_and_unknown_backends_are_errors() {
        assert!(err("[[service]]\nid='a'\nname='x'\nbackend='k8s'\n").contains("config"));
        assert!(
            err("[[service]]\nid='a'\nname='x'\nbackend='systemd'\nunit='u'\nunti='u'\n")
                .contains("unti")
        );
    }
}
