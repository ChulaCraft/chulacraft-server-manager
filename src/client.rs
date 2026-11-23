use std::path::PathBuf;

use axum::http::StatusCode;

use http_unix_client::Response;

use crate::{Error, Result, models::ServerStatus};

pub const DEFAULT_SOCKET_PATH: &'static str = "/run/mcsv_manager.sock";

pub struct Client {
    http_unix_client: http_unix_client::Client,
    socket_path: PathBuf
}

impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { http_unix_client: http_unix_client::Client::new(), socket_path }
    }

    fn error_for_status(response: Response) -> Result<Response> {
        match response.status() {
            StatusCode::FORBIDDEN => Err(Error::Forbidden),
            StatusCode::INTERNAL_SERVER_ERROR => Err(Error::ServiceFailed),
            StatusCode::NOT_FOUND => Err(Error::NotFound),
            _ => Ok(response.error_for_status()?)
        }
    }
    async fn post_server_action(&self, server_id: &str, action: &str) -> Result<()> {
        let response = self.http_unix_client
            .post(&self.socket_path, &format!("/server/{}/{}", server_id, action))
            .send()
            .await?;
        Self::error_for_status(response)?;
        Ok(())
    }

    pub async fn start_server(&self, server_id: &str) -> Result<()> {
        self.post_server_action(server_id, "start").await
    }
    pub async fn stop_server(&self, server_id: &str) -> Result<()> {
        self.post_server_action(server_id, "stop").await
    }
    pub async fn restart_server(&self, server_id: &str) -> Result<()> {
        self.post_server_action(server_id, "restart").await
    }
    pub async fn get_server_status(&self, server_id: &str) -> Result<ServerStatus> {
        let response = self.http_unix_client
            .get(&self.socket_path, &format!("/server/{}/status", server_id))
            .send()
            .await?;
        let body = Self::error_for_status(response)?.text().await?;
        Ok(serde_json::from_str(&body)?)
    }
    pub async fn list_servers(&self) -> Result<Vec<String>> {
        let response = self.http_unix_client
            .get(&self.socket_path, &format!("/servers"))
            .send()
            .await?;
        let body = Self::error_for_status(response)?.text().await?;
        Ok(serde_json::from_str(&body)?)
    }
    pub async fn send_command(&self, server_id: &str, command: &str) -> Result<()> {
        let response = self.http_unix_client
            .post(&self.socket_path, &format!("/server/{}/cmd", server_id))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&serde_json::json!({
                "command": command
            }))?)
            .send()
            .await?;
        Self::error_for_status(response)?;
        Ok(())
    }
}

impl From<http_unix_client::Client> for Client {
    fn from(value: http_unix_client::Client) -> Self {
        Self { http_unix_client: value, socket_path: PathBuf::from(DEFAULT_SOCKET_PATH) }
    }
}