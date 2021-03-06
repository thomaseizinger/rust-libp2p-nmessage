use libp2p::core::connection::ConnectionId;
use libp2p::core::{upgrade, ConnectedPoint, Multiaddr, UpgradeInfo};
use libp2p::futures::future::BoxFuture;
use libp2p::futures::task::{Context, Poll};
use libp2p::futures::FutureExt;
use libp2p::swarm::protocols_handler::OutboundUpgradeSend;
use libp2p::swarm::{
    KeepAlive, NegotiatedSubstream, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler,
    PollParameters, ProtocolsHandler, ProtocolsHandlerEvent, ProtocolsHandlerUpgrErr,
    SubstreamProtocol,
};
use libp2p::{InboundUpgrade, OutboundUpgrade, PeerId};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::future::{Future, Ready};
use std::{io, iter, mem};

type Protocol<T, E> = BoxFuture<'static, Result<T, E>>;
type InboundProtocolFn<I, E> = Box<dyn FnOnce(InboundSubstream) -> Protocol<I, E> + Send + 'static>;
type OutboundProtocolFn<O, E> =
    Box<dyn FnOnce(OutboundSubstream) -> Protocol<O, E> + Send + 'static>;

enum InboundProtocolState<T, E> {
    GotFunctionNeedSubstream(InboundProtocolFn<T, E>),
    GotSubstreamNeedFunction(InboundSubstream),
    Executing(Protocol<T, E>),
}

enum OutboundProtocolState<T, E> {
    GotFunctionNeedSubstream(OutboundProtocolFn<T, E>),
    GotFunctionRequestedSubstream(OutboundProtocolFn<T, E>),
    Executing(Protocol<T, E>),
}

enum ProtocolState<I, O, E> {
    None,
    Inbound(InboundProtocolState<I, E>),
    Outbound(OutboundProtocolState<O, E>),
    Done,
    Poisoned,
}

pub struct Handler<TInboundOut, TOutboundOut, TErr> {
    state: ProtocolState<TInboundOut, TOutboundOut, TErr>,
    info: &'static [u8],
}

impl<TInboundOut, TOutboundOut, TErr> Handler<TInboundOut, TOutboundOut, TErr> {
    pub fn new(info: &'static [u8]) -> Self {
        Self {
            state: ProtocolState::None,
            info,
        }
    }
}

pub struct ProtocolInfo {
    info: &'static [u8],
}

impl ProtocolInfo {
    fn new(info: &'static [u8]) -> Self {
        Self { info }
    }
}

impl UpgradeInfo for ProtocolInfo {
    type Info = &'static [u8];
    type InfoIter = iter::Once<&'static [u8]>;

    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(self.info)
    }
}

pub struct InboundSubstream(NegotiatedSubstream);

pub struct OutboundSubstream(NegotiatedSubstream);

macro_rules! impl_read_write {
    ($t:ty) => {
        impl $t {
            pub async fn write_message(&mut self, msg: &[u8]) -> Result<(), io::Error> {
                upgrade::write_with_len_prefix(&mut self.0, msg).await
            }

            pub async fn read_message(
                &mut self,
                max_size: usize,
            ) -> Result<Vec<u8>, upgrade::ReadOneError> {
                upgrade::read_one(&mut self.0, max_size).await
            }
        }
    };
}

impl_read_write!(InboundSubstream);
impl_read_write!(OutboundSubstream);

impl InboundUpgrade<NegotiatedSubstream> for ProtocolInfo {
    type Output = InboundSubstream;
    type Error = Infallible;
    type Future = Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, socket: NegotiatedSubstream, _: Self::Info) -> Self::Future {
        std::future::ready(Ok(InboundSubstream(socket)))
    }
}

impl OutboundUpgrade<NegotiatedSubstream> for ProtocolInfo {
    type Output = OutboundSubstream;
    type Error = Infallible;
    type Future = Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, socket: NegotiatedSubstream, _: Self::Info) -> Self::Future {
        std::future::ready(Ok(OutboundSubstream(socket)))
    }
}

pub enum ProtocolInEvent<I, O, E> {
    ExecuteInbound(InboundProtocolFn<I, E>),
    ExecuteOutbound(OutboundProtocolFn<O, E>),
}

pub enum ProtocolOutEvent<I, O, E> {
    Inbound(Result<I, E>),
    Outbound(Result<O, E>),
}

impl<TInboundOut, TOutboundOut, TErr> ProtocolsHandler for Handler<TInboundOut, TOutboundOut, TErr>
where
    TInboundOut: Send + 'static,
    TOutboundOut: Send + 'static,
    TErr: Send + 'static,
{
    type InEvent = ProtocolInEvent<TInboundOut, TOutboundOut, TErr>;
    type OutEvent = ProtocolOutEvent<TInboundOut, TOutboundOut, TErr>;
    type Error = Infallible;
    type InboundProtocol = ProtocolInfo;
    type OutboundProtocol = ProtocolInfo;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(ProtocolInfo::new(self.info), ())
    }

    fn inject_fully_negotiated_inbound(
        &mut self,
        substream: InboundSubstream,
        _: Self::InboundOpenInfo,
    ) {
        match mem::replace(&mut self.state, ProtocolState::Poisoned) {
            ProtocolState::None => {
                self.state = ProtocolState::Inbound(
                    InboundProtocolState::GotSubstreamNeedFunction(substream),
                );
            }
            ProtocolState::Inbound(InboundProtocolState::GotFunctionNeedSubstream(protocol_fn)) => {
                self.state =
                    ProtocolState::Inbound(InboundProtocolState::Executing(protocol_fn(substream)));
            }
            ProtocolState::Inbound(_) | ProtocolState::Done => {
                panic!("Illegal state, substream is already present.");
            }
            ProtocolState::Outbound(_) => {
                panic!("Failed to process inbound substream in outbound protocol.");
            }
            ProtocolState::Poisoned => {
                panic!("Illegal state, currently in transient state poisoned.");
            }
        }
    }

    fn inject_fully_negotiated_outbound(
        &mut self,
        substream: OutboundSubstream,
        _: Self::OutboundOpenInfo,
    ) {
        match mem::replace(&mut self.state, ProtocolState::Poisoned) {
            ProtocolState::Outbound(OutboundProtocolState::GotFunctionRequestedSubstream(
                protocol_fn,
            )) => {
                self.state = ProtocolState::Outbound(OutboundProtocolState::Executing(
                    protocol_fn(substream),
                ));
            }
            ProtocolState::None
            | ProtocolState::Outbound(OutboundProtocolState::GotFunctionNeedSubstream(_)) => {
                panic!("Illegal state, receiving substream means it was requested.");
            }
            ProtocolState::Outbound(_) | ProtocolState::Done => {
                panic!("Illegal state, substream is already present.");
            }
            ProtocolState::Inbound(_) => {
                panic!("Failed to process outbound substream in inbound protocol.");
            }
            ProtocolState::Poisoned => {
                panic!("Illegal state, currently in transient state poisoned.");
            }
        }
    }

    fn inject_event(&mut self, event: Self::InEvent) {
        match event {
            ProtocolInEvent::ExecuteInbound(protocol_fn) => {
                match mem::replace(&mut self.state, ProtocolState::Poisoned) {
                    ProtocolState::None => {
                        self.state = ProtocolState::Inbound(
                            InboundProtocolState::GotFunctionNeedSubstream(protocol_fn),
                        );
                    }
                    ProtocolState::Inbound(InboundProtocolState::GotSubstreamNeedFunction(
                        substream,
                    )) => {
                        self.state = ProtocolState::Inbound(InboundProtocolState::Executing(
                            protocol_fn(substream),
                        ));
                    }
                    ProtocolState::Inbound(_) | ProtocolState::Done => {
                        panic!("Illegal state, protocol fn is already present.");
                    }
                    ProtocolState::Outbound(_) => {
                        panic!("Failed to process inbound protocol fn in outbound protocol.");
                    }
                    ProtocolState::Poisoned => {
                        panic!("Illegal state, currently in transient state poisoned.");
                    }
                }
            }
            ProtocolInEvent::ExecuteOutbound(protocol_fn) => {
                match mem::replace(&mut self.state, ProtocolState::Poisoned) {
                    ProtocolState::None => {
                        self.state = ProtocolState::Outbound(
                            OutboundProtocolState::GotFunctionNeedSubstream(protocol_fn),
                        );
                    }
                    ProtocolState::Outbound(_) | ProtocolState::Done => {
                        panic!("Illegal state, protocol fn is already present.");
                    }
                    ProtocolState::Inbound(_) => {
                        panic!("Failed to process outbound protocol fn in inbound protocol.");
                    }
                    ProtocolState::Poisoned => {
                        panic!("Illegal state, currently in transient state poisoned.");
                    }
                }
            }
        }
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _: Self::OutboundOpenInfo,
        err: ProtocolsHandlerUpgrErr<<Self::OutboundProtocol as OutboundUpgradeSend>::Error>,
    ) {
        log::error!("Failed to upgrade: {}", err);
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    #[allow(clippy::type_complexity)]
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ProtocolsHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        match mem::replace(&mut self.state, ProtocolState::Poisoned) {
            ProtocolState::Inbound(InboundProtocolState::Executing(mut protocol)) => match protocol
                .poll_unpin(cx)
            {
                Poll::Ready(res) => {
                    self.state = ProtocolState::Done;
                    Poll::Ready(ProtocolsHandlerEvent::Custom(ProtocolOutEvent::Inbound(
                        res,
                    )))
                }
                Poll::Pending => {
                    self.state = ProtocolState::Inbound(InboundProtocolState::Executing(protocol));
                    Poll::Pending
                }
            },
            ProtocolState::Outbound(OutboundProtocolState::Executing(mut protocol)) => {
                match protocol.poll_unpin(cx) {
                    Poll::Ready(res) => {
                        self.state = ProtocolState::Done;
                        Poll::Ready(ProtocolsHandlerEvent::Custom(ProtocolOutEvent::Outbound(
                            res,
                        )))
                    }
                    Poll::Pending => {
                        self.state =
                            ProtocolState::Outbound(OutboundProtocolState::Executing(protocol));
                        Poll::Pending
                    }
                }
            }
            ProtocolState::Outbound(OutboundProtocolState::GotFunctionNeedSubstream(protocol)) => {
                self.state = ProtocolState::Outbound(
                    OutboundProtocolState::GotFunctionRequestedSubstream(protocol),
                );
                Poll::Ready(ProtocolsHandlerEvent::OutboundSubstreamRequest {
                    protocol: SubstreamProtocol::new(ProtocolInfo::new(self.info), ()),
                })
            }
            ProtocolState::Poisoned => {
                unreachable!("Protocol is poisoned (transient state)")
            }
            other => {
                self.state = other;
                Poll::Pending
            }
        }
    }
}

/// A behaviour that can execute await/.async protocols.
///
/// Note: It is not possible to execute the same protocol with the same peer several simultaneous times.
pub struct Behaviour<I, O, E> {
    protocol_in_events: VecDeque<(PeerId, ProtocolInEvent<I, O, E>)>,
    protocol_out_events: VecDeque<(PeerId, ProtocolOutEvent<I, O, E>)>,

    connected_peers: HashMap<PeerId, Vec<Multiaddr>>,

    info: &'static [u8],
}

impl<I, O, E> Behaviour<I, O, E> {
    /// Constructs a new [`Behaviour`] with the given protocol info.
    ///
    /// # Example
    ///
    /// ```
    /// # use libp2p_async_await::Behaviour;
    ///
    /// let _ = Behaviour::new(b"/foo/bar/1.0.0");
    /// ```
    pub fn new(info: &'static [u8]) -> Self {
        Self {
            protocol_in_events: VecDeque::default(),
            protocol_out_events: VecDeque::default(),
            connected_peers: HashMap::default(),
            info,
        }
    }
}

impl<I, O, E> Behaviour<I, O, E> {
    pub fn do_protocol_listener<F>(
        &mut self,
        peer: PeerId,
        protocol: impl FnOnce(InboundSubstream) -> F + Send + 'static,
    ) where
        F: Future<Output = Result<I, E>> + Send + 'static,
    {
        self.protocol_in_events.push_back((
            peer,
            ProtocolInEvent::ExecuteInbound(Box::new(move |substream| protocol(substream).boxed())),
        ));
    }

    pub fn do_protocol_dialer<F>(
        &mut self,
        peer: PeerId,
        protocol: impl FnOnce(OutboundSubstream) -> F + Send + 'static,
    ) where
        F: Future<Output = Result<O, E>> + Send + 'static,
    {
        self.protocol_in_events.push_back((
            peer,
            ProtocolInEvent::ExecuteOutbound(Box::new(move |substream| {
                protocol(substream).boxed()
            })),
        ));
    }
}

#[derive(Clone)]
pub enum BehaviourOutEvent<I, O, E> {
    Inbound(PeerId, Result<I, E>),
    Outbound(PeerId, Result<O, E>),
}

impl<I, O, E> NetworkBehaviour for Behaviour<I, O, E>
where
    I: Send + 'static,
    O: Send + 'static,
    E: Send + 'static,
{
    type ProtocolsHandler = Handler<I, O, E>;
    type OutEvent = BehaviourOutEvent<I, O, E>;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        Handler::new(self.info)
    }

    fn addresses_of_peer(&mut self, peer: &PeerId) -> Vec<Multiaddr> {
        self.connected_peers.get(peer).cloned().unwrap_or_default()
    }

    fn inject_connected(&mut self, _: &PeerId) {}

    fn inject_disconnected(&mut self, _: &PeerId) {}

    fn inject_connection_established(
        &mut self,
        peer: &PeerId,
        _: &ConnectionId,
        point: &ConnectedPoint,
    ) {
        let multiaddr = point.get_remote_address().clone();

        self.connected_peers
            .entry(*peer)
            .or_default()
            .push(multiaddr);
    }

    fn inject_connection_closed(
        &mut self,
        peer: &PeerId,
        _: &ConnectionId,
        point: &ConnectedPoint,
    ) {
        let multiaddr = point.get_remote_address();

        self.connected_peers
            .entry(*peer)
            .or_default()
            .retain(|addr| addr != multiaddr);
    }

    fn inject_event(&mut self, peer: PeerId, _: ConnectionId, event: ProtocolOutEvent<I, O, E>) {
        self.protocol_out_events.push_back((peer, event));
    }

    fn poll(
        &mut self,
        _: &mut Context<'_>,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<ProtocolInEvent<I, O, E>, Self::OutEvent>> {
        if let Some((peer, event)) = self.protocol_in_events.pop_front() {
            if !self.connected_peers.contains_key(&peer) {
                self.protocol_in_events.push_back((peer, event));
            } else {
                return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                    peer_id: peer,
                    handler: NotifyHandler::Any,
                    event,
                });
            }
        }

        if let Some((peer, event)) = self.protocol_out_events.pop_front() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(match event {
                ProtocolOutEvent::Inbound(res) => BehaviourOutEvent::Inbound(peer, res),
                ProtocolOutEvent::Outbound(res) => BehaviourOutEvent::Outbound(peer, res),
            }));
        }

        Poll::Pending
    }
}
