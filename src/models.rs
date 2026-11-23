use std::{fmt, io};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use time::{OffsetDateTime, UtcOffset, format_description};

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
/// Formats a duration in seconds into a human-readable string like "2d 4h 15m 30s".
fn format_duration(total_seconds: u64) -> String {
    if total_seconds == 0 {
        return "0s".to_string();
    }

    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 {
        parts.push(format!("{seconds}s"));
    }

    parts.join(" ")
}

/// Formats bytes into a human-readable unit (B, KB, MB, GB, TB).
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Converts Unix epoch seconds into the machine's local time, falling back to UTC if unavailable.
fn format_epoch_local(epoch_secs: u64) -> String {
    let Ok(utc_dt) = OffsetDateTime::from_unix_timestamp(epoch_secs as i64) else {
        return format!("{epoch_secs} (invalid timestamp)");
    };

    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_dt = utc_dt.to_offset(offset);

    let fmt = format_description::parse_borrowed::<1>("[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory]:[offset_minute]")
        .expect("valid format description");

    local_dt.format(&fmt).unwrap_or_else(|_| local_dt.to_string())
}

impl fmt::Display for ProcessStat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PID: {} | {} | CPU: {:.1}% | Mem: {} | Started: {} | Uptime: {}",
            self.pid,
            self.name,
            self.cpu_usage,
            format_bytes(self.memory),
            format_epoch_local(self.start_time),
            format_duration(self.run_time),
        )
    }
}

impl fmt::Display for ServerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} ({}/{}) [load: {}]",
            self.name, self.active_state, self.sub_state, self.load_state
        )?;

        match &self.stat {
            Some(stat) => write!(f, "  {}", stat),
            None => write!(f, "  No process stats available"),
        }
    }
}
