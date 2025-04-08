use anyhow::Result;
use bore_cli::{client::Client, server::Server};
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};
use tracing::{info, warn, error};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Starts a local proxy to the remote server.
    Local {
        /// The local port to expose.
        #[clap(env = "BORE_LOCAL_PORT")]
        local_port: u16,

        /// The local host to expose.
        #[clap(short, long, value_name = "HOST", default_value = "localhost")]
        local_host: String,

        /// Address of the remote server to expose local ports to.
        #[clap(short, long, env = "BORE_SERVER")]
        to: String,

        /// Optional port on the remote server to select.
        #[clap(short, long, default_value_t = 0)]
        port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Maximum number of reconnection attempts (0 = infinite).
        #[clap(long, default_value_t = 0)]
        max_retries: u32,

        /// Initial reconnection delay in milliseconds (doubles on each retry).
        #[clap(long, default_value_t = 1000)]
        retry_delay_ms: u64,

        /// Maximum reconnection delay in milliseconds.
        #[clap(long, default_value_t = 30000)]
        max_retry_delay_ms: u64,
    },

    /// Runs the remote proxy server.
    Server {
        /// Minimum accepted TCP port number.
        #[clap(long, default_value_t = 1024, env = "BORE_MIN_PORT")]
        min_port: u16,

        /// Maximum accepted TCP port number.
        #[clap(long, default_value_t = 65535, env = "BORE_MAX_PORT")]
        max_port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,
    },
}

async fn run_client_with_reconnect(
    local_host: &str,
    local_port: u16,
    to: &str,
    port: u16,
    secret: Option<&str>,
    max_retries: u32,
    retry_delay_ms: u64,
    max_retry_delay_ms: u64,
) -> Result<()> {
    let mut current_port = port;
    let mut reconnect_attempt = 0;
    let mut reconnect_delay = retry_delay_ms;
    let infinite_retries = max_retries == 0;
    
    info!("Starting bore client with reconnection enabled");
    info!("Target server: {}, local port: {}, initial remote port: {}", to, local_port, port);
    info!("Retry configuration: max_retries={} (0=infinite), initial_delay={}ms, max_delay={}ms", 
          max_retries, retry_delay_ms, max_retry_delay_ms);
    
    loop {
        reconnect_attempt += 1;
        info!("Connection attempt #{}", reconnect_attempt);
        
        // Establish the initial connection with retry logic
        info!("Attempting to establish connection to {}...", to);
        let client = match Client::new_with_retry(
            local_host,
            local_port,
            to,
            current_port,
            secret,
            max_retries,
            retry_delay_ms,
            max_retry_delay_ms,
        ).await {
            Ok(client) => {
                info!("Successfully established connection to server");
                // Reset reconnection delay on successful connection
                reconnect_delay = retry_delay_ms;
                client
            },
            Err(e) => {
                error!("Failed to establish connection after all retries: {}", e);
                
                // If max retries is set and we've reached it, exit
                if !infinite_retries && reconnect_attempt >= max_retries {
                    error!("Maximum reconnection attempts ({}) reached. Exiting.", max_retries);
                    return Err(e);
                }
                
                // Wait before trying again with exponential backoff
                info!("Waiting {}ms before next reconnection attempt...", reconnect_delay);
                tokio::time::sleep(tokio::time::Duration::from_millis(reconnect_delay)).await;
                
                // Exponential backoff with a maximum delay
                reconnect_delay = std::cmp::min(reconnect_delay * 2, max_retry_delay_ms);
                continue;
            }
        };

        // Store the port for reconnection
        current_port = client.remote_port();
        info!("Using remote port: {}", current_port);
        
        // Listen for connections - this will block until connection is lost
        info!("Listening for connections...");
        if let Err(e) = client.listen().await {
            error!("Connection lost due to error: {}", e);
        } else {
            warn!("Connection closed gracefully");
        }

        // If we get here, the connection was lost, so we'll try to reconnect
        info!("Connection to server lost");
        
        // If max retries is set and we've reached it, exit
        if !infinite_retries && reconnect_attempt >= max_retries {
            error!("Maximum reconnection attempts ({}) reached. Exiting.", max_retries);
            return Err(anyhow::anyhow!("Maximum reconnection attempts reached"));
        }
        
        // Wait before reconnecting with exponential backoff
        info!("Waiting {}ms before reconnection attempt...", reconnect_delay);
        tokio::time::sleep(tokio::time::Duration::from_millis(reconnect_delay)).await;
        
        // Exponential backoff with a maximum delay
        reconnect_delay = std::cmp::min(reconnect_delay * 2, max_retry_delay_ms);
        
        info!("Attempting to reconnect to {}:{}", to, current_port);
        // The loop will continue and try to reconnect
    }
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
    match command {
        Command::Local {
            local_host,
            local_port,
            to,
            port,
            secret,
            max_retries,
            retry_delay_ms,
            max_retry_delay_ms,
        } => {
            run_client_with_reconnect(
                &local_host,
                local_port,
                &to,
                port,
                secret.as_deref(),
                max_retries,
                retry_delay_ms,
                max_retry_delay_ms,
            )
            .await?;
        }
        Command::Server {
            min_port,
            max_port,
            secret,
        } => {
            let port_range = min_port..=max_port;
            if port_range.is_empty() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "port range is empty")
                    .exit();
            }
            Server::new(port_range, secret.as_deref()).listen().await?;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    run(Args::parse().command)
}
