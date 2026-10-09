use std::{error::Error, path::PathBuf};
use clap::{Parser, Subcommand};
use http_unix_client::{Client, Response, StatusCode};
use mcsv_manager::models::ServerStatus;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long, global = true, default_value = "/run/mcsv_manager.sock")]
    socket: PathBuf,


    #[arg(short, long, global = true, default_value_t = false)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Start {
        server_id: String
    },
    Stop {
        server_id: String
    },
    Restart {
        server_id: String
    },
    SendCommand {
        server_id: String,
        command: String
    },
    Status {
        server_id: String
    },
    List,
}

async fn unwrap_response(response: Response) -> Result<String, Box<dyn Error>> {
    Err(match response.status() {
        StatusCode::OK => return Ok(response.text().await?),
        StatusCode::NOT_FOUND => String::from("Unknown Server ID"),
        StatusCode::INTERNAL_SERVER_ERROR => format!("Error: {}", response.text().await.unwrap_or(String::from("something went wrong"))),
        c => format!("Unexpected status: {}", c)
    }.into())
}

async fn post_server_action(client: &Client, cli: &Cli, server_id: &str, action: &str) -> Result<(), Box<dyn Error>> {
    let response = client
        .post(&cli.socket, &format!("/server/{}/{}", server_id, action))
        .send()
        .await.map_err(|e| e.to_string())?;
    unwrap_response(response).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    
    let client = Client::new();

    match &cli.command {
        Commands::List => {
            let response = client
                .get(cli.socket, &format!("/servers"))
                .send()
                .await.map_err(|e| e.to_string())?;
            let body = unwrap_response(response).await?;
            let list = serde_json::from_str::<Vec<String>>(&body).map_err(|e| e.to_string())?;
            if cli.json {
                println!("{}", serde_json::to_string(&list)?);
            } else {
                for sn in list {
                    println!("{}", &sn);
                }
            }
        },
        Commands::Start { server_id } => {
            post_server_action(&client, &cli, server_id, "start").await?;
        },
        Commands::Stop { server_id } => {
            post_server_action(&client, &cli, server_id, "stop").await?;
        },
        Commands::Restart { server_id } => {
            post_server_action(&client, &cli, server_id, "restart").await?;
        },
        Commands::Status { server_id } => {
            let response = client
                .get(cli.socket, &format!("/server/{}/status", &server_id))
                .send()
                .await.map_err(|e| e.to_string())?;
            let body = unwrap_response(response).await?;
            let stat = serde_json::from_str::<ServerStatus>(&body).map_err(|e| e.to_string())?;
            if cli.json {
                println!("{}", serde_json::to_string(&stat)?);
            } else {
                println!("{:#?}", &stat);
            }
            
        },
        Commands::SendCommand { server_id, command } => {
            let response = client
                .post(cli.socket, &format!("/server/{}/cmd", &server_id))
                .header("Content-Type", "application/json")
                .body(serde_json::to_string(&serde_json::json!({
                    "command": command
                }))?)
                .send()
                .await.map_err(|e| e.to_string())?;
            unwrap_response(response).await?;
        }
    };
    Ok(())
}