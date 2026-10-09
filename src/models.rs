use std::io;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

use crate::systemd1::{Process, UnitStatus};


#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessStat {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub memory: u64,
    pub start_time: u64,
    pub run_time: u64
}

impl TryFrom<&Process> for ProcessStat {
    type Error = io::Error;
    fn try_from(value: &Process) -> std::result::Result<ProcessStat, Self::Error> {
        let mut system = System::new_with_specifics(RefreshKind::nothing()
                .with_processes(ProcessRefreshKind::everything()));
        system.refresh_all();
        let proc = system.process(Pid::from_u32(value.pid))
            .ok_or(io::Error::new(io::ErrorKind::NotFound, format!("Process with pid {} not found", value.pid)))?;
        Ok(Self {
            pid: value.pid,
            name: proc.name().to_string_lossy().into_owned(),
            cpu_usage: proc.cpu_usage(),
            memory: proc.memory(),
            start_time: proc.start_time(),
            run_time: proc.run_time()
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub stat: Option<ProcessStat>
}

impl From<UnitStatus> for ServerStatus {
    fn from(value: UnitStatus) -> Self {
        Self {
            name: value.name,
            load_state: value.load_state,
            active_state: value.active_state,
            sub_state: value.sub_state,
            stat: None
        }
    }
}