use clap::{Parser, Subcommand};
use clawchat_server::{auth, ClawChatServer, ServerConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "clawchat-server", about = "ClawChat server daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the ClawChat server
    Serve {
        /// Unix socket path
        #[arg(long, default_value = default_socket_path())]
        socket: PathBuf,

        /// TCP bind address (set to empty or use --no-tcp to disable)
        #[arg(long, default_value = "127.0.0.1:9229")]
        tcp: String,

        /// Disable TCP listener
        #[arg(long)]
        no_tcp: bool,

        /// HTTP/WebSocket bind address (e.g., 0.0.0.0:8080)
        #[arg(long)]
        http: Option<String>,

        /// Disable API key validation (open access, for local dev)
        #[arg(long)]
        no_auth: bool,

        /// SQLite database path
        #[arg(long, default_value = default_db_path())]
        db: PathBuf,

        /// API key file path
        #[arg(long, default_value = default_key_path())]
        key_file: PathBuf,
    },

    /// Manage authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Show the current API key
    ShowKey {
        #[arg(long, default_value = default_key_path())]
        key_file: PathBuf,
    },
    /// Rotate the API key (generates a new one)
    RotateKey {
        #[arg(long, default_value = default_key_path())]
        key_file: PathBuf,
    },
}

fn default_data_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".clawchat"))
        .unwrap_or_else(|| PathBuf::from(".clawchat"))
}

fn default_socket_path() -> &'static str {
    // Leak the string to get a 'static str for clap default
    Box::leak(
        default_data_dir()
            .join("clawchat.sock")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    )
}

fn default_db_path() -> &'static str {
    Box::leak(
        default_data_dir()
            .join("clawchat.db")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    )
}

fn default_key_path() -> &'static str {
    Box::leak(
        default_data_dir()
            .join("auth.key")
            .to_string_lossy()
            .into_owned()
            .into_boxed_str(),
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            socket,
            tcp,
            no_tcp,
            http,
            no_auth,
            db,
            key_file,
        } => {
            let config = ServerConfig {
                socket_path: socket,
                tcp_addr: if no_tcp { None } else { Some(tcp) },
                http_addr: http,
                db_path: db,
                auth_key_path: key_file,
                no_auth,
            };

            let server = ClawChatServer::new(config)?;
            if no_auth {
                log::info!("Running in NO-AUTH mode (open access)");
            } else {
                log::info!("API key: {}", server.api_key());
            }
            server.run().await?;
        }
        Commands::Auth { action } => match action {
            AuthAction::ShowKey { key_file } => {
                let key = auth::load_or_create_key(&key_file)?;
                println!("{}", key);
            }
            AuthAction::RotateKey { key_file } => {
                let key = auth::rotate_key(&key_file)?;
                println!("New API key: {}", key);
                println!("All connected agents will need to reconnect with the new key.");
            }
        },
    }

    Ok(())
}
