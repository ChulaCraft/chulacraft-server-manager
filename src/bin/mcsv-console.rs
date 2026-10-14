use std::{path::PathBuf};
use chrono::{DateTime, Utc};
use chrono_english::{Dialect, parse_date_string};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use mcsv_manager::models::ServerStatus;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;
use tokio_tungstenite::client_async;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "/run/mcsv_manager.sock")]
    socket: PathBuf,
    
    server_id: String,

    #[arg(short = 'S', long, value_parser = parse_journal_time)]
    since: Option<DateTime<Utc>>,

    #[arg(long)]
    since_current_start: bool
}

fn parse_journal_time(s: &str) -> Result<DateTime<Utc>, String> {
    // Handle specific journalctl keywords not natively handled by chrono-english
    let parsed_str = match s.to_lowercase().as_str() {
        "today" => "00:00",
        "yesterday" => "yesterday 00:00",
        _ => s,
    };

    parse_date_string(parsed_str, Utc::now(), Dialect::Uk)
        .map_err(|e| format!("Invalid time format '{}': {}", s, e))
}

#[derive(Deserialize, Debug)]
struct LogMsg {
    message: String,
    comm: String,
    pid: i64,
    // timestamp: u64
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let (ws_stream, _response) = client_async(
        format!("ws://bypass/server/{}/console", args.server_id),
        UnixStream::connect(args.socket.clone()).await?).await?;
    println!("Connected to console!");

    let (mut write, mut read) = ws_stream.split();

    tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            if msg.is_text() {
                let msg = serde_json::from_str::<Map<String, Value>>(msg.into_text().unwrap().as_str()).unwrap();
                let msg_type = msg.get("type").unwrap().as_str().unwrap();
                match msg_type {
                    "error" => {
                        println!("Error: {}", msg.get("message").unwrap().as_str().unwrap());
                    },
                    "log" => {
                        let log_line = serde_json::from_value::<LogMsg>(Value::Object(msg)).unwrap();
                        println!("{}[{}] {}", log_line.comm, log_line.pid, log_line.message);
                    },
                    ut => eprintln!("Unknown message type: {}", ut)
                }
            }
        }
    });

    if args.since_current_start {
        let client = http_unix_client::Client::new();
        let response = client
            .get(args.socket.clone(), &format!("/server/{}/status", &args.server_id))
            .send()
            .await.map_err(|e| e.to_string())?;
        let body = response.text().await?;
        let stat = serde_json::from_str::<ServerStatus>(&body).map_err(|e| e.to_string())?;
        if let Some(stat) = stat.stat {
            let msg = json!({
                "type": "get_logs",
                "since": stat.start_time
            });
            let msg = serde_json::to_string(&msg).unwrap();
            write.send(msg.into()).await?;
        }

    } else if args.since.is_some() {
        let since_usec = args.since.as_ref().and_then(|t| Some(t.timestamp_micros())).unwrap_or(0);
        let msg = json!({
            "type": "get_logs",
            "since": since_usec
        });
        let msg = serde_json::to_string(&msg).unwrap();
        write.send(msg.into()).await?;
    }

    for line in std::io::stdin().lines() {
        let msg = json!({
            "type": "command",
            "command": line.unwrap()
        });
        let msg = serde_json::to_string(&msg).unwrap();
        let _ = write.send(msg.into()).await?;
    }

    Ok(())
}