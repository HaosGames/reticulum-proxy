//! E2E Test Harness for SOCKS5 Proxy and Reverse Proxy binaries
//!
//! Uses a custom Rust hub instead of rnsd for reliable packet routing.

use std::path::PathBuf;
use std::time::Duration;

use rand_core::OsRng;
use reticulum::{
    destination::DestinationName,
    identity::PrivateIdentity,
    iface::tcp_server::TcpServer,
    transport::{Transport, TransportConfig},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

/// Port constants for E2E tests
#[allow(dead_code)]
pub const RNS_PORT: u16 = 4711;
pub const SOCKS5_PORT: u16 = 1080;
pub const TCP_ECHO_PORT: u16 = 9000;

/// RNS Config path for tests
#[allow(dead_code)]
pub fn rns_config_path() -> PathBuf {
    PathBuf::from("/tmp/reticulum_e2e_test")
}

/// Start a Rust-based hub that routes packets between connected clients
pub async fn start_hub() -> Transport {
    let identity = PrivateIdentity::new_from_name("hub");
    let mut config = TransportConfig::new("hub", &identity, true); // enable_transport = true
    config.set_retransmit(true);
    let mut transport = Transport::new(config);
    
    // Create a dummy destination so the hub is a proper Reticulum node
    // This helps with announcement propagation
    let dummy_id = PrivateIdentity::new_from_name("hub_dummy");
    let _dest = transport.add_destination(dummy_id, reticulum::destination::DestinationName::new("hub", "tcp")).await;
    
    // Spawn TCP server interface
    transport.iface_manager().lock().await.spawn(
        TcpServer::new("127.0.0.1:4711", transport.iface_manager()),
        TcpServer::spawn,
    );
    
    // Wait for server to start
    sleep(Duration::from_millis(500)).await;
    
    transport
}

/// Start TCP echo server using socat
pub async fn start_tcp_echo_server(port: u16) -> tokio::process::Child {
    let port_str = port.to_string();
    let child = tokio::process::Command::new("socat")
        .arg(format!("TCP-LISTEN:{}", port_str))
        .arg("SYSTEM:cat")
        .spawn()
        .expect("Failed to start socat");
    
    // Wait for server to start
    sleep(Duration::from_millis(500)).await;
    
    child
}

/// Stop TCP echo server
pub async fn stop_tcp_echo_server(mut child: tokio::process::Child) {
    child.kill().await.ok();
    child.wait().await.ok();
}

/// Send HTTP request through SOCKS5 proxy and return response code
#[allow(dead_code)]
pub async fn send_http_via_socks5(
    socks5_host: &str,
    socks5_port: u16,
    target: &str,
) -> std::io::Result<u16> {
    // Connect to SOCKS5 proxy
    let addr = format!("{}:{}", socks5_host, socks5_port);
    let mut stream = TcpStream::connect(&addr).await?;
    
    // SOCKS5 greeting
    stream.write_all(&[0x05, 0x01, 0x00]).await?; // VER, NMETHODS, METHODS
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    
    if buf[0] != 0x05 || buf[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SOCKS5 auth failed",
        ));
    }
    
    // Parse target (format: host:port or http://host:port)
    let target = target.trim_start_matches("http://");
    let (host, port) = if let Some(pos) = target.rfind(':') {
        (&target[..pos], target[pos + 1..].parse().unwrap_or(80))
    } else {
        (target, 80u16)
    };
    
    // SOCKS5 Connect request
    let mut request = vec![0x05, 0x01, 0x00, 0x03]; // VER, CMD, RSV, ATYP
    request.push(host.len() as u8);
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    
    stream.write_all(&request).await?;
    
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await?;
    
    if reply[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("SOCKS5 connect failed: {}", reply[1]),
        ));
    }
    
    // Send HTTP request
    let http_request = format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", host);
    stream.write_all(http_request.as_bytes()).await?;
    
    // Read HTTP response
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    
    // Parse HTTP status code
    if response.len() >= 12 {
        let status_line = String::from_utf8_lossy(&response[..response.len().min(100)]);
        if let Some(start) = status_line.find("HTTP/") {
            if let Some(end) = status_line[start..].find(' ') {
                let code_str = &status_line[start + end + 1..];
                if let Some(end) = code_str.find(|c: char| !c.is_ascii_digit()) {
                    let code: u16 = code_str[..end].parse().unwrap_or(0);
                    return Ok(code);
                }
            }
        }
    }
    
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Could not parse HTTP response",
    ))
}

/// Send raw data through SOCKS5 proxy and get response
pub async fn send_raw_via_socks5(
    socks5_host: &str,
    socks5_port: u16,
    target: &str,
    data: &[u8],
) -> std::io::Result<Vec<u8>> {
    let addr = format!("{}:{}", socks5_host, socks5_port);
    let mut stream = TcpStream::connect(&addr).await?;
    
    // SOCKS5 greeting
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    
    // Parse target
    let target = target.trim_start_matches("http://");
    let (host, port) = if let Some(pos) = target.rfind(':') {
        (&target[..pos], target[pos + 1..].parse().unwrap_or(80))
    } else {
        (target, 80u16)
    };
    
    // SOCKS5 Connect
    let mut request = vec![0x05, 0x01, 0x00, 0x03];
    request.push(host.len() as u8);
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await?;
    
    if reply[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "SOCKS5 connect failed",
        ));
    }
    
    // Send data
    stream.write_all(data).await?;
    
    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    // Give the server time to respond
    sleep(Duration::from_millis(500)).await;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    
    Ok(response)
}
