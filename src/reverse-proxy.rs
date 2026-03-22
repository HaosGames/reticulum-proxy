#[forbid(unsafe_code)]
#[macro_use]
extern crate log;

use rand_core::OsRng;
use reticulum::{
    destination::{
        self, DestinationDesc, DestinationName, SingleInputDestination,
        link::{self, Link, LinkEvent, LinkId},
    },
    hash::AddressHash,
    identity::{Identity, PrivateIdentity},
    iface::tcp_client::TcpClient,
    transport::{self, Transport, TransportConfig},
};
use socks5_reticulum_proxy::{ReticulumInstance, ReticulumStream};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use structopt::StructOpt;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{
        Mutex, RwLock,
        mpsc::{Receiver, Sender, channel},
    },
    task,
};

/// # How to use it:
///
/// Listen on a local address, authentication-free:
///     `$ RUST_LOG=debug cargo run --example router -- --listen-addr 127.0.0.1:1080 no-auth`
///
/// Listen on a local address, with basic username/password requirement:
///     `$ RUST_LOG=debug cargo run --example router -- --listen-addr 127.0.0.1:1080 password --username admin --password password`
///
/// Now, connections will be refused since there are no backends.
///
/// Run a backend proxy, with skipped authentication mode (-k):
///     `$ RUST_LOG=debug cargo run --example server -- --listen-addr 127.0.0.1:1337 --public-addr 127.0.0.1 -k no-auth`
///
/// Connect to the secret admin console and add the backend:
///     `$ socat --experimental SOCKS5-CONNECT:127.0.0.1:admin.internal:1234 READLINE`
///     `ADD 127.0.0.1:1337`
///
/// You can add more backends and they'll be used in a round-robin fashion.
///
#[derive(Debug, StructOpt)]
#[structopt(
    name = "socks5-reticulum-proxy",
    about = "A socks5 proxy to bridge TCP/IP to Reticulum"
)]
struct Opt {
    /// Text file with hex string
    #[structopt(short = "i", long)]
    pub reticulum_identiy_path: PathBuf,

    #[structopt(short = "c", long)]
    pub reticulum_connection: String,

    #[structopt(short = "d", long)]
    pub reticulum_destination_name: String,

    #[structopt(short = "a", long)]
    pub reticulum_destination_aspects: String,

    #[structopt(short = "m", long)]
    pub mappings_path: String,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    spawn_proxy().await
}

async fn spawn_proxy() -> std::io::Result<()> {
    let opt: &'static Opt = Box::leak(Box::new(Opt::from_args()));

    let backends = Arc::new(RwLock::new(HashSet::new()));

    let rns_identitiy = if let Ok(file) = File::open(opt.reticulum_identiy_path).await {
        let mut hex_string = String::default();
        file.read_to_string(&mut hex_string).await.unwrap();
        PrivateIdentity::new_from_hex_string(&hex_string).unwrap()
    } else {
        let ident = PrivateIdentity::new_from_rand(OsRng);
        let file = File::create(opt.reticulum_identiy_path).await.unwrap();
        file.write(ident.to_hex_string().as_bytes()).await;
        ident
    };
    let rns_config = TransportConfig::new("socks5-proxy", &rns_identitiy, false);
    let rns_transport = Transport::new(rns_config);
    let _client_addr = rns_transport.iface_manager().lock().await.spawn(
        TcpClient::new(opt.reticulum_connection.as_str()),
        TcpClient::spawn,
    );
    info!(
        "Connected to Reticulum Instance @ {}",
        opt.reticulum_connection
    );

    let mappings_file_content = tokio::fs::read_to_string(opt.mappings_path).await.unwrap();
    let mappings: HashMap<String, (String, String)> = serde_json::from_str(mappings_file_content.as_str()).unwrap();
    let destination = rns_transport
        .add_destination(
            rns_identitiy,
            DestinationName::new(&opt.reticulum_destination_name, &opt.reticulum_destination_aspects),
        )
        .await;
    let destination_hash = destination.lock().await.desc.address_hash;

    let rns_client = ReticulumInstance::new(rns_transport).await;
    info!(
        "Listen for reticulum connections @ {}",
        destination_hash
    );

    loop {
        match rns_client.listen(destination_hash).await {
            Some(stream) => {
                spawn_and_log_error(serve_socks5(opt, stream));
            }
            None => {
                error!("Reticulum Listener ended");
            }
        }
    }
}

static CONN_NUM: AtomicUsize = AtomicUsize::new(0);

async fn serve_socks5(opt: &Opt, stream: ReticulumStream) -> std::io::Result<()> {
    fast_socks5::server::transfer(inner, client).await;
    Ok(())
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = std::io::Result<()>> + Send + 'static,
{
    task::spawn(async move {
        match fut.await {
            Ok(()) => {}
            Err(err) => error!("{:#}", &err),
        }
    })
}
