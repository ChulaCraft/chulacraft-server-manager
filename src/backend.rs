//! The seam between the HTTP layer and the host. Handlers only see
//! `ServiceBackend`; production uses `SystemdBackend`, tests and the
//! `fake-backend` feature use `FakeBackend`.

use std::{fmt::Debug, io, sync::Arc};

use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::{
    handlers::AppError,
    mcsv_mgr::{JournalBroadcaster, LogLine, McsvManager},
    systemd1::{Process, Systemd1, UnitStatus},
};

/// Units the manager may start: `minecraft@<name>.service` and
/// `chulacraft-update@<name>.service`, with `<name>` of `[A-Za-z0-9_-]+`. Keep in
/// step with `config/polkit-1/rules.d/mcsv-manager.rules`, which enforces the same
/// pattern on the host.
pub fn allowed_unit(unit: &str) -> bool {
    ["minecraft@", "chulacraft-update@"].iter().any(|prefix| {
        unit.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(".service"))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
    })
}

fn refuse_unit(unit: &str) -> AppError {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unit not allowed: {unit:?}"),
    )
    .into()
}

/// Live and historical log lines of one service.
pub trait LogSource: Send + Sync + Debug {
    fn name(&self) -> &str;
    fn subscribe(&self) -> broadcast::Receiver<LogLine>;
    fn get_logs(
        &self,
        since: u64,
        until: Option<u64>,
        max_lines: Option<u64>,
    ) -> Result<Vec<LogLine>, io::Error>;
    fn get_rlogs(
        &self,
        since: Option<u64>,
        until: u64,
        max_lines: Option<u64>,
    ) -> Result<Vec<LogLine>, io::Error>;
    fn get_logs_to(
        &self,
        since: u64,
        until: Option<u64>,
        max_lines: Option<u64>,
        tx: mpsc::Sender<LogLine>,
    ) -> Result<(), io::Error>;
    fn get_rlogs_to(
        &self,
        since: Option<u64>,
        until: u64,
        max_lines: Option<u64>,
        tx: mpsc::Sender<LogLine>,
    ) -> Result<(), io::Error>;
}

#[async_trait]
pub trait ServiceBackend: Send + Sync + Debug {
    /// Instance names of the known Minecraft servers.
    async fn server_names(&self) -> Vec<String>;
    async fn unit_status(&self, name: &str) -> Result<Option<UnitStatus>, AppError>;
    async fn server_process(&self, name: &str) -> Result<Option<Process>, AppError>;
    async fn start_server(&self, name: &str) -> Result<(), AppError>;
    async fn stop_server(&self, name: &str) -> Result<(), AppError>;
    async fn restart_server(&self, name: &str) -> Result<(), AppError>;
    /// Starts any unit by its full name (for example `chulacraft-update@bot.service`).
    async fn start_unit(&self, unit: &str) -> Result<(), AppError>;
    /// True while the unit is active or activating.
    async fn unit_active(&self, unit: &str) -> Result<bool, AppError>;
    async fn inject_command(&self, name: &str, cmd: &str) -> Result<(), AppError>;
    async fn log_source(&self, name: &str) -> Option<Arc<dyn LogSource>>;
    /// SIGHUP: rediscover instances.
    async fn reload(&self) -> Result<(), AppError>;
}

impl LogSource for JournalBroadcaster {
    fn name(&self) -> &str {
        &self.name
    }

    fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    fn get_logs(
        &self,
        since: u64,
        until: Option<u64>,
        max_lines: Option<u64>,
    ) -> Result<Vec<LogLine>, io::Error> {
        JournalBroadcaster::get_logs(self, since, until, max_lines)
    }

    fn get_rlogs(
        &self,
        since: Option<u64>,
        until: u64,
        max_lines: Option<u64>,
    ) -> Result<Vec<LogLine>, io::Error> {
        JournalBroadcaster::get_rlogs(self, since, until, max_lines)
    }

    fn get_logs_to(
        &self,
        since: u64,
        until: Option<u64>,
        max_lines: Option<u64>,
        tx: mpsc::Sender<LogLine>,
    ) -> Result<(), io::Error> {
        JournalBroadcaster::get_logs_to(self, since, until, max_lines, tx)
    }

    fn get_rlogs_to(
        &self,
        since: Option<u64>,
        until: u64,
        max_lines: Option<u64>,
        tx: mpsc::Sender<LogLine>,
    ) -> Result<(), io::Error> {
        JournalBroadcaster::get_rlogs_to(self, since, until, max_lines, tx)
    }
}

/// Production adapter over systemd (D-Bus) and the journal.
#[derive(Debug)]
pub struct SystemdBackend {
    dbus: Arc<Systemd1>,
    mgr: Mutex<McsvManager>,
}

impl SystemdBackend {
    pub async fn new() -> Result<Self, zbus::Error> {
        let dbus = Arc::new(Systemd1::new().await?);
        let mgr = Mutex::new(McsvManager::new(dbus.clone()));
        Ok(Self { dbus, mgr })
    }
}

#[async_trait]
impl ServiceBackend for SystemdBackend {
    async fn server_names(&self) -> Vec<String> {
        self.mgr.lock().await.server_names.clone()
    }

    async fn unit_status(&self, name: &str) -> Result<Option<UnitStatus>, AppError> {
        Ok(self.mgr.lock().await.get_server_unit_status(name).await?)
    }

    async fn server_process(&self, name: &str) -> Result<Option<Process>, AppError> {
        Ok(self.mgr.lock().await.get_server_process(name).await?)
    }

    async fn start_server(&self, name: &str) -> Result<(), AppError> {
        Ok(self.mgr.lock().await.start_server(name).await?)
    }

    async fn stop_server(&self, name: &str) -> Result<(), AppError> {
        Ok(self.mgr.lock().await.stop_server(name).await?)
    }

    async fn restart_server(&self, name: &str) -> Result<(), AppError> {
        Ok(self.mgr.lock().await.restart_server(name).await?)
    }

    async fn start_unit(&self, unit: &str) -> Result<(), AppError> {
        if !allowed_unit(unit) {
            return Err(refuse_unit(unit));
        }
        self.dbus.start_unit(unit, "replace").await?;
        Ok(())
    }

    async fn unit_active(&self, unit: &str) -> Result<bool, AppError> {
        let units = self.dbus.list_units_by_names(&vec![unit]).await?;
        Ok(units
            .first()
            .is_some_and(|u| matches!(u.active_state.as_str(), "active" | "activating")))
    }

    async fn inject_command(&self, name: &str, cmd: &str) -> Result<(), AppError> {
        Ok(self.mgr.lock().await.inject_command(name, cmd).await?)
    }

    async fn log_source(&self, name: &str) -> Option<Arc<dyn LogSource>> {
        let jb = self.mgr.lock().await.log_broadcasters.get(name)?.clone();
        Some(jb)
    }

    async fn reload(&self) -> Result<(), AppError> {
        Ok(self.mgr.lock().await.update().await?)
    }
}

#[cfg(any(test, feature = "fake-backend"))]
pub use fake::{FakeBackend, FakeLog};

#[cfg(any(test, feature = "fake-backend"))]
mod fake {
    use std::{collections::HashSet, sync::Mutex as StdMutex};

    use super::*;

    /// A log source with canned history and a sender for live lines.
    #[derive(Debug)]
    pub struct FakeLog {
        name: String,
        pub tx: broadcast::Sender<LogLine>,
        history: Vec<LogLine>,
    }

    fn line(message: &str, timestamp: u64) -> LogLine {
        LogLine {
            message: message.into(),
            priority: "6".into(),
            timestamp,
            command: "java".into(),
            pid: 1,
        }
    }

    impl LogSource for FakeLog {
        fn name(&self) -> &str {
            &self.name
        }

        fn subscribe(&self) -> broadcast::Receiver<LogLine> {
            self.tx.subscribe()
        }

        fn get_logs(
            &self,
            since: u64,
            until: Option<u64>,
            max_lines: Option<u64>,
        ) -> Result<Vec<LogLine>, io::Error> {
            Ok(self
                .history
                .iter()
                .filter(|l| l.timestamp >= since && until.is_none_or(|u| l.timestamp <= u))
                .take(max_lines.map_or(usize::MAX, |m| m as usize))
                .cloned()
                .collect())
        }

        fn get_rlogs(
            &self,
            since: Option<u64>,
            until: u64,
            max_lines: Option<u64>,
        ) -> Result<Vec<LogLine>, io::Error> {
            Ok(self
                .history
                .iter()
                .rev()
                .filter(|l| l.timestamp <= until && since.is_none_or(|s| l.timestamp >= s))
                .take(max_lines.map_or(usize::MAX, |m| m as usize))
                .cloned()
                .collect())
        }

        fn get_logs_to(
            &self,
            since: u64,
            until: Option<u64>,
            max_lines: Option<u64>,
            tx: mpsc::Sender<LogLine>,
        ) -> Result<(), io::Error> {
            for l in self.get_logs(since, until, max_lines)? {
                if tx.blocking_send(l).is_err() {
                    break;
                }
            }
            Ok(())
        }

        fn get_rlogs_to(
            &self,
            since: Option<u64>,
            until: u64,
            max_lines: Option<u64>,
            tx: mpsc::Sender<LogLine>,
        ) -> Result<(), io::Error> {
            for l in self.get_rlogs(since, until, max_lines)? {
                if tx.blocking_send(l).is_err() {
                    break;
                }
            }
            Ok(())
        }
    }

    /// In-memory stand-in for the host. Records every action so tests can assert on it.
    #[derive(Debug)]
    pub struct FakeBackend {
        names: Vec<String>,
        logs: Vec<std::sync::Arc<FakeLog>>,
        active_units: StdMutex<HashSet<String>>,
        /// `"start survival"`, `"stop survival"`, `"unit chulacraft-update@bot.service"`, `"cmd survival say hi"`.
        pub calls: StdMutex<Vec<String>>,
    }

    impl FakeBackend {
        pub fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|n| n.to_string()).collect(),
                logs: names
                    .iter()
                    .map(|n| {
                        std::sync::Arc::new(FakeLog {
                            name: n.to_string(),
                            tx: broadcast::channel(16).0,
                            history: vec![line("Done (1.0s)!", 1_000), line("<Steve> hi", 2_000)],
                        })
                    })
                    .collect(),
                active_units: StdMutex::new(HashSet::new()),
                calls: StdMutex::new(vec![]),
            }
        }

        fn known(&self, name: &str) -> Result<(), AppError> {
            if self.names.iter().any(|n| n == name) {
                Ok(())
            } else {
                // Same as the real backend, which answers an unknown instance with an IO error.
                Err(io::Error::new(io::ErrorKind::NotFound, "invalid instance name").into())
            }
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait]
    impl ServiceBackend for FakeBackend {
        async fn server_names(&self) -> Vec<String> {
            self.names.clone()
        }

        async fn unit_status(&self, name: &str) -> Result<Option<UnitStatus>, AppError> {
            self.known(name)?;
            Ok(Some(UnitStatus {
                name: format!("minecraft@{name}.service"),
                description: format!("Minecraft Server {name}"),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
                followed: String::new(),
                object_path: zbus::zvariant::OwnedObjectPath::try_from("/")
                    .expect("valid object path"),
                job_id: 0,
                job_type: String::new(),
                job_path: zbus::zvariant::OwnedObjectPath::try_from("/")
                    .expect("valid object path"),
            }))
        }

        async fn server_process(&self, name: &str) -> Result<Option<Process>, AppError> {
            self.known(name)?;
            Ok(None)
        }

        async fn start_server(&self, name: &str) -> Result<(), AppError> {
            self.known(name)?;
            self.record(format!("start {name}"));
            Ok(())
        }

        async fn stop_server(&self, name: &str) -> Result<(), AppError> {
            self.known(name)?;
            self.record(format!("stop {name}"));
            Ok(())
        }

        async fn restart_server(&self, name: &str) -> Result<(), AppError> {
            self.known(name)?;
            self.record(format!("restart {name}"));
            Ok(())
        }

        async fn start_unit(&self, unit: &str) -> Result<(), AppError> {
            if !allowed_unit(unit) {
                return Err(refuse_unit(unit));
            }
            self.record(format!("unit {unit}"));
            self.active_units.lock().unwrap().insert(unit.to_string());
            Ok(())
        }

        async fn unit_active(&self, unit: &str) -> Result<bool, AppError> {
            Ok(self.active_units.lock().unwrap().contains(unit))
        }

        async fn inject_command(&self, name: &str, cmd: &str) -> Result<(), AppError> {
            self.known(name)?;
            self.record(format!("cmd {name} {}", cmd.trim_end()));
            Ok(())
        }

        async fn log_source(&self, name: &str) -> Option<Arc<dyn LogSource>> {
            let log = self.logs.iter().find(|l| l.name == name)?.clone();
            Some(log)
        }

        async fn reload(&self) -> Result<(), AppError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_units_are_minecraft_and_update_instances_only() {
        for ok in [
            "minecraft@survival.service",
            "minecraft@Sky_Block-2.service",
            "chulacraft-update@bot.service",
        ] {
            assert!(allowed_unit(ok), "{ok}");
        }
        for bad in [
            "",
            "ssh.service",
            "minecraft@.service",
            "minecraft@a.b.service",
            "minecraft@a/b.service",
            "minecraft@a b.service",
            "minecraft@a\n.service",
            "minecraft@survival.socket",
            "minecraft@survival.service ",
            "xminecraft@survival.service",
            "chulacraft-update@.service",
            "chulacraft-update.service",
            "minecraft@..service",
            "minecraft@survival.service.service",
        ] {
            assert!(!allowed_unit(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_fake_backend_refuses_other_units() {
        let backend = FakeBackend::new(&[]);
        assert!(backend.start_unit("ssh.service").await.is_err());
        assert!(
            backend
                .start_unit("chulacraft-update@bot.service")
                .await
                .is_ok()
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["unit chulacraft-update@bot.service"]
        );
    }
}
