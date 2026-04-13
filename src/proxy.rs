#[forbid(unsafe_code)]
use fast_socks5::{
    ReplyError, Result, Socks5Command, SocksError, client,
    server::{Socks5ServerProtocol, transfer},
    util::target_addr::TargetAddr,
};
use rand_core::OsRng;
use reticulum_proxy::{Connector, ReticulumInstance};
use reticulum_std::{DestinationHash, Identity, ReticulumNodeBuilder};
use std::{
    collections::HashSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use structopt::StructOpt;
use tokio::{net::TcpListener, sync::RwLock, task};
use tracing::{
    Instrument, Level, debug, error, event, info, instrument, level_filters::LevelFilter, span,
    warn,
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
    /// Bind on address address. eg. `127.0.0.1:1080`
    #[structopt(short, long)]
    pub listen_addr: String,

    /// TCP Connection to Reticulum Instance. eg. `127.0.0.1:8080`
    #[structopt(short, long)]
    pub reticulum_addr: String,

    /// Choose authentication type
    #[structopt(subcommand, name = "auth")] // Note that we mark a field as a subcommand
    pub auth: AuthMode,
}

/// Choose the authentication type
#[derive(StructOpt, Debug, PartialEq)]
enum AuthMode {
    NoAuth,
    Password {
        #[structopt(short, long)]
        username: String,

        #[structopt(short, long)]
        password: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .compact()
        .with_max_level(LevelFilter::TRACE)
        .init();
    spawn_socks_server().await
}

async fn spawn_socks_server() -> Result<()> {
    let opt: &'static Opt = Box::leak(Box::new(Opt::from_args()));

    let backends = Arc::new(RwLock::new(HashSet::new()));

    let identity = Identity::generate(&mut OsRng);
    let builder = ReticulumNodeBuilder::new()
        .identity(identity.clone())
        .add_tcp_client(opt.reticulum_addr.parse().unwrap());
    let mut node = builder.build().await.unwrap();
    node.start().await.unwrap();
    info!("Connected to Reticulum Instance @ {}", opt.reticulum_addr);

    let mut rns = ReticulumInstance::new(node).await;

    let listener = TcpListener::bind(&opt.listen_addr).await?;

    info!("Listen for socks connections @ {}", &opt.listen_addr);

    // Standard TCP loop
    loop {
        async {
            match listener.accept().await {
                Ok((socket, client_addr)) => {
                    debug!(
                        client_addr = client_addr.to_string(),
                        "got new socks5 connection"
                    );
                    let connector = rns.connector();
                    spawn_and_log_error(serve_socks5(opt, backends.clone(), socket, connector));
                }
                Err(err) => {
                    error!(err = %err, "error when accepting socks5 connection");
                }
            }
        }
        .instrument(span!(
            Level::INFO,
            "accepting socs5 connection",
            listen_addr = opt.listen_addr
        ))
        .await
    }
}

static CONN_NUM: AtomicUsize = AtomicUsize::new(0);

#[instrument(skip(opt, backends, socket, connector))]
async fn serve_socks5(
    opt: &Opt,
    backends: Arc<RwLock<HashSet<String>>>,
    socket: tokio::net::TcpStream,
    connector: Connector,
) -> Result<(), SocksError> {
    let (proto, cmd, target_addr) = match &opt.auth {
        AuthMode::NoAuth => Socks5ServerProtocol::accept_no_auth(socket).await?,
        AuthMode::Password { username, password } => {
            Socks5ServerProtocol::accept_password_auth(socket, |user, pass| {
                user == *username && pass == *password
            })
            .await?
            .0
        }
    }
    .read_command()
    .await?;

    if cmd != Socks5Command::TCPConnect {
        proto.reply_error(&ReplyError::CommandNotSupported).await?;
        return Err(ReplyError::CommandNotSupported.into());
    }

    // Not the most reasonable way to implement an admin interface,
    // but rather an example of conditional interception (i.e. just
    // not proxying at all and doing something else in-process).
    if let TargetAddr::Domain(ref domain, _) = target_addr {
        if domain.ends_with(".rns") {
            let Some(destination) = domain.strip_suffix(".rns") else {
                return Err(ReplyError::AddressTypeNotSupported.into());
            };
            let mut buffer = [0u8; 16];
            let Ok(_) = hex::decode_to_slice(destination, &mut buffer) else {
                return Err(ReplyError::AddressTypeNotSupported.into());
            };
            let destination = DestinationHash::new(buffer);

            let inner = proto
                .reply_success(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0))
                .await?;
            let rns_stream = connector.connect(destination).await?;
            transfer(inner, rns_stream).await;
            debug!("socks connection closed");
            return Ok(());
        }
    }

    let (target_addr, target_port) = target_addr.into_string_and_port();

    let backends = backends.read().await;
    let backends: Vec<_> = backends.iter().collect(); // not good but this is just a demo
    if backends.is_empty() {
        warn!("No backends! Go add one using the console");
        proto.reply_error(&ReplyError::NetworkUnreachable).await?;
        return Ok(());
    }
    let n = CONN_NUM.fetch_add(1, Ordering::SeqCst);

    let mut config = client::Config::default();
    config.set_skip_auth(true);
    let client = client::Socks5Stream::connect(
        backends[n % backends.len()],
        target_addr,
        target_port,
        config,
    )
    .await?;
    drop(backends);

    let inner = proto
        .reply_success(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0))
        .await?;

    transfer(inner, client).await;
    Ok(())
}

fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    task::spawn(async move {
        match fut.await {
            Ok(()) => {}
            Err(err) => error!("{:#}", &err),
        }
    })
}
