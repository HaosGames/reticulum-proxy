//! Simple connect/listen test for Reticulum

use std::sync::Once;
use std::time::Duration;

use log::info;
use rand_core::OsRng;
use reticulum::{
    destination::DestinationName,
    identity::PrivateIdentity,
    iface::{tcp_client::TcpClient, tcp_server::TcpServer},
    transport::{Transport, TransportConfig},
};
use socks5_reticulum_proxy::ReticulumInstance;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time;

static INIT: Once = Once::new();

fn setup() {
    INIT.call_once(|| {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    });
}

async fn build(name: &str, listen: &str, connects: &[&str]) -> Transport {
    let config = TransportConfig::new(name, &PrivateIdentity::new_from_rand(OsRng), true);
    let t = Transport::new(config);
    t.iface_manager().lock().await.spawn(
        TcpServer::new(listen, t.iface_manager()),
        TcpServer::spawn,
    );
    for &addr in connects {
        t.iface_manager().lock().await.spawn(TcpClient::new(addr), TcpClient::spawn);
    }
    info!("{} @ {} -> {:?}", name, listen, connects);
    t
}

/// Simple test: Connect -> Listen -> Write -> Read
#[tokio::test]
async fn simple_connect_listen() {
    setup();

    // A: proxy destination
    let mut a = build("a", "127.0.0.1:9481", &["127.0.0.1:9482"]).await;
    // B: reverse destination, connected to A
    let mut b = build("b", "127.0.0.1:9482", &["127.0.0.1:9481"]).await;
    time::sleep(Duration::from_millis(300)).await;

    // Create destination on A
    let id_a = PrivateIdentity::new_from_name("simple_a");
    let dest_a = a.add_destination(id_a, DestinationName::new("simple_a", "tcp")).await;
    let hash_a = dest_a.lock().await.desc.address_hash;

    // Create destination on B
    let id_b = PrivateIdentity::new_from_name("simple_b");
    let dest_b = b.add_destination(id_b, DestinationName::new("simple_b", "tcp")).await;
    let hash_b = dest_b.lock().await.desc.address_hash;

    // Announce
    a.send_announce(&dest_a, None).await;
    b.send_announce(&dest_b, None).await;
    a.recv_announces().await;
    b.recv_announces().await;

    // Paths
    a.request_path(&hash_b, None, None).await;
    b.request_path(&hash_a, None, None).await;
    time::sleep(Duration::from_millis(300)).await;

    let inst_a = ReticulumInstance::new(a).await;
    let inst_b = ReticulumInstance::new(b).await;

    let msg = "HELLO";
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let got2 = got.clone();

    // B listens on hash_b
    let h = tokio::spawn(async move {
        info!("B: listening on {}", hash_b);
        if let Some(mut s) = inst_b.listen(hash_b).await {
            info!("B: got connection");
            let mut buf = vec![0u8; 64];
            match time::timeout(Duration::from_secs(3), s.read(&mut buf)).await {
                Ok(Ok(n)) => {
                    info!("B: read {} bytes: {:?}", n, String::from_utf8_lossy(&buf[..n]));
                    *got2.lock().unwrap() = buf[..n].to_vec();
                }
                Ok(Err(e)) => log::error!("B: read err: {}", e),
                Err(_) => log::error!("B: read timeout"),
            }
        }
    });

    time::sleep(Duration::from_millis(100)).await;

    // A connects to B and sends
    let h2 = tokio::spawn(async move {
        info!("A: connecting to {}", hash_b);
        match time::timeout(Duration::from_secs(3), inst_a.connect(hash_b)).await {
            Ok(Ok(mut s)) => {
                info!("A: connected, writing: {}", msg);
                let _ = s.write_all(msg.as_bytes()).await;
                info!("A: wrote");
            }
            Ok(Err(e)) => log::error!("A: connect err: {}", e),
            Err(_) => log::error!("A: connect timeout"),
        }
    });

    let _ = time::timeout(Duration::from_secs(10), async { 
        let (r1, r2) = tokio::join!(h, h2); 
        r1.unwrap();
        r2.unwrap();
    }).await;

    let data = got.lock().unwrap();
    assert!(!data.is_empty(), "No data received");
    assert_eq!(&*data, msg.as_bytes(), "Data mismatch");
    info!("Test passed!");
}
