use anyhow::Result;
use bore_cli::{client::Client, server::Server};
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

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
    let mut attempt = 0;
    let mut delay = retry_delay_ms;
    let mut last_port = port;

    loop {
        attempt += 1;
        if max_retries > 0 && attempt > max_retries {
            warn!("Maximum reconnection attempts ({}) reached. Exiting.", max_retries);
            break;
        }

        info!(
            "Connection attempt {} {}",
            attempt,
            if attempt > 1 { "(reconnecting)" } else { "" }
        );

        match Client::new(local_host, local_port, to, last_port, secret).await {
            Ok(client) => {
                // Store the port in case we need to reconnect
                last_port = client.remote_port();
                
                // Reset backoff on successful connection
                delay = retry_delay_ms;
                
                info!("Connected successfully to {}:{}", to, last_port);
                
                // Listen for connections - this will block until connection is lost
                if let Err(e) = client.listen().await {
                    warn!("Connection lost: {}", e);
                } else {
                    warn!("Connection closed");
                }
            }
            Err(e) => {
                warn!("Failed to connect: {}", e);
            }
        }

        // Wait before reconnecting with exponential backoff
        info!("Reconnecting in {} ms...", delay);
        sleep(Duration::from_millis(delay)).await;
        
        // Exponential backoff with a maximum delay
        delay = std::cmp::min(delay * 2, max_retry_delay_ms);
    }

    Ok(())
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
