use std::{sync::Once, time::Duration};

use bytes::BytesMut;
use log::info;
use rand_core::OsRng;
use reticulum_std::{
    Destination, DestinationType, Direction, Identity, ReticulumNode, ReticulumNodeBuilder,
};
use socks5_reticulum_proxy::ReticulumInstance;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time;

static INIT: Once = Once::new();

fn setup() {
    INIT.call_once(|| {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("trace")).init()
    });
}

async fn build_transport_full(
    name: &str,
    server_addr: &str,
    client_addr: &[&str],
    retransmit: bool,
) -> ReticulumNode {
    let identity = Identity::generate(&mut OsRng);
    let mut builder = ReticulumNodeBuilder::new()
        .identity(identity.clone())
        .add_tcp_server(server_addr.parse().unwrap())
        .enable_transport(retransmit);
    for client_addr in client_addr {
        builder = builder.add_tcp_client(client_addr.parse().unwrap());
    }
    let mut node = builder.build().await.unwrap();
    node.start().await.unwrap();

    log::info!("test: transport {} created", name);

    node
}

async fn build_transport(name: &str, server_addr: &str, client_addr: &[&str]) -> ReticulumNode {
    build_transport_full(name, server_addr, client_addr, true).await
}

#[tokio::test]
async fn send_receive() {
    setup();

    let transport_a = build_transport("a", "127.0.0.1:8081", &[]).await;
    let _transport_b = build_transport("b", "127.0.0.1:8082", &["127.0.0.1:8081"]).await;
    let transport_c = build_transport("c", "127.0.0.1:8083", &["127.0.0.1:8082"]).await;
    let id_a = Identity::generate(&mut OsRng);
    let dest_a = Destination::new(
        Some(id_a),
        Direction::In,
        DestinationType::Single,
        "test_a",
        &[],
    )
    .unwrap();
    let dest_a_hash = dest_a.hash().clone();
    transport_a
        .announce_destination(&dest_a_hash, None)
        .await
        .unwrap();
    transport_c.request_path(&dest_a_hash).await.unwrap();
    time::sleep(Duration::from_secs(1)).await;

    let mut instance_a = ReticulumInstance::new(transport_a).await;
    let mut instance_c = ReticulumInstance::new(transport_c).await;

    let message = "foo";
    let receive_loop = tokio::spawn(async move {
        let mut listener = instance_a.listener(dest_a).await.unwrap();
        if let Some(mut stream) = listener.listen().await {
            let mut buffer = [0; 3];
            stream.read(&mut buffer).await.unwrap();
            let received = String::from_utf8_lossy(&buffer).to_string();
            info!("received test message: {}", received);
            assert_eq!(received, String::from(message));
        } else {
            info!("Listen for connection ended");
        }
    });
    let send_loop = tokio::spawn(async move {
        let connector = instance_c.connector();
        let mut stream = connector.connect(dest_a_hash).await.unwrap();
        stream.write(message.as_bytes()).await.unwrap();
    });
    receive_loop.await.unwrap();
    send_loop.await.unwrap();
}

#[tokio::test]
async fn send_receive_reverse() {
    setup();

    let mut transport_a = build_transport("a", "127.0.0.1:8181", &[]).await;
    let _transport_b = build_transport("b", "127.0.0.1:8182", &["127.0.0.1:8181"]).await;
    let transport_c = build_transport("c", "127.0.0.1:8183", &["127.0.0.1:8182"]).await;
    let id_a = Identity::generate(&mut OsRng);
    let dest_a = Destination::new(
        Some(id_a),
        Direction::In,
        DestinationType::Single,
        "test_a",
        &[],
    )
    .unwrap();
    let dest_a_hash = dest_a.hash().clone();
    transport_a
        .announce_destination(&dest_a_hash, None)
        .await
        .unwrap();
    transport_c.request_path(&dest_a_hash).await.unwrap();
    time::sleep(Duration::from_secs(1)).await;

    let mut instance_a = ReticulumInstance::new(transport_a).await;
    let mut instance_c = ReticulumInstance::new(transport_c).await;

    let message = "foo";
    let listen_handle = tokio::spawn(async move {
        let mut listener = instance_a.listener(dest_a).await.unwrap();
        if let Some(mut stream) = listener.listen().await {
            stream.write(message.as_bytes()).await.unwrap();
            time::sleep(Duration::from_secs(1)).await;
        } else {
            info!("Listener ended");
        }
    });
    let connect_handle = tokio::spawn(async move {
        let connector = instance_c.connector();
        let mut stream = connector.connect(dest_a_hash).await.unwrap();
        loop {
            time::sleep(Duration::from_secs(1)).await;
            let mut buffer = BytesMut::with_capacity(3);
            stream.read_buf(&mut buffer).await.unwrap();
            if buffer.is_empty() {
                continue;
            }
            let received = String::from_utf8_lossy(&buffer).to_string();
            info!("received test message: {}", received);
            assert_eq!(received, String::from(message));
            break;
        }
    });
    connect_handle.await.unwrap();
    listen_handle.await.unwrap();
}
