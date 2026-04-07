#[forbid(unsafe_code)]
#[macro_use]
extern crate log;
use anyhow::Result;
use reticulum_std::{Destination, DestinationHash, LinkId, NodeEvent, ReticulumNode};
use std::collections::HashMap;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
};
use tokio_util::sync::CancellationToken;

pub struct ReticulumInstance {
    event_loop: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    connect_requests: tokio::sync::mpsc::Sender<(
        DestinationHash,
        tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    )>,
    listener_requests:
        tokio::sync::mpsc::Sender<(Destination, tokio::sync::oneshot::Sender<Result<Listener>>)>,
}

pub struct Listener {
    streams: tokio::sync::mpsc::Receiver<ReticulumStream>,
}

impl Listener {
    pub async fn listen(&mut self) -> Option<ReticulumStream> {
        self.streams.recv().await
    }
}

pub struct Connector {
    request: tokio::sync::mpsc::Sender<(
        DestinationHash,
        tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    )>,
}

impl Connector {
    pub async fn connect(self, hash: DestinationHash) -> Result<ReticulumStream> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.request.send((hash, sender)).await.unwrap();
        receiver.await.unwrap()
    }
}

#[derive(Debug)]
pub struct ReticulumStream {
    received: tokio::sync::mpsc::Receiver<Vec<u8>>,
    to_send: tokio::sync::mpsc::Sender<(LinkId, Vec<u8>)>,
    link: LinkId,
}

struct Context {
    listeners: HashMap<DestinationHash, tokio::sync::mpsc::Sender<ReticulumStream>>,
    to_send_sender: tokio::sync::mpsc::Sender<(LinkId, Vec<u8>)>,
    received: HashMap<LinkId, tokio::sync::mpsc::Sender<Vec<u8>>>,
    cancel: CancellationToken,
}

impl ReticulumInstance {
    pub async fn new(mut node: ReticulumNode) -> Self {
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let (connect_requests_sender, mut connect_requests) = tokio::sync::mpsc::channel(10);
        let (listener_requests_sender, mut listener_requests) = tokio::sync::mpsc::channel(10);
        let (to_send_sender, mut to_send) = tokio::sync::mpsc::channel(10);

        let mut context = Context {
            listeners: Default::default(),
            received: Default::default(),
            to_send_sender,
            cancel: cancel.clone(),
        };

        let event_loop = tokio::spawn(async move {
            let mut receiver = node
                .take_event_receiver()
                .expect("this is the only instance");
            loop {
                select! {
                    _ = cancel_task.cancelled() => {
                        break;
                    }
                    event = receiver.recv() => {
                        if let Some(event) = event {
                            Self::handle_event(&mut node,event, &mut context).await;
                        } else {
                            info!("event loop receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    request = connect_requests.recv() => {
                        if let Some((hash, sender)) = request {
                            Self::handle_connect_request(&mut node, &mut context, hash, sender).await;
                        } else {
                            info!("connect_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    request = listener_requests.recv() => {
                        if let Some((hash, sender)) = request {
                            Self::handle_listener_request(&mut node, &mut context, hash, sender).await;
                        } else {
                            info!("listener_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    to_send = to_send.recv() => {
                        if let Some((link, data)) = to_send {
                            Self::handle_to_send(&mut node, &mut context, link, data).await;
                        } else {
                            info!("listener_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                }
            }
        });
        let instance = Self {
            event_loop,
            cancel,
            connect_requests: connect_requests_sender,
            listener_requests: listener_requests_sender,
        };

        instance
    }

    pub fn connector(&mut self) -> Connector {
        Connector {
            request: self.connect_requests.clone(),
        }
    }

    pub async fn listener(&mut self, destination: Destination) -> Result<Listener> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.listener_requests
            .send((destination, sender))
            .await
            .unwrap();
        receiver.await.unwrap()
    }

    async fn handle_connect_request(
        node: &mut ReticulumNode,
        ctx: &mut Context,
        hash: DestinationHash,
        sender: tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    ) {
        todo!()
    }
    async fn handle_listener_request(
        node: &mut ReticulumNode,
        ctx: &mut Context,
        destination: Destination,
        sender: tokio::sync::oneshot::Sender<Result<Listener>>,
    ) {
        todo!()
    }
    async fn handle_to_send(
        node: &mut ReticulumNode,
        ctx: &mut Context,
        link: LinkId,
        data: Vec<u8>,
    ) {
        todo!()
    }
    async fn handle_event(node: &mut ReticulumNode, event: NodeEvent, listeners: &mut Context) {
        match event {
            NodeEvent::AnnounceReceived {
                announce,
                interface_index,
            } => todo!(),
            NodeEvent::PathFound {
                destination_hash,
                hops,
                interface_index,
            } => todo!(),
            NodeEvent::PathRequestReceived { destination_hash } => todo!(),
            NodeEvent::PathLost { destination_hash } => todo!(),
            NodeEvent::PacketReceived {
                destination,
                data,
                interface_index,
            } => todo!(),
            NodeEvent::PacketDeliveryConfirmed { packet_hash } => todo!(),
            NodeEvent::DeliveryFailed { packet_hash, error } => todo!(),
            NodeEvent::LinkRequest {
                link_id,
                destination_hash,
                peer_keys,
            } => {
                // Establish Link
            }
            NodeEvent::LinkEstablished {
                link_id,
                is_initiator,
            } => {
                // Build ReticulumStream
                // Send ReticulumStream to listener
            }
            NodeEvent::MessageReceived {
                link_id,
                msgtype,
                sequence,
                data,
            } => todo!(),
            NodeEvent::LinkDataReceived { link_id, data } => todo!(),
            NodeEvent::LinkStale { link_id } => todo!(),
            NodeEvent::LinkRecovered { link_id } => todo!(),
            NodeEvent::ChannelRetransmit {
                link_id,
                sequence,
                tries,
            } => todo!(),
            NodeEvent::LinkIdentified {
                link_id,
                identity_hash,
            } => todo!(),
            NodeEvent::LinkClosed {
                link_id,
                reason,
                is_initiator,
                destination_hash,
            } => todo!(),
            NodeEvent::PacketProofRequested {
                packet_hash,
                destination_hash,
            } => todo!(),
            NodeEvent::LinkProofRequested {
                link_id,
                packet_hash,
            } => todo!(),
            NodeEvent::LinkDeliveryConfirmed {
                link_id,
                packet_hash,
            } => todo!(),
            NodeEvent::ResourceAdvertised {
                link_id,
                resource_hash,
                transfer_size,
                data_size,
            } => todo!(),
            NodeEvent::ResourceTransferStarted {
                link_id,
                resource_hash,
                is_sender,
            } => todo!(),
            NodeEvent::ResourceProgress {
                link_id,
                resource_hash,
                progress,
                transfer_size,
                data_size,
                is_sender,
            } => todo!(),
            NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                data,
                metadata,
                is_sender,
                segment_index,
                total_segments,
            } => todo!(),
            NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error,
                is_sender,
            } => todo!(),
            NodeEvent::RequestReceived {
                link_id,
                request_id,
                path,
                path_hash,
                data,
                requested_at,
            } => todo!(),
            NodeEvent::ResponseReceived {
                link_id,
                request_id,
                response_data,
            } => todo!(),
            NodeEvent::RequestTimedOut {
                link_id,
                request_id,
            } => todo!(),
            NodeEvent::InterfaceDown(_) => todo!(),
            _ => todo!(),
        }
    }
}

impl AsyncRead for ReticulumStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.received.poll_recv(cx) {
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
        match self.to_send.try_send((self.link, data)) {
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
        self.received.close();
        std::task::Poll::Ready(Ok(()))
    }
}
