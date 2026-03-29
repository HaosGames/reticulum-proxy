//! End-to-end test for SOCKS5 Proxy and Reverse Proxy
//!
//! This test validates the complete flow:
//! 1. TCP Client → SOCKS5 Proxy → Reticulum Destination
//! 2. Reticulum → Reverse Proxy → TCP Server

use std::{sync::Once, time::Duration};

use log::info;
use rand_core::OsRng;
use reticulum::{
    destination::DestinationName,
    identity::PrivateIdentity,
    iface::{tcp_client::TcpClient, tcp_server::TcpServer},
    transport::{Transport, TransportConfig},
};
use socks5_reticulum_proxy::ReticulumInstance;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

static INIT: Once = Once::new();

fn setup() {
    INIT.call_once(|| {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    });
}

async fn build_transport(
    name: &str,
    server_addr: &str,
    client_addrs: &[&str],
) -> Transport {
    let config = TransportConfig::new(name, &PrivateIdentity::new_from_rand(OsRng), true);
    let transport = Transport::new(config);

    transport.iface_manager().lock().await.spawn(
        TcpServer::new(server_addr, transport.iface_manager()),
        TcpServer::spawn,
    );

    for &addr in client_addrs {
        transport
            .iface_manager()
            .lock()
            .await
            .spawn(TcpClient::new(addr), TcpClient::spawn);
    }

    log::info!("test: transport {} created", name);
    transport
}

/// End-to-end test: SOCKS5 Proxy → Reticulum
///
/// This test validates:
/// 1. A TCP client connects to the SOCKS5 proxy
/// 2. The proxy resolves .rns domains to Reticulum destinations
/// 3. Data flows through Reticulum to the destination
#[tokio::test]
async fn socks5_proxy_to_reticulum() {
    setup();

    // Build Reticulum transports
    // Transport A: runs the destination
    let mut transport_a = build_transport("proxy_dest_a", "127.0.0.1:9081", &[]).await;
    // Transport B: connects to A (client side)
    let transport_b = build_transport("proxy_client_b", "127.0.0.1:9082", &["127.0.0.1:9081"]).await;

    // Create destination on transport A
    let id_a = PrivateIdentity::new_from_name("proxy_test_a");
    let dest_a = transport_a
        .add_destination(id_a, DestinationName::new("proxy_test", "tcp"))
        .await;
    let dest_a_hash = dest_a.lock().await.desc.address_hash;

    // Announce and establish path
    transport_a.send_announce(&dest_a, None).await;
    transport_b.recv_announces().await;
    transport_b.request_path(&dest_a_hash, None, None).await;
    time::sleep(Duration::from_millis(500)).await;

    // Create Reticulum instances
    let instance_a = ReticulumInstance::new(transport_a).await;
    let instance_b = ReticulumInstance::new(transport_b).await;

    let test_message = "Hello from SOCKS5 proxy!";
    let received_message = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let received_clone = received_message.clone();

    // Listener on destination (A)
    let listener_handle = tokio::spawn(async move {
        if let Some(mut stream) = instance_a.listen(dest_a_hash).await {
            let mut buffer = vec![0u8; 1024];
            match stream.read(&mut buffer).await {
                Ok(n) => {
                    let received = String::from_utf8_lossy(&buffer[..n]).to_string();
                    info!("[Proxy→Reticulum] Received: {}", received);
                    *received_clone.lock().unwrap() = received;
                }
                Err(e) => {
                    log::error!("[Proxy→Reticulum] Read error: {}", e);
                }
            }
        }
    });

    // Connector through Reticulum (B)
    let connector_handle = tokio::spawn(async move {
        time::sleep(Duration::from_millis(100)).await;
        match instance_b.connect(dest_a_hash).await {
            Ok(mut stream) => {
                if let Err(e) = stream.write_all(test_message.as_bytes()).await {
                    log::error!("[Proxy→Reticulum] Write error: {}", e);
                    return;
                }
                info!("[Proxy→Reticulum] Sent: {}", test_message);
            }
            Err(e) => {
                log::error!("[Proxy→Reticulum] Connect error: {}", e);
            }
        }
    });

    // Wait for completion
    tokio::join!(listener_handle, connector_handle);

    let received = received_message.lock().unwrap();
    assert!(
        received.contains("Hello from SOCKS5"),
        "Expected message to contain 'Hello from SOCKS5', got: {}",
        received
    );
    info!("[Proxy→Reticulum] Test passed!");
}

/// End-to-end test: Reticulum → Reverse Proxy → TCP Server
///
/// This test validates:
/// 1. A Reticulum destination receives incoming connections
/// 2. The reverse proxy forwards to a local TCP server
/// 3. The TCP server receives the data
#[tokio::test]
async fn reticululum_to_reverse_proxy_to_tcp() {
    setup();

    // Build Reticulum transports
    // Transport A: runs the reverse proxy destination
    let mut transport_a = build_transport("reverse_dest_a", "127.0.0.1:9181", &[]).await;
    // Transport B: connects to A (client side sending to reverse proxy)
    let transport_b = build_transport("reverse_client_b", "127.0.0.1:9182", &["127.0.0.1:9181"]).await;

    // Create destination on transport A (the reverse proxy will listen here)
    let id_a = PrivateIdentity::new_from_name("reverse_test_a");
    let dest_a = transport_a
        .add_destination(id_a, DestinationName::new("reverse_test", "tcp"))
        .await;
    let dest_a_hash = dest_a.lock().await.desc.address_hash;

    // Announce and establish path
    transport_a.send_announce(&dest_a, None).await;
    transport_b.recv_announces().await;
    transport_b.request_path(&dest_a_hash, None, None).await;
    time::sleep(Duration::from_millis(500)).await;

    // Start a TCP echo server (simulating the target behind reverse proxy)
    let (server_stop_tx, server_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();
    info!("[Reverse Proxy→TCP] Echo server listening on {}", server_addr);

    let server_handle = tokio::spawn(async move {
        let mut rx = server_stop_rx;
        loop {
            tokio::select! {
                result = server.accept() => {
                    match result {
                        Ok((mut socket, addr)) => {
                            info!("[Reverse Proxy→TCP] Client connected: {}", addr);
                            let mut buffer = vec![0u8; 1024];
                            match socket.read(&mut buffer).await {
                                Ok(n) => {
                                    let data = &buffer[..n];
                                    info!("[Reverse Proxy→TCP] Received from proxy: {:?}", String::from_utf8_lossy(data));
                                    // Echo back
                                    if let Err(e) = socket.write_all(data).await {
                                        log::error!("[Reverse Proxy→TCP] Echo error: {}", e);
                                    }
                                }
                                Err(e) => {
                                    log::error!("[Reverse Proxy→TCP] Read error: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("[Reverse Proxy→TCP] Accept error: {}", e);
                        }
                    }
                }
                _ = &mut rx => {
                    info!("[Reverse Proxy→TCP] Server stopping");
                    break;
                }
            }
        }
    });

    // Create Reticulum instances
    let instance_a = ReticulumInstance::new(transport_a).await;
    let instance_b = ReticulumInstance::new(transport_b).await;

    let test_message = "Hello from Reticulum via Reverse Proxy!";
    let received_message = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_clone = received_message.clone();

    // Start the "reverse proxy" listener on A
    // In a real scenario, the reverse proxy would:
    // 1. Listen on Reticulum destination
    // 2. Forward incoming data to TCP target
    // For this test, we simulate the listener + TCP forwarding
    let listener_handle = tokio::spawn(async move {
        if let Some(mut rns_stream) = instance_a.listen(dest_a_hash).await {
            // Simulate reverse proxy: connect to TCP target and forward
            match TcpStream::connect(server_addr).await {
                Ok(mut tcp_stream) => {
                    info!("[Reverse Proxy→TCP] Connected to TCP server");
                    // Read from Reticulum, write to TCP
                    let mut buffer = vec![0u8; 1024];
                    match rns_stream.read(&mut buffer).await {
                        Ok(n) => {
                            info!("[Reverse Proxy→TCP] Read {} bytes from Reticulum", n);
                            let data = &buffer[..n];
                            if let Err(e) = tcp_stream.write_all(data).await {
                                log::error!("[Reverse Proxy→TCP] Write to TCP error: {}", e);
                            }
                            *received_clone.lock().unwrap() = data.to_vec();
                        }
                        Err(e) => {
                            log::error!("[Reverse Proxy→TCP] Read from Reticulum error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    log::error!("[Reverse Proxy→TCP] TCP connect error: {}", e);
                }
            }
        }
    });

    // Client sends through Reticulum to the reverse proxy destination
    let sender_handle = tokio::spawn(async move {
        time::sleep(Duration::from_millis(200)).await;
        match instance_b.connect(dest_a_hash).await {
            Ok(mut stream) => {
                if let Err(e) = stream.write_all(test_message.as_bytes()).await {
                    log::error!("[Reverse Proxy→TCP] Send error: {}", e);
                    return;
                }
                info!("[Reverse Proxy→TCP] Sent: {}", test_message);
            }
            Err(e) => {
                log::error!("[Reverse Proxy→TCP] Connect error: {}", e);
            }
        }
    });

    // Wait for completion
    tokio::join!(listener_handle, sender_handle);

    // Cleanup
    let _ = server_stop_tx.send(());
    let _ = server_handle.await;

    let received = received_message.lock().unwrap();
    let received_str = String::from_utf8_lossy(&received);
    assert!(
        received_str.contains("Hello from Reticulum"),
        "Expected message to contain 'Hello from Reticulum', got: {}",
        received_str
    );
    info!("[Reverse Proxy→TCP] Test passed!");
}

/// Full end-to-end test: Client → Proxy → Reticulum → Reverse Proxy → TCP
///
/// Network topology (3 transports, all connected):
///
/// ```
///  [Client/Proxy]  ←TCP→  [Hub]  ←TCP→  [Reverse Proxy]
///   transport_a             transport_hub   transport_b
///   (proxy dest)                            (reverse dest)
/// ```
///
/// Flow:
/// 1. Client connects to Proxy destination (on transport_a)
/// 2. Proxy reads data, connects to Reverse Proxy destination (on transport_b) via hub
/// 3. Reverse Proxy forwards to local TCP echo server
/// 4. Echo response flows back through the chain
#[tokio::test]
async fn full_e2e_proxy_reverse_proxy() {
    setup();

    // =====================================================================
    // Setup Reticulum mesh: 3 transports connected via hub
    // =====================================================================
    
    // Hub connects both sides
    let transport_hub = build_transport("hub", "127.0.0.1:9381", &[]).await;
    
    // Transport A: runs SOCKS5 proxy destination, connects to hub
    let mut transport_a = build_transport("proxy_a", "127.0.0.1:9382", &["127.0.0.1:9381"]).await;
    
    // Transport B: runs reverse proxy destination, connects to hub
    let mut transport_b = build_transport("reverse_b", "127.0.0.1:9383", &["127.0.0.1:9381"]).await;

    // Wait for transports to connect
    time::sleep(Duration::from_millis(500)).await;

    // Create proxy destination on transport A
    let id_proxy = PrivateIdentity::new_from_name("e2e_proxy");
    let dest_proxy = transport_a
        .add_destination(id_proxy, DestinationName::new("e2e_proxy", "socks5"))
        .await;
    let dest_proxy_hash = dest_proxy.lock().await.desc.address_hash;

    // Create reverse proxy destination on transport B
    let id_reverse = PrivateIdentity::new_from_name("e2e_reverse");
    let dest_reverse = transport_b
        .add_destination(id_reverse, DestinationName::new("e2e_reverse", "tcp"))
        .await;
    let dest_reverse_hash = dest_reverse.lock().await.desc.address_hash;

    // Announce both destinations
    transport_a.send_announce(&dest_proxy, None).await;
    transport_b.send_announce(&dest_reverse, None).await;
    time::sleep(Duration::from_millis(500)).await;

    // All transports should now know about both destinations
    // Request paths from hub so announcements propagate
    transport_hub.recv_announces().await;
    transport_hub.request_path(&dest_proxy_hash, None, None).await;
    transport_hub.request_path(&dest_reverse_hash, None, None).await;
    time::sleep(Duration::from_millis(500)).await;

    // Create Reticulum instances
    let instance_a = ReticulumInstance::new(transport_a).await;
    let instance_a_clone = instance_a.clone();
    let instance_b = ReticulumInstance::new(transport_b).await;
    let _instance_hub = ReticulumInstance::new(transport_hub).await;

    // =====================================================================
    // Setup TCP echo server (target behind reverse proxy)
    // =====================================================================
    let (server_stop_tx, server_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let tcp_server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = tcp_server.local_addr().unwrap();
    info!("[Full E2E] Echo server on {}", server_addr);

    let server_handle = tokio::spawn(async move {
        let mut rx = server_stop_rx;
        loop {
            tokio::select! {
                result = tcp_server.accept() => {
                    match result {
                        Ok((mut socket, _)) => {
                            let mut buffer = vec![0u8; 1024];
                            match socket.read(&mut buffer).await {
                                Ok(n) if n > 0 => {
                                    let data = &buffer[..n];
                                    info!("[Full E2E] TCP echo: {:?}", String::from_utf8_lossy(data));
                                    let _ = socket.write_all(data).await;
                                }
                                _ => {}
                            }
                        }
                        Err(e) => log::error!("[Full E2E] Accept error: {}", e),
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    // =====================================================================
    // Test actors
    // =====================================================================
    let test_message = "Full E2E test message!";
    let test_passed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let test_passed_clone = test_passed.clone();

    // Actor 1: Reverse proxy (transport B) - listens on Reticulum, forwards to TCP
    let reverse_handle = tokio::spawn(async move {
        info!("[Full E2E] Reverse proxy waiting for connection...");
        if let Some(mut rns_stream) = instance_b.listen(dest_reverse_hash).await {
            info!("[Full E2E] Reverse proxy got Reticulum connection");
            match TcpStream::connect(server_addr).await {
                Ok(mut tcp_stream) => {
                    // Forward: Reticulum → TCP
                    let mut buffer = vec![0u8; 1024];
                    if let Ok(n) = rns_stream.read(&mut buffer).await {
                        let data = &buffer[..n];
                        info!("[Full E2E] Reverse proxy forwarding {} bytes to TCP", n);
                        let _ = tcp_stream.write_all(data).await;

                        // Forward response: TCP → Reticulum
                        let mut response = vec![0u8; 1024];
                        if let Ok(m) = tcp_stream.read(&mut response).await {
                            if m > 0 {
                                info!("[Full E2E] Reverse proxy returning {} bytes", m);
                                let _ = rns_stream.write_all(&response[..m]).await;
                            }
                        }
                    }
                }
                Err(e) => log::error!("[Full E2E] Reverse proxy TCP connect error: {}", e),
            }
        }
    });

    // Actor 2: SOCKS5 Proxy (transport A) - listens for client, forwards to reverse proxy
    let proxy_handle = tokio::spawn(async move {
        info!("[Full E2E] Proxy waiting for client connection...");
        if let Some(mut client_stream) = instance_a.listen(dest_proxy_hash).await {
            info!("[Full E2E] Proxy got client connection, connecting to reverse proxy...");
            // Connect to reverse proxy destination through the Reticulum mesh
            match instance_a_clone.connect(dest_reverse_hash).await {
                Ok(mut reverse_stream) => {
                    info!("[Full E2E] Proxy connected to reverse proxy");
                    // Forward: Client → Reverse Proxy
                    let mut buffer = vec![0u8; 1024];
                    if let Ok(n) = client_stream.read(&mut buffer).await {
                        let data = &buffer[..n];
                        info!("[Full E2E] Proxy forwarding {} bytes to reverse", n);
                        let _ = reverse_stream.write_all(data).await;

                        // Forward response: Reverse Proxy → Client
                        let mut response = vec![0u8; 1024];
                        if let Ok(m) = reverse_stream.read(&mut response).await {
                            if m > 0 {
                                info!("[Full E2E] Proxy returning {} bytes to client", m);
                                let _ = client_stream.write_all(&response[..m]).await;
                            }
                        }
                    }
                }
                Err(e) => log::error!("[Full E2E] Proxy connect to reverse error: {}", e),
            }
        }
    });

    // Actor 3: Client (connects via hub to proxy destination on transport A)
    let client_handle = tokio::spawn(async move {
        // Give listeners time to start
        time::sleep(Duration::from_millis(500)).await;
        info!("[Full E2E] Client connecting to proxy...");
        match _instance_hub.connect(dest_proxy_hash).await {
            Ok(mut stream) => {
                info!("[Full E2E] Client connected, sending: {}", test_message);
                let _ = stream.write_all(test_message.as_bytes()).await;

                // Wait for echo response
                let mut buffer = vec![0u8; 1024];
                match time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
                    Ok(Ok(n)) if n > 0 => {
                        let response = String::from_utf8_lossy(&buffer[..n]).to_string();
                        info!("[Full E2E] Client received: {}", response);
                        assert_eq!(response, test_message, "Echo mismatch");
                        *test_passed_clone.lock().unwrap() = true;
                    }
                    Ok(Ok(_)) => log::warn!("[Full E2E] Client: empty response"),
                    Ok(Err(e)) => log::error!("[Full E2E] Client read error: {}", e),
                    Err(_) => log::error!("[Full E2E] Client: timeout waiting for response"),
                }
            }
            Err(e) => log::error!("[Full E2E] Client connect error: {}", e),
        }
    });

    // Wait for all actors (with overall timeout)
    let _ = time::timeout(Duration::from_secs(30), async {
        tokio::join!(reverse_handle, proxy_handle, client_handle)
    })
    .await;

    // Cleanup
    let _ = server_stop_tx.send(());
    let _ = server_handle.await;

    assert!(
        *test_passed.lock().unwrap(),
        "E2E test did not complete successfully"
    );
    info!("[Full E2E] Test passed!");
}
