use serde::{Serialize, Deserialize};
use zbus::{Connection, Error, Proxy};

type Result<T> = std::result::Result<T, Error>;


#[derive(Debug)]
pub struct Systemd1 {
    pub conn: Connection,
    pub proxy: Proxy<'static>
}

#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct UnitStatus {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub followed: String,
    pub object_path: zbus::zvariant::OwnedObjectPath,
    pub job_id: u32,
    pub job_type: String,
    pub job_path: zbus::zvariant::OwnedObjectPath,
}

#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct UnitFile {
    pub path: String,
    pub state: String
}

#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct Process {
    pub control_group: String,
    pub pid: u32,
    pub cmd_line: String
}

impl Systemd1 {
    pub async fn new() -> Result<Self> {
        let conn = Connection::system().await?;
        let proxy = Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        ).await?;
        Ok(Systemd1{ conn, proxy })
    }

    pub async fn list_units_by_patterns(&self, states: &Vec<&str>, patterns: &Vec<&str>) -> Result<Vec<UnitStatus>> {
        Ok(self.proxy.call("ListUnitsByPatterns",&(states, patterns)).await?)
    }

    pub async fn list_units_by_names(&self, names: &Vec<&str>) -> Result<Vec<UnitStatus>> {
        Ok(self.proxy.call("ListUnitsByNames",&(names)).await?)
    }

    pub async fn list_unit_files_by_patterns(&self, states: &Vec<&str>, patterns: &Vec<&str>) -> Result<Vec<UnitFile>> {
        Ok(self.proxy.call("ListUnitFilesByPatterns",&(states, patterns)).await?)
    }

    pub async fn start_unit(&self, name: &str, mode: &str) -> Result<zbus::zvariant::OwnedObjectPath> {
        Ok(self.proxy.call("StartUnit", &(name, mode)).await?)
    }

    pub async fn stop_unit(&self, name: &str, mode: &str) -> Result<zbus::zvariant::OwnedObjectPath> {
        Ok(self.proxy.call("StopUnit", &(name, mode)).await?)
    }

    pub async fn restart_unit(&self, name: &str, mode: &str) -> Result<zbus::zvariant::OwnedObjectPath> {
        Ok(self.proxy.call("RestartUnit", &(name, mode)).await?)
    }

    pub async fn get_unit_processes(&self, name: &str) -> Result<Vec<Process>> {
        Ok(self.proxy.call("GetUnitProcesses", &(name)).await?)
    }
}