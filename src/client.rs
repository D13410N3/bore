//! Client implementation for the `bore` service.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::sleep, time::Duration};
use tracing::{debug, error, info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    proxy, ClientMessage, Delimited, ServerMessage, CONTROL_PORT, NETWORK_TIMEOUT,
};

/// State structure for the client.
pub struct Client {
    /// Control connection to the server.
    conn: Option<Delimited<TcpStream>>,

    /// Destination address of the server.
    to: String,

    // Local host that is forwarded.
    local_host: String,

    /// Local port that is forwarded.
    local_port: u16,

    /// Port that is publicly available on the remote.
    remote_port: u16,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,
}

impl Client {
    /// Create a new client.
    pub async fn new(
        local_host: &str,
        local_port: u16,
        to: &str,
        port: u16,
        secret: Option<&str>,
    ) -> Result<Self> {
        let mut stream = Delimited::new(connect_with_timeout(to, CONTROL_PORT).await?);
        let auth = secret.map(Authenticator::new);
        if let Some(auth) = &auth {
            auth.client_handshake(&mut stream).await?;
        }

        stream.send(ClientMessage::Hello(port)).await?;
        let remote_port = match stream.recv_timeout().await? {
            Some(ServerMessage::Hello(remote_port)) => remote_port,
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected initial non-hello message"),
            None => bail!("unexpected EOF"),
        };
        info!(remote_port, "connected to server");
        info!("listening at {to}:{remote_port}");

        Ok(Client {
            conn: Some(stream),
            to: to.to_string(),
            local_host: local_host.to_string(),
            local_port,
            remote_port,
            auth,
        })
    }

    /// Create a new client with retry logic for the initial connection.
    pub async fn new_with_retry(
        local_host: &str,
        local_port: u16,
        to: &str,
        port: u16,
        secret: Option<&str>,
        max_retries: u32,
        retry_delay_ms: u64,
        max_retry_delay_ms: u64,
    ) -> Result<Self> {
        let mut attempt = 0;
        let mut delay = retry_delay_ms;
        let infinite_retries = max_retries == 0;

        info!("Starting initial connection with retry logic");
        info!("Connection parameters: to={}, port={}, local_host={}, local_port={}", 
              to, port, local_host, local_port);
        info!("Retry parameters: max_retries={} (0=infinite), initial_delay={}ms, max_delay={}ms",
              max_retries, retry_delay_ms, max_retry_delay_ms);

        loop {
            attempt += 1;
            if !infinite_retries && attempt > max_retries {
                let msg = format!("Maximum connection attempts ({}) reached", max_retries);
                error!("{}", msg);
                bail!(msg);
            }

            info!(
                "Initial connection attempt #{} {}",
                attempt,
                if attempt > 1 { "(retrying)" } else { "" }
            );
            info!("Connecting to {}:{} (control port)", to, CONTROL_PORT);

            match Self::new(local_host, local_port, to, port, secret).await {
                Ok(client) => {
                    info!("Initial connection successful! Remote port: {}", client.remote_port());
                    return Ok(client);
                },
                Err(e) => {
                    error!("Failed to establish initial connection: {}", e);
                    
                    // If this is the last attempt, propagate the error
                    if !infinite_retries && attempt >= max_retries {
                        error!("Maximum connection attempts reached, giving up");
                        return Err(e);
                    }
                    
                    // Wait before retrying with exponential backoff
                    info!("Retrying initial connection in {}ms...", delay);
                    sleep(Duration::from_millis(delay)).await;
                    
                    // Exponential backoff with a maximum delay
                    delay = std::cmp::min(delay * 2, max_retry_delay_ms);
                    info!("Next retry delay: {}ms", delay);
                }
            }
        }
    }

    /// Returns the port publicly available on the remote.
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Start the client, listening for new connections.
    pub async fn listen(mut self) -> Result<()> {
        let mut conn = self.conn.take().unwrap();
        let this = Arc::new(self);
        
        info!("Client started listening for messages from server at {}:{}", this.to, this.remote_port);
        info!("Local endpoint: {}:{}", this.local_host, this.local_port);
        
        let mut last_heartbeat = std::time::Instant::now();
        let heartbeat_timeout = Duration::from_secs(10); // Consider connection lost if no heartbeat for 10 seconds
        
        loop {
            // Check if we've received a heartbeat recently
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_heartbeat);
            
            if elapsed > heartbeat_timeout {
                error!("No heartbeat received from server for {:?} - connection may be lost", elapsed);
                return Err(anyhow::anyhow!("Connection lost: no heartbeat received for {:?}", elapsed));
            }
            
            // Use a timeout for receiving messages to detect connection issues
            match tokio::time::timeout(Duration::from_secs(5), conn.recv()).await {
                Ok(result) => {
                    match result? {
                        Some(ServerMessage::Hello(_)) => {
                            warn!("Unexpected hello message from server");
                        },
                        Some(ServerMessage::Challenge(_)) => {
                            warn!("Unexpected challenge from server");
                        },
                        Some(ServerMessage::Heartbeat) => {
                            debug!("Heartbeat received from server");
                            last_heartbeat = std::time::Instant::now();
                        },
                        Some(ServerMessage::Connection(id)) => {
                            info!("New connection request received from server (id: {})", id);
                            let this = Arc::clone(&this);
                            tokio::spawn(
                                async move {
                                    info!("Handling new connection");
                                    match this.handle_connection(id).await {
                                        Ok(_) => info!("Connection handled successfully"),
                                        Err(err) => warn!(%err, "Connection handling failed"),
                                    }
                                }
                                .instrument(info_span!("proxy", %id)),
                            );
                        }
                        Some(ServerMessage::Error(err)) => {
                            error!("Server reported error: {}", err);
                            return Err(anyhow::anyhow!("Server error: {}", err));
                        },
                        None => {
                            info!("Server closed the connection gracefully");
                            return Ok(());
                        },
                    }
                },
                Err(_) => {
                    // Timeout occurred, but we haven't exceeded heartbeat timeout yet
                    debug!("No message received within timeout, but connection still active");
                    continue;
                }
            }
        }
    }

    async fn handle_connection(&self, id: Uuid) -> Result<()> {
        info!("Establishing connection to server for connection ID: {}", id);
        
        // Connect to the control port
        info!("Connecting to control port at {}:{}", self.to, CONTROL_PORT);
        let mut remote_conn = match connect_with_timeout(&self.to[..], CONTROL_PORT).await {
            Ok(conn) => {
                info!("Successfully connected to control port");
                Delimited::new(conn)
            },
            Err(e) => {
                error!("Failed to connect to control port: {}", e);
                return Err(e.into());
            }
        };
        
        // Perform authentication if needed
        if let Some(auth) = &self.auth {
            info!("Performing client authentication");
            if let Err(e) = auth.client_handshake(&mut remote_conn).await {
                error!("Authentication failed: {}", e);
                return Err(e);
            }
            info!("Authentication successful");
        }
        
        // Send accept message
        info!("Sending accept message for connection ID: {}", id);
        if let Err(e) = remote_conn.send(ClientMessage::Accept(id)).await {
            error!("Failed to send accept message: {}", e);
            return Err(e.into());
        }
        
        // Connect to local port
        info!("Connecting to local service at {}:{}", self.local_host, self.local_port);
        let mut local_conn = match connect_with_timeout(&self.local_host, self.local_port).await {
            Ok(conn) => {
                info!("Successfully connected to local service");
                conn
            },
            Err(e) => {
                error!("Failed to connect to local service: {}", e);
                return Err(e.into());
            }
        };
        
        // Extract parts from remote connection
        let parts = remote_conn.into_parts();
        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
        
        // Write any buffered data to local connection
        if !parts.read_buf.is_empty() {
            info!("Writing {} bytes of buffered data to local connection", parts.read_buf.len());
            if let Err(e) = local_conn.write_all(&parts.read_buf).await {
                error!("Failed to write buffered data to local connection: {}", e);
                return Err(e.into());
            }
        }
        
        // Start proxying data between connections
        info!("Starting to proxy data between remote and local connections");
        match proxy(local_conn, parts.io).await {
            Ok(_) => {
                info!("Proxy completed successfully");
                Ok(())
            },
            Err(e) => {
                error!("Proxy failed: {}", e);
                Err(e.into())
            }
        }
    }
}

async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    match tokio::time::timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))
}
