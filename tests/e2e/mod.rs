//! E2E Test Harness for SOCKS5 Proxy and Reverse Proxy
//!
//! This module provides utilities for end-to-end testing of the proxy binaries
//! using an external Reticulum instance (rnsd).

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::time::sleep;

/// Port constants for E2E tests
pub const RNS_PORT: u16 = 4711;
pub const SOCKS5_PORT: u16 = 1080;
pub const TCP_ECHO_PORT: u16 = 9000;
pub const RNS_HOST: &str = "127.0.0.1";

/// RNS Config path for tests
pub fn rns_config_path() -> PathBuf {
    PathBuf::from("/tmp/reticulum_e2e_test")
}

/// Start rnsd (Reticulum daemon) as a subprocess
pub async fn start_rnsd() -> std::io::Result<tokio::process::Child> {
    let config_dir = rns_config_path();
    std::fs::create_dir_all(&config_dir).ok();
    
    // Create minimal Reticulum config (this is a directory, not a file)
    let config_content = r#"
[reticulum]
shared_instance = yes
instance_control_port = 4712

[[Default Interface]]
type = TCPInterface
listen_ip = 127.0.0.1
listen_port = 4711
enabled = yes
"#;
    
    let config_file = config_dir.join("config");
    std::fs::write(&config_file, config_content).ok();
    
    // Use the venv Python with rns
    let rnsd_path = "/tmp/rns-venv/bin/rnsd";
    
    let child = tokio::process::Command::new(rnsd_path)
        .args(["--config", config_dir.to_str().unwrap()])
        .spawn()?;
    
    // Wait for rnsd to start
    sleep(Duration::from_secs(2)).await;
    
    Ok(child)
}

/// Stop rnsd subprocess
pub async fn stop_rnsd(mut child: tokio::process::Child) {
    child.kill().await.ok();
    child.wait().await.ok();
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
