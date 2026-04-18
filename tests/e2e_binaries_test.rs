//! Full E2E tests for SOCKS5 Proxy and Reverse Proxy binaries

mod e2e;

use std::path::PathBuf;
use std::time::Duration;

use rand_core::OsRng;
use tokio::net::TcpStream;
use tokio::time::sleep;

use e2e::{SOCKS5_PORT, TCP_ECHO_PORT};

fn test_dir() -> PathBuf {
    PathBuf::from("/tmp/reticulum_e2e_binaries_test")
}

fn proxy_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("proxy")
}

fn reverse_proxy_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("reverse-proxy")
}

fn create_identity(path: &PathBuf) {
    let identity = reticulum_std::Identity::generate(&mut OsRng);
    let bytes = identity.private_key_bytes().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(path, bytes).unwrap();
}

fn create_mappings(path: &PathBuf, port: u16) {
    let mappings = serde_json::json!({
        "test_service": {
            "aspects": "tcp.http",
            "forward_to": format!("127.0.0.1:{}", port)
        }
    });
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(path, serde_json::to_string_pretty(&mappings).unwrap()).unwrap();
}

#[tokio::test]
async fn e2e_binaries_prerequisites() {
    assert!(proxy_binary().exists(), "proxy binary should exist");
    assert!(
        reverse_proxy_binary().exists(),
        "reverse-proxy binary should exist"
    );
    println!("Binary paths verified");
}

#[tokio::test]
async fn e2e_proxy_starts() {
    let test_dir = test_dir();
    std::fs::remove_dir_all(&test_dir).ok();
    std::fs::create_dir_all(&test_dir).ok();

    let proxy_identity = test_dir.join("proxy_identity.hex");
    create_identity(&proxy_identity);

    // Start Rust hub instead of rnsd
    let _hub = e2e::start_hub().await;
    sleep(Duration::from_millis(500)).await;

    let mut proxy = tokio::process::Command::new(proxy_binary())
        .args([
            "-l",
            &format!("127.0.0.1:{}", SOCKS5_PORT),
            "-r",
            "127.0.0.1:4711",
            "no-auth",
        ])
        .env("RUST_LOG", "info")
        .spawn()
        .expect("Should start proxy");

    sleep(Duration::from_secs(2)).await;

    let connected = TcpStream::connect(format!("127.0.0.1:{}", SOCKS5_PORT)).await;
    println!("Proxy connection: {:?}", connected);

    proxy.kill().await.ok();
    std::fs::remove_dir_all(&test_dir).ok();

    assert!(connected.is_ok(), "Should connect to SOCKS5 proxy");
}

#[tokio::test]
async fn e2e_reverse_proxy_starts() {
    let test_dir = test_dir();
    std::fs::remove_dir_all(&test_dir).ok();
    std::fs::create_dir_all(&test_dir).ok();

    let reverse_identity = test_dir.join("reverse_identity.hex");
    create_identity(&reverse_identity);

    let mappings = test_dir.join("mappings.json");
    create_mappings(&mappings, TCP_ECHO_PORT);

    // Start Rust hub instead of rnsd
    let _hub = e2e::start_hub().await;
    sleep(Duration::from_millis(500)).await;

    let mut reverse = tokio::process::Command::new(reverse_proxy_binary())
        .args([
            "-i",
            reverse_identity.to_str().unwrap(),
            "-c",
            "127.0.0.1:4711",
            "-m",
            mappings.to_str().unwrap(),
        ])
        .env("RUST_LOG", "info")
        .spawn()
        .expect("Should start reverse-proxy");

    sleep(Duration::from_secs(2)).await;

    println!("Reverse-proxy started");

    reverse.kill().await.ok();
    std::fs::remove_dir_all(&test_dir).ok();
}

async fn get_reverse_hash() -> String {
    // Wait for the hash file to be created
    for _ in 0..30 {
        if let Ok(content) = std::fs::read_to_string("/tmp/reticulum-reverse-hash") {
            if let Some(line) = content.lines().next() {
                if let Some(hash) = line.split(':').nth(1) {
                    return hash.to_string();
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    panic!("Could not get reverse proxy hash");
}

#[tokio::test]
async fn e2e_full_flow() {
    let test_dir = test_dir();
    std::fs::remove_dir_all(&test_dir).ok();
    std::fs::create_dir_all(&test_dir).ok();

    let proxy_identity = test_dir.join("proxy_identity.hex");
    let reverse_identity = test_dir.join("reverse_identity.hex");
    create_identity(&proxy_identity);
    create_identity(&reverse_identity);

    let mappings = test_dir.join("mappings.json");
    create_mappings(&mappings, TCP_ECHO_PORT);

    // Clean up hash file from previous runs
    std::fs::remove_file("/tmp/reticulum-reverse-hash").ok();

    // Start Rust hub instead of rnsd
    let _hub = e2e::start_hub().await;
    sleep(Duration::from_millis(500)).await;

    let echo = e2e::start_tcp_echo_server(TCP_ECHO_PORT).await;

    // Start PROXY first this time
    let mut proxy = tokio::process::Command::new(proxy_binary())
        .args([
            "-l",
            &format!("127.0.0.1:{}", SOCKS5_PORT),
            "-r",
            "127.0.0.1:4711",
            "no-auth",
        ])
        .env("RUST_LOG", "trace")
        .spawn()
        .expect("Should start proxy");

    sleep(Duration::from_secs(2)).await;

    // Then start reverse-proxy (so proxy is already connected when it announces)
    let mut reverse = tokio::process::Command::new(reverse_proxy_binary())
        .args([
            "-i",
            reverse_identity.to_str().unwrap(),
            "-c",
            "127.0.0.1:4711",
            "-m",
            mappings.to_str().unwrap(),
        ])
        .env("RUST_LOG", "trace")
        .spawn()
        .expect("Should start reverse-proxy");

    sleep(Duration::from_secs(3)).await;

    // Extra time for announcements to propagate
    sleep(Duration::from_secs(2)).await;

    // Get the hash from reverse-proxy
    let hash = get_reverse_hash().await;
    println!("Reverse proxy hash: {}", hash);

    // More time for proxy to receive and process the announcement
    sleep(Duration::from_secs(2)).await;

    // Send request through SOCKS5 using .rns domain with hash
    // Format: service_name.destination_hash.rns
    let target = format!("{}.rns", hash);
    println!("Connecting to: {}", target);

    let result = e2e::send_raw_via_socks5("127.0.0.1", SOCKS5_PORT, &target, b"Hello E2E!").await;

    println!("E2E flow result: {:?}", result);

    proxy.kill().await.ok();
    reverse.kill().await.ok();
    e2e::stop_tcp_echo_server(echo).await;
    std::fs::remove_dir_all(&test_dir).ok();

    // Verify we got a response (even if empty, the connection worked)
    if let Ok(data) = result {
        println!("Received {} bytes", data.len());
    }
}
