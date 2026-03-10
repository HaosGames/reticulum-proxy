use std::{sync::Once, time::Duration};

use log::info;
use rand_core::OsRng;
use reticulum::{destination::DestinationName, identity::PrivateIdentity, iface::{tcp_client::TcpClient, tcp_server::TcpServer}, transport::{Transport, TransportConfig}};
use socks5_reticulum_proxy::ReticulumInstance;
use tokio::time;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static INIT: Once = Once::new();

fn setup() {
    INIT.call_once(|| {
        env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("trace")
        ).init()
    });
}

async fn build_transport_full(
    name: &str,
    server_addr: &str,
    client_addr: &[&str],
    retransmit: bool
) -> Transport {
    let mut config = TransportConfig::new(
        name,
        &PrivateIdentity::new_from_rand(OsRng),
        true
    );

    if retransmit {
        config.set_retransmit(true);
    }

    let transport = Transport::new(config);

    transport.iface_manager().lock().await.spawn(
        TcpServer::new(server_addr, transport.iface_manager()),
        TcpServer::spawn,
    );

    for &addr in client_addr {
        transport
            .iface_manager()
            .lock()
            .await
            .spawn(TcpClient::new(addr), TcpClient::spawn);
    }

    log::info!("test: transport {} created", name);

    transport
}

async fn build_transport(name: &str, server_addr: &str, client_addr: &[&str]) -> Transport {
    build_transport_full(name, server_addr, client_addr, true).await
}

#[tokio::test]
async fn send_receive() {
    setup();

    let mut transport_a = build_transport("a", "127.0.0.1:8081", &[]).await;
    let transport_b = build_transport("b", "127.0.0.1:8082", &["127.0.0.1:8081"]).await;
    let id_a = PrivateIdentity::new_from_name("a");
    let dest_a = transport_a
        .add_destination(id_a, DestinationName::new("test", "hop"))
        .await;
    let dest_a_hash = dest_a.lock().await.desc.address_hash;
    transport_a.send_announce(&dest_a, None).await;
    transport_b.recv_announces().await;
    transport_b.request_path(&dest_a_hash, None, None).await;
    time::sleep(Duration::from_secs(4)).await;

    let instance_a = ReticulumInstance::new(transport_a).await;
    let instance_b = ReticulumInstance::new(transport_b).await;

    let message = "foo";
    let receive_loop = tokio::spawn(async move {
        if let Some(mut stream) = instance_a.listen(dest_a_hash).await {
            let mut buffer = String::default();
            stream.read_to_string(&mut buffer).await.unwrap();
            assert_eq!(buffer, String::from(message));
        } else {
            info!("Listen for connection ended");
        }

    });
    time::sleep(Duration::from_secs(1)).await;
    let send_loop = tokio::spawn(async move {
        let mut stream = instance_b.connect(dest_a_hash).await.unwrap();
        stream.write(message.as_bytes()).await.unwrap();
    });
    time::sleep(Duration::from_secs(1)).await;
    receive_loop.await.unwrap();
    time::sleep(Duration::from_secs(1)).await;
    send_loop.await.unwrap();
}