use std::{path::PathBuf, str::FromStr};
use clap::Parser;
use http_unix_client::{Client, StatusCode};

#[derive(Debug, Clone)]
#[allow(non_camel_case_types)]
enum ServerAction {
    start,
    stop,
    restart
}

impl FromStr for ServerAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(Self::start),
            "stop" => Ok(Self::stop),
            "restart" => Ok(Self::restart),
            _ => Err(String::from("Invalid Action"))
        }
    }
}

impl ToString for ServerAction {
    fn to_string(&self) -> String {
        String::from(match self {
            Self::start => "start",
            Self::stop => "stop",
            Self::restart => "restart"
        })
    }
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "/run/mcsv_manager.sock")]
    socket: PathBuf,

    action: ServerAction,    
    server_id: String
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = Args::parse();

    
    let client = Client::new();
    
    let response = client
        .post(args.socket, &format!("/server/{}/{}", args.server_id, args.action.to_string()))
        .header("Host", "bypass")
        .send()
        .await.map_err(|e| e.to_string())?;
    Err(match response.status() {
        StatusCode::OK => return Ok(()),
        StatusCode::NOT_FOUND => String::from("Unknown Server ID"),
        StatusCode::INTERNAL_SERVER_ERROR => format!("Error: {}", response.text().await.unwrap_or(String::from("something went wrong"))),
        c => format!("Unexpected status: {}", c)
    })
}