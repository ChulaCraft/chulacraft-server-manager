use std::path::PathBuf;
use clap::{Parser, Subcommand};
use mcsv_manager::{Result, client::{Client, DEFAULT_SOCKET_PATH}};

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long, global = true, default_value = DEFAULT_SOCKET_PATH)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    let client = Client::new(cli.socket);

    match &cli.command {
        Commands::List => {
            let list = client.list_servers().await?;
            if cli.json {
                println!("{}", serde_json::to_string(&list)?);
            } else {
                for sn in list {
                    println!("{}", &sn);
                }
            }
        },
        Commands::Start { server_id } => {
            client.start_server(server_id).await?;
        },
        Commands::Stop { server_id } => {
            client.stop_server(server_id).await?;
        },
        Commands::Restart { server_id } => {
            client.restart_server(server_id).await?;
        },
        Commands::Status { server_id } => {
            let stat = client.get_server_status(server_id).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&stat)?);
            } else {
                println!("{}", &stat);
            }
            
        },
        Commands::SendCommand { server_id, command } => {
            client.send_command(server_id, command).await?;
        }
    };
    Ok(())
}