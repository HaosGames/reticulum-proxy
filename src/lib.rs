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
#[derive(Clone)]
pub struct ReticulumInstance {
    transport: Arc<Mutex<Transport>>,
}

pub struct ReticulumStream {
    to_send_sender: Sender<Vec<u8>>,
    received_receiver: Receiver<Vec<u8>>,
}

impl ReticulumInstance {
    pub async fn new(transport: Transport) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
        }
    }
    pub async fn listen(&self, destination: AddressHash) -> Option<ReticulumStream> {
        // wait for new incoming link for destination and extract link_id
        let mut receiver = self.transport.lock().await.in_link_events();
        let link_id;
        loop {
            trace!("waiting for new incoming Link");
            match receiver.recv().await {
                Ok(event) => {
                    if let LinkEvent::Activated = event.event {
                        if event.address_hash == destination {
                            link_id = Some(event.id);
                            debug!(
                                "got new active incoming link {} for destination {}",
                                event.id, destination
                            );
                            break;
                        }
                    }
                }
                _ => {
                    debug!("in_link_events_receiver ended");
                    return None;
                }
            }
        }
        let link_id = link_id.unwrap();

        // setup channels
        let (to_send_sender, mut to_send_receiver) = channel::<Vec<u8>>(1000);
        let (received_sender, received_receiver) = channel::<Vec<u8>>(1000);

        let transport = self.transport.clone();
        let _receive_loop = tokio::spawn(async move {
            let mut events = transport.lock().await.in_link_events();
            loop {
                if let Ok(event) = events.recv().await {
                    if event.id == link_id {
                        if let LinkEvent::Data(payload) = event.event {
                            trace!("received {} bytes over link: {}", payload.len(), link_id);
                            let data = payload.as_slice().iter().cloned().collect();
                            received_sender.send(data).await.unwrap();
                        }
                    } else {
                        trace!("ignoring event with id: {}", event.id);
                    }
                } else {
                    debug!("receive loop ended");
                    break;
                }
            }
        });

        let transport = self.transport.clone();
        let _send_loop = tokio::spawn(async move {
            loop {
                if let Some(event) = to_send_receiver.recv().await {
                    let transport = transport.lock().await;
                    if let Some(link) = transport.find_in_link(&link_id).await {
                        let packet = link.lock().await.data_packet(event.as_slice()).unwrap();
                        transport.send_packet(packet).await;
                    } else {
                        trace!("could not find link while sending. Ending send loop");
                        break;
                    }
                } else {
                    debug!("send_loop ended");
                    break;
                }
            }
        });
        Some(ReticulumStream {
            to_send_sender,
            received_receiver,
        })
    }
    pub async fn connect(
        &self,
        destination_hash: AddressHash,
    ) -> Result<ReticulumStream, std::io::Error> {
        // try to retreive destination description for link creation
        let transport = self.transport.lock().await;
        let mut receiver = transport.out_link_events();
        let destination = transport.get_out_destination(&destination_hash).await;
        let Some(destination) = destination else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "did not receive an announcement for that address yet",
            ));
        };
        let description = destination.lock().await.desc.clone();

        //set up link
        trace!("creating link for out destination: {}", destination_hash);
        let link = transport.link(description).await;
        let link_id = link.lock().await.id().clone();
        debug!(
            "created link {} for out destination {}",
            link_id, destination_hash
        );

        // wait for link activation
        loop {
            trace!("waiting for outgoing link {} to activate", link_id);
            match receiver.recv().await {
                Ok(event) => {
                    if let LinkEvent::Activated = event.event {
                        if event.id == link_id {
                            trace!("outgoing link is now active: {}", event.id);
                            break;
                        }
                    }
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "out_link_receiver ended while waiting for link activation",
                    ));
                }
            }
        }
        drop(transport);

        // setup channels
        let (to_send_sender, mut to_send_receiver) = channel::<Vec<u8>>(1000);
        let (received_sender, received_receiver) = channel(1000);

        // send loop: read data from to_send channel receiver and send on links
        let client = self.clone();
        let _send_loop = tokio::spawn(async move {
            while let Some(bytes) = to_send_receiver.recv().await {
                let transport = client.transport.lock().await;
                log::trace!("got bytes ({}) for outgoing link {}", bytes.len(), link_id);
                let link = link.lock().await;
                let packet = link.data_packet(&bytes).unwrap();
                log::trace!("sending to {} on link {}", destination_hash, link_id);
                transport.send_packet(packet).await;
            }
        });

        // upstream link data: put link data into received channel sender
        let client = self.clone();
        let _receive_loop = async move || {
            let mut out_link_events = client.transport.lock().await.out_link_events();
            loop {
                match out_link_events.recv().await {
                    Ok(link_event) => {
                        if link_event.id != link_id {
                            continue;
                        }
                        if link_event.address_hash != destination_hash {
                            continue;
                        }
                        match link_event.event {
                            LinkEvent::Activated => {}
                            LinkEvent::Data(payload) => {
                                log::trace!(
                                    "link {} payload ({} bytes)",
                                    link_event.id,
                                    payload.len()
                                );
                                let data: Vec<u8> = payload.as_slice().iter().cloned().collect();
                                match received_sender.send(data).await {
                                    Ok(()) => log::trace!("tun sent {} bytes", payload.len()),
                                    Err(err) => {
                                        log::error!("tun error sending bytes: {err:?}");
                                    }
                                }
                            }
                            LinkEvent::Closed => {}
                            LinkEvent::Proof(_) => {}
                        }
                    }
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

        Ok(ReticulumStream {
            to_send_sender,
            received_receiver,
        })
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
