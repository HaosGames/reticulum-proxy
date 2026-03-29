#[forbid(unsafe_code)]
#[macro_use]
extern crate log;

use rand_core::OsRng;
use reticulum::{
    destination::DestinationName,
    hash::AddressHash,
    identity::PrivateIdentity,
    iface::tcp_client::TcpClient,
    transport::{Transport, TransportConfig},
};
use serde::{Deserialize, Serialize};
use socks5_reticulum_proxy::ReticulumInstance;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use structopt::StructOpt;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;

/// JSON mapping configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MappingConfig {
    pub aspects: String,
    pub forward_to: String,
    pub address_hash: Option<String>,
}

pub type Mappings = HashMap<String, MappingConfig>;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "reverse-proxy",
    about = "A reverse proxy to bridge Reticulum to TCP/IP"
)]
struct Opt {
    #[structopt(short = "i", long)]
    pub reticulum_identity_path: PathBuf,
    #[structopt(short = "c", long)]
    pub reticulum_connection: String,
    #[structopt(short = "m", long)]
    pub mappings_path: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .init();
    spawn_reverse_proxy().await
}

async fn spawn_reverse_proxy() -> anyhow::Result<()> {
    let opt: &'static Opt = Box::leak(Box::new(Opt::from_args()));

    let rns_identity = load_or_create_identity(&opt.reticulum_identity_path).await?;
    let rns_config = TransportConfig::new("reverse-proxy", &rns_identity, false);
    let mut rns_transport = Transport::new(rns_config);

    let _client_addr = rns_transport.iface_manager().lock().await.spawn(
        TcpClient::new(opt.reticulum_connection.as_str()),
        TcpClient::spawn,
    );
    info!(
        "Connected to Reticulum instance @ {}",
        opt.reticulum_connection
    );

    let mut mappings = load_mappings(&opt.mappings_path).await?;
    info!("Loaded {} destination mappings", mappings.len());

    for mapping in mappings.iter_mut() {
        let destination_name = DestinationName::new(&mapping.0, &mapping.1.aspects);
        let destination = rns_transport
            .add_destination(rns_identity.clone(), destination_name)
            .await;
        let destination_hash = destination.lock().await.desc.address_hash;
        let hash_str = destination_hash.to_hex_string();
        mapping.1.address_hash = Some(hash_str.clone());
        
        // Write hash to file for discovery
        let hash_file = std::path::PathBuf::from("/tmp/reticulum-reverse-hash");
        use tokio::io::AsyncWriteExt;
        if let Ok(mut file) = tokio::fs::File::create(&hash_file).await {
            file.write_all(format!("{}:{}", mapping.0, hash_str).as_bytes()).await.ok();
            info!("Wrote hash to /tmp/reticulum-reverse-hash");
        }
    }

    let rns_client = ReticulumInstance::new(rns_transport).await;

    let cancel = CancellationToken::new();

    for mapping in mappings {
        let destination_hash = mapping.1.address_hash.unwrap();
        let target_addr: SocketAddr = mapping.1.forward_to.parse().map_err(|e| {
            anyhow::anyhow!("Invalid forward address '{}': {}", mapping.1.forward_to, e)
        })?;
        let name = mapping.0.clone();
        let rns_client = rns_client.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            info!(
                "Listening for Reticulum connections @ {} for mapping {}",
                destination_hash, mapping.0
            );
            loop {
                match rns_client
                    .listen(AddressHash::new_from_hex_string(destination_hash.as_str()).unwrap())
                    .await
                {
                    Some(stream) => {
                        let name = name.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, name, target_addr).await
                            {
                                error!("Connection handler error: {}", e);
                            }
                        });
                    }
                    None => {
                        error!("Reticulum listener ended");
                        break;
                    }
                }
            }
            cancel.cancel();
        });
    }
    cancel.cancelled().await;

    Ok(())
}

async fn load_or_create_identity(path: &PathBuf) -> anyhow::Result<PrivateIdentity> {
    if let Ok(mut file) = File::open(path).await {
        let mut hex_string = String::new();
        file.read_to_string(&mut hex_string).await?;
        let identity = PrivateIdentity::new_from_hex_string(hex_string.trim())
            .map_err(|e| anyhow::anyhow!("Failed to parse identity: {:?}", e))?;
        info!("Loaded existing identity from {:?}", path);
        Ok(identity)
    } else {
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let hex = identity.to_hex_string();
        let mut file = File::create(path).await?;
        file.write_all(hex.as_bytes()).await?;
        info!("Created new identity and saved to {:?}", path);
        Ok(identity)
    }
}

async fn load_mappings(path: &PathBuf) -> anyhow::Result<Mappings> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read mappings file {:?}: {}", path, e))?;
    let mappings: Mappings = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse mappings JSON: {}", e))?;
    Ok(mappings)
}

async fn handle_connection(
    mut stream: socks5_reticulum_proxy::ReticulumStream,
    name: String,
    target_addr: SocketAddr,
) -> anyhow::Result<()> {
    info!("Forwarding connection for '{}' to {}", name, target_addr);
    let mut target = TcpStream::connect(target_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to target {}: {}", target_addr, e))?;

    match tokio::io::copy_bidirectional(&mut stream, &mut target).await {
        Ok((a, b)) => {
            info!(
                "Connection closed for '{}': {} bytes sent, {} bytes received",
                name, a, b
            );
        }
        Err(e) => {
            error!("Connection error for '{}': {}", name, e);
        }
    }
    Ok(())
}
