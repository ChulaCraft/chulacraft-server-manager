use std::{path::PathBuf};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::UnixStream;
use tokio_tungstenite::client_async;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "/run/mcsv_manager.sock")]
    socket: PathBuf,
    
    server_id: String
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
        UnixStream::connect(args.socket).await?).await?;
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

    for line in std::io::stdin().lines() {
        let msg = json!({
            "type": "command",
            "command": line.unwrap()
        });
        let msg = serde_json::to_string(&msg).unwrap();
        let _ = write.send(msg.into()).await;
    }

    Ok(())
}