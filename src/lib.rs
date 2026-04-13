#[forbid(unsafe_code)]
use anyhow::{Result, anyhow};
use reticulum_core::link::LinkState;
use reticulum_std::{Destination, DestinationHash, LinkHandle, LinkId, NodeEvent, ReticulumNode};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    select,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, warn};

#[allow(unused)]
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

#[derive(Debug)]
pub struct Listener {
    streams: tokio::sync::mpsc::Receiver<ReticulumStream>,
}

impl Listener {
    #[instrument(skip(self))]
    pub async fn listen(&mut self) -> Option<ReticulumStream> {
        self.streams.recv().await
    }
}

#[derive(Debug)]
pub struct Connector {
    request: tokio::sync::mpsc::Sender<(
        DestinationHash,
        tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    )>,
}

impl Connector {
    #[instrument(skip(self))]
    pub async fn connect(self, hash: DestinationHash) -> Result<ReticulumStream> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.request.send((hash, sender)).await.unwrap();
        trace!("sent connect request to event channel");
        receiver.await?
    }
}

#[derive(Debug)]
pub struct ReticulumStream {
    received: tokio::sync::mpsc::Receiver<Vec<u8>>,
    to_send: tokio::sync::mpsc::Sender<(LinkId, Vec<u8>)>,
    link: LinkId,
}

#[allow(unused)]
struct Context {
    listeners: HashMap<DestinationHash, tokio::sync::mpsc::Sender<ReticulumStream>>,
    to_send_sender: tokio::sync::mpsc::Sender<(LinkId, Vec<u8>)>,
    received: HashMap<LinkId, tokio::sync::mpsc::Sender<Vec<u8>>>,
    cancel: CancellationToken,
    connect_requests: HashMap<LinkId, ConnectionRequest>,
    link_states: HashMap<LinkId, LinkState>,
    announce: HashSet<DestinationHash>,
}

struct ConnectionRequest {
    handle: LinkHandle,
    response: tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    hash: DestinationHash,
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
            connect_requests: Default::default(),
            to_send_sender,
            cancel: cancel.clone(),
            link_states: Default::default(),
            announce: Default::default(),
        };

        let event_loop = tokio::spawn(async move {
            let mut receiver = node
                .take_event_receiver()
                .expect("this is the only instance");
            loop {
                let result = select! {
                    _ = cancel_task.cancelled() => {
                        break;
                    }
                    event = receiver.recv() => {
                        if let Some(event) = event {
                            Self::handle_event(&mut node,event, &mut context).await
                        } else {
                            info!("event loop receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    request = connect_requests.recv() => {
                        if let Some((hash, sender)) = request {
                            Self::handle_connect_request(&mut node, &mut context, hash, sender).await
                        } else {
                            info!("connect_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    request = listener_requests.recv() => {
                        if let Some((hash, sender)) = request {
                            Self::handle_listener_request(&mut node, &mut context, hash, sender).await
                        } else {
                            info!("listener_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    to_send = to_send.recv() => {
                        if let Some((link, data)) = to_send {
                            Self::handle_to_send(&mut node, &mut context, link, data).await
                        } else {
                            info!("listener_requests receiver ended. Shutting down");
                            cancel_task.cancel();
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        for destination in &context.announce {
                            node.announce_destination(&destination, None).await.unwrap();
                        }
                        Ok(())
                    }
                };
                if let Err(error) = result {
                    error!("{:#}", error);
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

    #[instrument(skip(self))]
    pub fn connector(&mut self) -> Connector {
        Connector {
            request: self.connect_requests.clone(),
        }
    }

    #[instrument(skip(self, destination), fields(destination.hash = destination.hash().to_string()))]
    pub async fn listener(&mut self, destination: Destination) -> Result<Listener> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.listener_requests
            .send((destination, sender))
            .await
            .unwrap();
        receiver.await.unwrap()
    }

    #[instrument(skip(node, ctx, response))]
    async fn handle_connect_request(
        node: &mut ReticulumNode,
        ctx: &mut Context,
        hash: DestinationHash,
        response: tokio::sync::oneshot::Sender<Result<ReticulumStream>>,
    ) -> Result<()> {
        trace!("handle connect request");
        let Some(keys) = node.get_identity(&hash) else {
            response
                .send(Err(anyhow!(
                    "no announcement received yet for destination {}",
                    hash
                )))
                .map_err(|e| {
                    anyhow!(
                        "error trying to send send error back to connection requestor: {}",
                        e.err().unwrap()
                    )
                })?;
            return Ok(());
        };
        let keys = keys.public_key_bytes();
        let mut signing_key = [0u8; 32];
        signing_key.copy_from_slice(&keys[32..64]);
        trace!("initiating link");
        let result = node.connect(&hash, &signing_key).await;
        let handle = match result {
            Ok(handle) => handle,
            Err(e) => {
                let e = anyhow::Error::from(e);
                response.send(Err(e)).map_err(|e| {
                    anyhow!(
                        "error when trying to send error back to connection requestor: {}",
                        e.err().unwrap()
                    )
                })?;
                return Ok(());
            }
        };
        trace!("link initiated");
        let id = handle.link_id().clone();
        ctx.link_states.insert(id, LinkState::Pending);
        let request = ConnectionRequest {
            handle,
            response: response,
            hash,
        };
        ctx.connect_requests.insert(id, request);
        Ok(())
    }
    #[instrument(skip(node,ctx,destination,response), fields(destination.hash = destination.hash().to_string()))]
    async fn handle_listener_request(
        node: &mut ReticulumNode,
        ctx: &mut Context,
        destination: Destination,
        response: tokio::sync::oneshot::Sender<Result<Listener>>,
    ) -> Result<()> {
        let hash = destination.hash().clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        if ctx.listeners.contains_key(&hash) {
            let error = anyhow!("Listener already exists for destination {}", hash);
            if let Err(error) = response.send(Err(error)) {
                return Err(anyhow!(
                    "error trying to send error response back to listener requestor: {}",
                    error.err().unwrap()
                ));
            }
            return Ok(());
        }
        node.register_destination(destination);
        ctx.announce.insert(hash);
        ctx.listeners.insert(hash, sender);
        let listener = Listener { streams: receiver };
        if let Err(error) = response.send(Ok(listener)) {
            let error = anyhow!(
                "error trying to send error response back to listener requestor: {}",
                error.err().unwrap()
            );
            ctx.listeners.remove(&hash);
            return Err(error);
        }
        Ok(())
    }
    #[instrument(skip(node, _ctx, data))]
    async fn handle_to_send(
        node: &mut ReticulumNode,
        _ctx: &mut Context,
        link_id: LinkId,
        data: Vec<u8>,
    ) -> Result<()> {
        trace!(sent.bytes = data.len(), "sending resource");
        match node
            .send_resource(&link_id, data.as_slice(), None, true)
            .await
        {
            Ok(hash) => {
                let hash = hex::encode(hash);
                trace!(
                    resource.hash = hash,
                    sent.bytes = data.len(),
                    "initiated sending resource",
                );
            }
            Err(e) => return Err(anyhow::Error::from(e).context("sending resource")),
        }
        Ok(())
    }
    #[instrument(skip(node, ctx, event))]
    async fn handle_event(
        node: &mut ReticulumNode,
        event: NodeEvent,
        ctx: &mut Context,
    ) -> Result<()> {
        match event {
            NodeEvent::LinkRequest {
                link_id,
                destination_hash,
                ..
            } => {
                if !ctx.listeners.contains_key(&destination_hash) {
                    debug!("ignoring link request for unregistered destination",);
                    return Ok(());
                }
                let _handle = node.accept_link(&link_id).await.map_err(|e| {
                    anyhow::Error::from(e).context("error when trying to accept link")
                })?;
                ctx.link_states.insert(link_id, LinkState::Handshake);
                let (received_sender, received_receiver) = tokio::sync::mpsc::channel(10);
                let stream = ReticulumStream {
                    received: received_receiver,
                    to_send: ctx.to_send_sender.clone(),
                    link: link_id,
                };
                ctx.received.insert(link_id, received_sender);
                let listener = ctx
                    .listeners
                    .get_mut(&destination_hash)
                    .expect("desination has registered listener");
                if let Err(_e) = listener.send(stream).await {
                    warn!("channel to listener was closed. cleaning up listener");
                    ctx.listeners.remove(&destination_hash);
                };
            }
            NodeEvent::LinkEstablished { link_id, .. } => {
                ctx.link_states.insert(link_id, LinkState::Active);
                if let Some(request) = ctx.connect_requests.remove(&link_id) {
                    debug!(request.destination = %request.hash, "link established for connection request");
                    node.set_resource_strategy(
                        &link_id,
                        reticulum_core::ResourceStrategy::AcceptAll,
                    )?;
                    trace!("set resource strategy for link");
                    let (sender, receiver) = tokio::sync::mpsc::channel(10);
                    let stream = ReticulumStream {
                        to_send: ctx.to_send_sender.clone(),
                        received: receiver,
                        link: link_id,
                    };
                    ctx.received.insert(link_id, sender);
                    request.response.send(Ok(stream)).map_err(|_| {
                        anyhow!(
                            "error trying to send reticulum stream back to connection requestor"
                        )
                    })?;
                } else {
                    trace!("link established");
                    node.set_resource_strategy(
                        &link_id,
                        reticulum_core::ResourceStrategy::AcceptAll,
                    )?;
                    trace!("set resource strategy for link");
                }
            }
            NodeEvent::LinkStale { link_id } => {
                debug!("link has become stale");
                ctx.link_states.insert(link_id, LinkState::Stale);
            }
            NodeEvent::LinkRecovered { link_id } => {
                debug!("link has recovered");
                ctx.link_states.insert(link_id, LinkState::Active);
            }
            NodeEvent::LinkClosed { link_id, .. } => {
                ctx.received.remove(&link_id);
                ctx.link_states.insert(link_id, LinkState::Closed);
                if let Some(mut request) = ctx.connect_requests.remove(&link_id) {
                    request
                        .response
                        .send(Err(anyhow!("link closed")))
                        .map_err(|e| {
                            anyhow!(
                                "could not send response back to connection requestor: {}",
                                e.err().unwrap()
                            )
                        })?;
                    request
                        .handle
                        .close()
                        .await
                        .map_err(|e| anyhow::Error::from(e))?;
                }
            }
            NodeEvent::ResourceAdvertised { link_id, .. } => {
                if !ctx.received.contains_key(&link_id) {
                    debug!("no stream for link. Ignoring resource advertisement")
                }
                node.accept_resource(&link_id).await?;
                trace!("resource accepted",);
            }
            NodeEvent::ResourceTransferStarted { .. } => trace!("resource transfer started",),
            NodeEvent::ResourceProgress { .. } => trace!("resource transfer progress"),
            NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                data,
                is_sender,
                ..
            } => {
                let data_len = data.len();
                let resource_id = hex::encode(&resource_hash);
                if is_sender {
                    trace!("resource transfered complete",);
                    return Ok(());
                }
                let Some(sender) = ctx.received.get_mut(&link_id) else {
                    return Err(anyhow!(
                        "link {}: resource {}: dropping {} received bytes because sender to stream doesn't exist",
                        link_id,
                        resource_id,
                        data_len
                    ));
                };
                let Ok(()) = sender.send(data).await else {
                    return Err(anyhow!(
                        "link {}: resource {}: dropping {} received bytes because stream was dropped",
                        link_id,
                        resource_id,
                        data_len
                    ));
                };
                trace!("resource received",);
            }
            NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error,
                is_sender,
            } => {
                warn!(
                    link.id = %link_id,
                    resource.hash = hex::encode(resource_hash),
                    %error, is_sender,
                    "resource transfer failed"
                );
            }
            NodeEvent::InterfaceDown(id) => warn!(id = id, "interface down"),
            event => trace!(event = ?event, "received event"),
        }
        Ok(())
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
                trace!(
                    received.bytes = data.len(),
                    link.id = %self.link,
                    "reading data from reticulum stream"
                );
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
        let data: Vec<u8> = buf.iter().cloned().collect();
        trace!(link.id = %self.link, "trying to send {} bytes over reticulum stream", data.len());
        match self.to_send.try_send((self.link, data)) {
            Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => std::task::Poll::Ready(Err(
                std::io::Error::new(std::io::ErrorKind::ConnectionReset, "receiver closed"),
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
        if self.received.is_empty() {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending
        }
    }
}
