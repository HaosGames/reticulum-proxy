#[forbid(unsafe_code)]
#[macro_use]
extern crate log;

use fast_socks5::{
    ReplyError, Result, Socks5Command, SocksError, client,
    server::{Socks5ServerProtocol, transfer},
    util::target_addr::TargetAddr,
};
use rand_core::OsRng;
use reticulum::{
    destination::{
        self, DestinationDesc, SingleInputDestination,
        link::{LinkEvent, LinkId},
    },
    hash::AddressHash,
    identity::PrivateIdentity,
    transport::{Transport, TransportConfig},
};
use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use structopt::StructOpt;
use tokio::{
    io::{AsyncRead, AsyncWrite},
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
    env_logger::init();

    spawn_socks_server().await
}

async fn spawn_socks_server() -> Result<()> {
    let opt: &'static Opt = Box::leak(Box::new(Opt::from_args()));

    let backends = Arc::new(RwLock::new(HashSet::new()));

    info!("Creating Reticulum Client");
    let rns_identitiy = PrivateIdentity::new_from_rand(OsRng);
    let rns_config = TransportConfig::new("socks5-proxy", &rns_identitiy, false);
    let rns_transport = Transport::new(rns_config);
    let rns_client = ReticulumClient::new(rns_transport, rns_identitiy, 1).await;
    info!("Starting Reticulum Client");
    rns_client.run().await;

    let listener = TcpListener::bind(&opt.listen_addr).await?;

    info!("Listen for socks connections @ {}", &opt.listen_addr);

    // Standard TCP loop
    loop {
        match listener.accept().await {
            Ok((socket, _client_addr)) => {
                let rns_client = rns_client.clone();
                spawn_and_log_error(serve_socks5(opt, backends.clone(), socket, rns_client));
            }
            Err(err) => {
                error!("accept error = {:?}", err);
            }
        }
    }
}

static CONN_NUM: AtomicUsize = AtomicUsize::new(0);

async fn serve_socks5(
    opt: &Opt,
    backends: Arc<RwLock<HashSet<String>>>,
    socket: tokio::net::TcpStream,
    rns_client: ReticulumClient,
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
            let Ok(destination) = AddressHash::new_from_hex_string(destination) else {
                return Err(ReplyError::AddressTypeNotSupported.into());
            };

            let inner = proto
                .reply_success(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0))
                .await?;
            let rns_stream = rns_client.establish_connection(&destination).await.unwrap();
            transfer(inner, rns_stream).await;
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

#[derive(Clone)]
struct ReticulumClient {
    in_destination: Arc<Mutex<SingleInputDestination>>,
    destination_descriptions: Arc<Mutex<BTreeMap<AddressHash, DestinationDesc>>>,
    received_senders: Arc<Mutex<BTreeMap<LinkId, Sender<Vec<u8>>>>>,
    announce_freq_secs: u64,
    transport: Arc<Mutex<Transport>>,
}

struct ReticulumStream {
    to_send_sender: Sender<Vec<u8>>,
    received_receiver: Receiver<Vec<u8>>,
}

impl ReticulumClient {
    pub async fn new(
        mut transport: Transport,
        id: PrivateIdentity,
        announce_freq_secs: u64,
    ) -> Self {
        let in_destination = transport
            .add_destination(id, destination::DestinationName::new("rns_vpn", "client"))
            .await;
        Self {
            in_destination,
            transport: Arc::new(Mutex::new(transport)),
            destination_descriptions: Default::default(),
            received_senders: Default::default(),
            announce_freq_secs,
        }
    }
    pub async fn establish_connection(
        &self,
        destination: &AddressHash,
    ) -> Result<ReticulumStream, std::io::Error> {
        //search for destination in already received announcements
        let description_map = self.destination_descriptions.lock().await;
        let Some(description) = description_map.get(destination) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "did not receive an announcement for that address yet",
            ));
        };

        //set up link
        let transport = self.transport.lock().await;
        let link = transport.link(*description).await;
        let link_id = link.lock().await.id().clone();
        log::debug!("created link {} for peer {}", link_id, destination);

        // setup channels
        let (to_send_sender, mut to_send_receiver) = channel::<Vec<u8>>(1000);
        let (received_sender, received_receiver) = channel(1000);
        let mut senders = self.received_senders.lock().await;
        senders.insert(*destination, received_sender);

        // send loop: read data from to_send channel receiver and send on links
        let _send_loop = async || {
            while let Some(bytes) = to_send_receiver.recv().await {
                log::trace!("got tun bytes ({})", bytes.len());
                if let Some(link) = transport.find_out_link(&link_id).await {
                    log::trace!("sending to {} on link {}", destination, link_id);
                    let link = link.lock().await;
                    let packet = link.data_packet(&bytes).unwrap();
                    drop(link);
                    transport.send_packet(packet).await;
                } else {
                    log::warn!("could not get link {}", link_id);
                }
            }
        };

        Ok(ReticulumStream {
            to_send_sender,
            received_receiver,
        })
    }

    pub async fn run(&self) -> tokio::task::JoinHandle<()> {
        // send announces
        let client = self.clone();
        let announce_loop = async move || loop {
            client
                .transport
                .lock()
                .await
                .send_announce(&client.in_destination, None)
                .await;
            tokio::time::sleep(std::time::Duration::from_secs(
                client.announce_freq_secs as u64,
            ))
            .await;
        };
        // save destination descriptions
        let client = self.clone();
        let destination_loop = async move || {
            let mut announce_recv = client.transport.lock().await.recv_announces().await;
            while let Ok(announce) = announce_recv.recv().await {
                let destination = announce.destination.lock().await;
                let mut destination_map = client.destination_descriptions.lock().await;
                destination_map.insert(destination.desc.address_hash.clone(), destination.desc);
            }
        };

        // upstream link data: put link data into received channel sender
        let client = self.clone();
        let receive_loop = async move || {
            let mut in_link_events = client.transport.lock().await.in_link_events();
            loop {
                match in_link_events.recv().await {
                    Ok(link_event) => match link_event.event {
                        LinkEvent::Activated => {}
                        LinkEvent::Data(payload) => {
                            let mut received_senders = client.received_senders.lock().await;
                            if let Some(sender) = received_senders.get_mut(&link_event.id) {
                                log::trace!(
                                    "link {} payload ({} bytes)",
                                    link_event.id,
                                    payload.len()
                                );
                                let data: Vec<u8> = payload.as_slice().iter().cloned().collect();
                                match sender.send(data).await {
                                    Ok(()) => log::trace!("tun sent {} bytes", payload.len()),
                                    Err(err) => {
                                        log::error!("tun error sending bytes: {err:?}");
                                    }
                                }
                            };
                        }
                        LinkEvent::Closed => {}
                        LinkEvent::Proof(_) => {}
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("recv in link event lagged: {n}");
                    }
                    Err(err) => {
                        log::error!("recv in link event error: {err:?}");
                        break;
                    }
                }
            }
        };
        let run_handle = tokio::spawn(async move {
            tokio::select! {
              _ = announce_loop() => log::info!("announce loop exited: shutting down"),
              _ = destination_loop() => log::info!("destination loop exited: shutting down"),
              _ = receive_loop() => log::info!("receive loop exited: shutting down"),
              _ = tokio::signal::ctrl_c() => log::info!("got ctrl-c: shutting down")
            }
        });
        run_handle
    }
}

impl AsyncRead for ReticulumStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.received_receiver.poll_recv(cx) {
            std::task::Poll::Ready(Some(data)) => {
                buf.put_slice(data.as_slice());
                return std::task::Poll::Ready(Ok(()));
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "",
            ))),
        }
    }
}

impl AsyncWrite for ReticulumStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let data = buf.iter().cloned().collect();
        match self.to_send_sender.try_send(data) {
            Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => std::task::Poll::Ready(Err(
                std::io::Error::new(std::io::ErrorKind::ConnectionReset, ""),
            )),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => std::task::Poll::Pending,
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.received_receiver.close();
        std::task::Poll::Ready(Ok(()))
    }
}
