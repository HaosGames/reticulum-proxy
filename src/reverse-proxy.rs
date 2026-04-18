#[forbid(unsafe_code)]
use rand_core::OsRng;
use reticulum_proxy::ReticulumInstance;
use reticulum_std::{Destination, DestinationType, Direction, Identity, ReticulumNodeBuilder};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use structopt::StructOpt;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, level_filters::LevelFilter};

/// JSON mapping configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MappingConfig {
    pub aspects: Vec<String>,
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
    tracing_subscriber::fmt()
        .compact()
        .with_max_level(LevelFilter::TRACE)
        .init();
    spawn_reverse_proxy().await
}

async fn spawn_reverse_proxy() -> anyhow::Result<()> {
    let opt: &'static Opt = Box::leak(Box::new(Opt::from_args()));

    let identity = load_or_create_identity(&opt.reticulum_identity_path).await?;
    let builder = ReticulumNodeBuilder::new()
        .identity(identity.clone())
        .add_tcp_client(opt.reticulum_connection.parse().unwrap());
    let mut node = builder.build().await.unwrap();
    node.start().await.unwrap();

    info!(
        "Connected to Reticulum instance @ {}",
        opt.reticulum_connection
    );
    let mut rns = ReticulumInstance::new(node).await;

    let mappings = load_mappings(&opt.mappings_path).await?;
    let cancel = CancellationToken::new();
    info!("Loaded {} destination mappings", mappings.len());

    for mut mapping in mappings {
        let mut destination = Destination::new(
            Some(identity.clone()),
            Direction::In,
            DestinationType::Single,
            &mapping.0,
            &[],
        )
        .unwrap();
        destination.set_accepts_links(true);
        let destination_hash = destination.hash().clone();
        mapping.1.address_hash = Some(destination_hash.to_string());

        // Write hash to file for discovery
        let hash_file = std::path::PathBuf::from("/tmp/reticulum-reverse-hash");
        use tokio::io::AsyncWriteExt;
        if let Ok(mut file) = tokio::fs::File::create(&hash_file).await {
            file.write_all(format!("{}:{}", mapping.0, destination_hash).as_bytes())
                .await
                .ok();
            info!("Wrote hash to /tmp/reticulum-reverse-hash");
        }

        let target_addr: SocketAddr = mapping.1.forward_to.parse().map_err(|e| {
            anyhow::anyhow!("Invalid forward address '{}': {}", mapping.1.forward_to, e)
        })?;
        let name = mapping.0.clone();
        let mut listener = rns.listener(destination).await.unwrap();
        let cancel_token = cancel.clone();
        tokio::spawn(async move {
            info!(
                destination.hash = hex::encode(destination_hash.as_bytes()),
                destination.name = mapping.0,
                "listening for Reticulum connections",
            );
            loop {
                select! {
                    result = listener.listen() => {
                        match result {
                            Some(stream) => {
                                let name = name.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, name, target_addr).await {
                                        error!("Connection handler error: {}", e);
                                    }
                                });
                            }
                            None => {
                                error!(
                                    destination.hash = hex::encode(destination_hash.as_bytes()),
                                    destination.name = mapping.0,
                                    "reticulum listener ended");
                                cancel_token.cancel();
                            }
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        break;
                    }

                }
            }
            cancel_token.cancel();
        });
    }

    cancel.cancelled().await;

    Ok(())
}

#[instrument]
async fn load_or_create_identity(path: &PathBuf) -> anyhow::Result<Identity> {
    if let Ok(mut file) = File::open(path).await {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        let identity = Identity::from_private_key_bytes(bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("Failed to parse identity: {:?}", e))?;
        info!("loaded existing identity");
        Ok(identity)
    } else {
        let identity = Identity::generate(&mut OsRng);
        let bytes = identity.private_key_bytes().unwrap();
        let mut file = File::create(path).await?;
        file.write_all(bytes.as_ref()).await?;
        info!("created and saved new identity");
        Ok(identity)
    }
}

#[instrument]
async fn load_mappings(path: &PathBuf) -> anyhow::Result<Mappings> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read mappings file {:?}: {}", path, e))?;
    let mappings: Mappings = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse mappings JSON: {}", e))?;
    info!("loaded mappings");
    Ok(mappings)
}

#[instrument(skip(stream))]
async fn handle_connection(
    mut stream: reticulum_proxy::ReticulumStream,
    name: String,
    target_addr: SocketAddr,
) -> anyhow::Result<()> {
    let mut target = TcpStream::connect(target_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to target {}: {}", target_addr, e))?;

    match tokio::io::copy_bidirectional(&mut stream, &mut target).await {
        Ok((a, b)) => {
            debug!(
                destination.name = name,
                sent.bytes = a,
                received.bytes = b,
                "connection closed",
            );
        }
        Err(e) => {
            error!(
                destination.name = name,
                error = %e,
                "connection error for",
            );
        }
    }
    Ok(())
}
