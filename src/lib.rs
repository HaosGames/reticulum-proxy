#[forbid(unsafe_code)]
#[macro_use]
extern crate log;
use reticulum::{
    destination::{SingleInputDestination, link::LinkEvent},
    hash::AddressHash,
    transport::Transport,
};
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
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
        let instance = Self {
            transport: Arc::new(Mutex::new(transport)),
        };

        // Spawn background task to continuously receive announcements
        let transport_clone = instance.transport.clone();
        tokio::spawn(async move {
            let mut receiver = transport_clone.lock().await.recv_announces().await;
            loop {
                match receiver.recv().await {
                    Ok(_ann) => {
                        log::debug!("Received announcement");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        log::debug!("Announcement receiver closed");
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("Announcement receiver lagged by {} messages", n);
                    }
                }
            }
        });

        instance
    }

    pub async fn send_announce(&self, destination: &Arc<Mutex<SingleInputDestination>>) {
        self.transport
            .lock()
            .await
            .send_announce(&destination, None)
            .await;
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
                                "in link {}: new active incoming link for destination {}",
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
        let link = self
            .transport
            .lock()
            .await
            .find_in_link(&link_id)
            .await
            .unwrap();
        link.lock().await.prove_messages(true);

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
                            if let Err(error) = received_sender.send(data).await {
                                let _bytes = error.0;
                                warn!(
                                    "received_sender ended while trying to send from reticlulum to stream"
                                )
                                // TODO we need to try to re-initiate the connection to the mapped destination
                                // so that these bytes can be delivered even if that connection ended.
                                // For that we may need to eliminate the channel in between
                                // the ReticulumStream and the TcpStream.
                            }
                        }
                    } else {
                        trace!("ignoring event with id: {}", event.id);
                    }
                } else {
                    debug!("listener in link event receiver ended. Ending Listener receive loop");
                    break;
                }
            }
            debug!("listener receive loop ended");
        });

        let transport = self.transport.clone();
        let _send_loop = tokio::spawn(async move {
            loop {
                if let Some(data) = to_send_receiver.recv().await {
                    trace!("in link {}: sending {} bytes", link_id, data.len());
                    let transport = transport.lock().await;
                    let link = link.lock().await;
                    for chunk in data.chunks(1024) {
                        let packet = link.data_packet(chunk).unwrap();
                        transport.send_packet(packet).await;
                    }
                } else {
                    debug!(
                        "listener to_send_receiver ended. Ending listener send loop and closing link"
                    );
                    link.lock().await.close();
                    break;
                }
            }
            debug!("listener send_loop ended");
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
        link.lock().await.prove_messages(true);
        let link_id = link.lock().await.id().clone();
        debug!(
            "out link {}: created for out destination {}",
            link_id, destination_hash
        );

        // wait for link activation
        loop {
            trace!("out link {}: waiting for activation", link_id);
            match receiver.recv().await {
                Ok(event) => {
                    if let LinkEvent::Activated = event.event {
                        if event.id == link_id {
                            trace!("out link {}: activated", event.id);
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
                log::trace!("out link {}: got bytes ({})", link_id, bytes.len());
                let link = link.lock().await;
                for chunk in bytes.chunks(1024) {
                    let packet = link.data_packet(chunk).unwrap();
                    log::trace!(
                        "out link {}: sending packet to {}",
                        link_id,
                        destination_hash
                    );
                    transport.send_packet(packet).await;
                }
            }
            debug!("connection send loop ended because to send receiver ended. Closing link");
            link.lock().await.close();
        });

        // upstream link data: put link data into received channel sender
        let client = self.clone();
        let _receive_loop = tokio::spawn(async move {
            let mut out_link_events = client.transport.lock().await.out_link_events();
            loop {
                match out_link_events.recv().await {
                    Ok(event) => {
                        if event.id != link_id {
                            trace!(
                                "ignoring out link event: id {}, destination {}",
                                event.id, event.address_hash
                            );
                            continue;
                        }
                        match event.event {
                            LinkEvent::Activated => {
                                trace!("out link {}: got activated event", link_id);
                            }
                            LinkEvent::Data(payload) => {
                                log::trace!(
                                    "out link {}: received payload ({} bytes)",
                                    event.id,
                                    payload.len()
                                );
                                let data: Vec<u8> = payload.as_slice().iter().cloned().collect();
                                match received_sender.send(data).await {
                                    Ok(()) => {}
                                    Err(err) => {
                                        log::error!(
                                            "out link {}: error while sending received bytes to stream: {err:?}. Ending connection receive loop",
                                            link_id
                                        );
                                        break;
                                    }
                                }
                            }
                            LinkEvent::Closed => {
                                debug!(
                                    "out link {}: closed. Ending connection receive loop",
                                    link_id
                                );
                                break;
                            }
                            LinkEvent::Proof(_) => {
                                debug!("out link {}: got proof event", link_id);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("connection out link receiver lagged: {n}");
                    }
                    Err(err) => {
                        log::error!("connection out link receiver error: {err:?}");
                        break;
                    }
                }
            }
            debug!("connection receive loop ended");
        });

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
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Ok(())),
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
