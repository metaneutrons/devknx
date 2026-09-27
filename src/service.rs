// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Long-lived owner for one KNXnet/IP connection and its capture store.
//!
//! This core can run in an independent foreground process with current-user
//! local IPC. Supervised automatic startup is not implemented yet.
//! Connection state is a watch value so late subscribers see the current state.
//! Live frames and router-loss reports use separate broadcast channels. A slow
//! subscriber receives Tokio's structured `Lagged(count)` error; that local
//! loss is distinct from a router's `RoutingLostMessage` report.

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::time::Duration;

use knx_rs_ip::{ConnectionSpec, KnxConnection, KnxReceiveEvent, connect};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::capture::{CaptureEndpoint, CaptureEvent, RoutingLossEvent};
use crate::operations::{OperationRequest, prepare};
use crate::storage::{CaptureStore, StorageError};

/// Current lifecycle state of one configured connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// The service has not started.
    Idle,
    /// A connection attempt is running.
    Connecting {
        /// One-based attempt number.
        attempt: u64,
    },
    /// Frames can be received from this endpoint.
    Connected {
        /// Connected KNXnet/IP endpoint.
        endpoint: CaptureEndpoint,
    },
    /// A failed or closed connection will be retried after a delay.
    WaitingRetry {
        /// Cause from the transport, never a claim about bus packet loss.
        reason: String,
        /// Delay before the next attempt.
        delay: Duration,
    },
    /// The caller stopped the service.
    Stopped,
    /// Persistence failed; capture stopped to avoid silently losing durable history.
    StorageFailed {
        /// Storage error text.
        reason: String,
    },
}

/// One live event, emitted only after its optional database commit succeeds.
#[derive(Clone, Debug)]
pub struct LiveCapture {
    /// Monotonic database ID when persistence is enabled.
    pub id: Option<i64>,
    /// Exact raw-preserving capture event.
    pub event: CaptureEvent,
}

/// One router-reported loss, emitted only after its optional database commit.
#[derive(Clone, Debug)]
pub struct LiveRoutingLoss {
    /// Monotonic router-loss history ID when persistence is enabled.
    pub id: Option<i64>,
    /// Router source, device state and reported count.
    pub event: RoutingLossEvent,
}

/// A local operation request sent to the sole connection owner.
pub struct OperationEnvelope {
    /// Intent, never a caller-supplied pre-encoded frame.
    pub request: OperationRequest,
    /// Transmission receipt or validation/transport error.
    pub response: oneshot::Sender<Result<OperationReceipt, String>>,
}

/// A confirmed transport send, not confirmation of actuator state.
#[derive(Clone, Debug)]
pub struct OperationReceipt {
    /// Durable audit ID.
    pub audit_id: i64,
    /// Durable sent-capture ID.
    pub capture_id: i64,
    /// Exact bytes passed to the transport.
    pub raw_cemi: Vec<u8>,
}

/// Reconnection delay policy.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectPolicy {
    initial: Duration,
    maximum: Duration,
}

impl ReconnectPolicy {
    /// Build a bounded exponential-backoff policy.
    ///
    /// # Errors
    ///
    /// Returns an error if either duration is zero or the maximum is smaller.
    pub fn new(initial: Duration, maximum: Duration) -> Result<Self, ServiceError> {
        if initial.is_zero() || maximum < initial {
            return Err(ServiceError::InvalidBackoff);
        }
        Ok(Self { initial, maximum })
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            maximum: Duration::from_secs(30),
        }
    }
}

/// Service configuration or persistence failure.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Backoff would be zero or unbounded in the wrong direction.
    #[error("initial reconnect delay must be positive and at most the maximum")]
    InvalidBackoff,
    /// A capture could not be committed; the service stopped.
    #[error("event persistence failed: {0}")]
    Storage(#[from] StorageError),
}

/// Owns one connection lifecycle, optional SQLite store, and event channels.
pub struct CaptureService {
    spec: ConnectionSpec,
    endpoint: CaptureEndpoint,
    store: Option<CaptureStore>,
    policy: ReconnectPolicy,
    state_tx: watch::Sender<ConnectionState>,
    frames_tx: broadcast::Sender<LiveCapture>,
    routing_losses_tx: broadcast::Sender<LiveRoutingLoss>,
    operations_tx: mpsc::Sender<OperationEnvelope>,
    operations_rx: mpsc::Receiver<OperationEnvelope>,
}

impl CaptureService {
    /// Construct a service with a bounded live-event queue.
    #[must_use]
    pub fn new(
        spec: ConnectionSpec,
        store: Option<CaptureStore>,
        policy: ReconnectPolicy,
        event_capacity: NonZeroUsize,
    ) -> Self {
        let endpoint = CaptureEndpoint::from(spec.clone());
        let (state_tx, _) = watch::channel(ConnectionState::Idle);
        let (frames_tx, _) = broadcast::channel(event_capacity.get());
        let (routing_losses_tx, _) = broadcast::channel(event_capacity.get());
        let (operations_tx, operations_rx) = mpsc::channel(32);
        Self {
            spec,
            endpoint,
            store,
            policy,
            state_tx,
            frames_tx,
            routing_losses_tx,
            operations_tx,
            operations_rx,
        }
    }

    /// Subscribe to the current connection state and future changes.
    #[must_use]
    pub fn subscribe_state(&self) -> watch::Receiver<ConnectionState> {
        self.state_tx.subscribe()
    }

    /// Subscribe to live frames. A lagged receiver gets an explicit count.
    #[must_use]
    pub fn subscribe_frames(&self) -> broadcast::Receiver<LiveCapture> {
        self.frames_tx.subscribe()
    }

    /// Subscribe to router-reported routing-frame losses.
    #[must_use]
    pub fn subscribe_routing_losses(&self) -> broadcast::Receiver<LiveRoutingLoss> {
        self.routing_losses_tx.subscribe()
    }

    /// A bounded channel to request validated operations from the connection owner.
    #[must_use]
    pub fn operation_sender(&self) -> mpsc::Sender<OperationEnvelope> {
        self.operations_tx.clone()
    }

    async fn execute_operation<C: KnxConnection + Sync>(
        &mut self,
        connection: &C,
        request: OperationRequest,
    ) -> Result<Result<OperationReceipt, String>, ServiceError> {
        let Some(store) = self.store.as_mut() else {
            return Ok(Err(
                "operations require a persistent capture store".to_owned()
            ));
        };
        let address_raw = match &request {
            OperationRequest::Read { address_raw, .. }
            | OperationRequest::TypedWrite { address_raw, .. }
            | OperationRequest::RawWrite { address_raw, .. } => *address_raw,
        };
        let group = store.ets_group(knx_rs_core::address::GroupAddress::from_raw(address_raw))?;
        let prepared = match prepare(request, group.as_ref()) {
            Ok(prepared) => prepared,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let kind = match &prepared.request {
            OperationRequest::Read { .. } => "read",
            OperationRequest::TypedWrite { .. } => "typed_write",
            OperationRequest::RawWrite { .. } => "raw_write",
        };
        let dpt = prepared.dpt.map(|value| value.to_string());
        let raw_cemi = prepared.frame.as_bytes().to_vec();
        let audit_id = store.start_operation_audit(
            "local_ipc",
            kind,
            address_raw,
            dpt.as_deref(),
            &raw_cemi,
        )?;
        if let Err(error) = connection.send(prepared.frame.clone()).await {
            let detail = error.to_string();
            store.finish_operation_audit(audit_id, false, Some(&detail))?;
            return Ok(Err(format!("KNXnet/IP transmission failed: {detail}")));
        }
        store.finish_operation_audit(audit_id, true, None)?;
        let event = CaptureEvent::sent(self.endpoint, prepared.frame);
        let capture_id = store.insert(&event)?;
        let _ = self.frames_tx.send(LiveCapture {
            id: Some(capture_id),
            event,
        });
        Ok(Ok(OperationReceipt {
            audit_id,
            capture_id,
            raw_cemi,
        }))
    }

    /// Run until shutdown, retrying failed or closed connections.
    ///
    /// # Errors
    ///
    /// Returns an error if capture persistence fails. Connection failures are
    /// state transitions and are retried, not returned as terminal errors.
    pub async fn run_until<S>(&mut self, shutdown: S) -> Result<(), ServiceError>
    where
        S: Future<Output = ()> + Send,
    {
        let spec = self.spec.clone();
        self.run_with(move || connect(spec.clone()), shutdown).await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "connection lifecycle and event handling remain in one owner loop"
    )]
    async fn run_with<C, F, Fut, E, S>(
        &mut self,
        mut connector: F,
        shutdown: S,
    ) -> Result<(), ServiceError>
    where
        C: KnxConnection + Sync,
        F: FnMut() -> Fut + Send,
        Fut: Future<Output = Result<C, E>> + Send,
        E: fmt::Display + Send,
        S: Future<Output = ()> + Send,
    {
        let mut shutdown = Box::pin(shutdown);
        let mut attempt = 0_u64;
        let mut delay = self.policy.initial;
        loop {
            attempt = attempt.saturating_add(1);
            self.state_tx
                .send_replace(ConnectionState::Connecting { attempt });
            let connect_attempt = connector();
            tokio::pin!(connect_attempt);
            let connection = loop {
                tokio::select! {
                    biased;
                    () = &mut shutdown => {
                        self.state_tx.send_replace(ConnectionState::Stopped);
                        return Ok(());
                    }
                    operation = self.operations_rx.recv() => {
                        if let Some(operation) = operation {
                            let _ = operation.response.send(Err("KNXnet/IP connection is connecting".to_owned()));
                        }
                    }
                    result = &mut connect_attempt => break result,
                }
            };
            let reason = match connection {
                Ok(mut connection) => {
                    self.state_tx.send_replace(ConnectionState::Connected {
                        endpoint: self.endpoint,
                    });
                    loop {
                        let received = tokio::select! {
                            biased;
                            () = &mut shutdown => {
                                connection.close().await;
                                self.state_tx.send_replace(ConnectionState::Stopped);
                                return Ok(());
                            }
                            operation = self.operations_rx.recv() => {
                                if let Some(operation) = operation {
                                    if operation.response.is_closed() {
                                        continue;
                                    }
                                    match self.execute_operation(&connection, operation.request).await {
                                        Ok(result) => {
                                            let _ = operation.response.send(result);
                                        }
                                        Err(error) => {
                                            connection.close().await;
                                            self.state_tx.send_replace(ConnectionState::StorageFailed {
                                                reason: error.to_string(),
                                            });
                                            return Err(error);
                                        }
                                    }
                                }
                                continue;
                            }
                            event = connection.recv_event() => event,
                        };
                        let Some(received) = received else {
                            connection.close().await;
                            break "connection closed".to_owned();
                        };
                        match received {
                            KnxReceiveEvent::Frame(frame) => {
                                let event = CaptureEvent::received(self.endpoint, frame);
                                let id = match self.store.as_mut() {
                                    Some(store) => match store.insert(&event) {
                                        Ok(id) => Some(id),
                                        Err(error) => {
                                            connection.close().await;
                                            self.state_tx.send_replace(
                                                ConnectionState::StorageFailed {
                                                    reason: error.to_string(),
                                                },
                                            );
                                            return Err(error.into());
                                        }
                                    },
                                    None => None,
                                };
                                let _ = self.frames_tx.send(LiveCapture { id, event });
                            }
                            KnxReceiveEvent::RoutingLostMessage(report) => {
                                let event = RoutingLossEvent::received(self.endpoint, report);
                                let id = match self.store.as_mut() {
                                    Some(store) => match store.insert_routing_loss(&event) {
                                        Ok(id) => Some(id),
                                        Err(error) => {
                                            connection.close().await;
                                            self.state_tx.send_replace(
                                                ConnectionState::StorageFailed {
                                                    reason: error.to_string(),
                                                },
                                            );
                                            return Err(error.into());
                                        }
                                    },
                                    None => None,
                                };
                                let _ = self.routing_losses_tx.send(LiveRoutingLoss { id, event });
                            }
                        }
                        delay = self.policy.initial;
                    }
                }
                Err(error) => error.to_string(),
            };
            self.state_tx
                .send_replace(ConnectionState::WaitingRetry { reason, delay });
            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    biased;
                    () = &mut shutdown => {
                        self.state_tx.send_replace(ConnectionState::Stopped);
                        return Ok(());
                    }
                    operation = self.operations_rx.recv() => {
                        if let Some(operation) = operation {
                            let _ = operation.response.send(Err("KNXnet/IP connection is disconnected".to_owned()));
                        }
                    }
                    () = &mut sleep => break,
                }
            }
            delay = delay.saturating_mul(2).min(self.policy.maximum);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::num::NonZeroU32;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use knx_rs_ip::{DeviceServer, KnxFuture, Result as KnxResult, RoutingLostMessage};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    struct FakeConnection {
        frames: VecDeque<CemiFrame>,
        close_when_empty: bool,
        closed: Arc<AtomicUsize>,
        received: Arc<AtomicUsize>,
    }

    impl KnxConnection for FakeConnection {
        fn send(&self, _frame: CemiFrame) -> KnxFuture<'_, KnxResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn recv(&mut self) -> KnxFuture<'_, Option<CemiFrame>> {
            Box::pin(async {
                if let Some(frame) = self.frames.pop_front() {
                    self.received.fetch_add(1, Ordering::SeqCst);
                    Some(frame)
                } else if self.close_when_empty {
                    None
                } else {
                    std::future::pending().await
                }
            })
        }

        fn close(&mut self) -> KnxFuture<'_, ()> {
            Box::pin(async {
                self.closed.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    struct FakeEventConnection {
        events: VecDeque<KnxReceiveEvent>,
        closed: Arc<AtomicUsize>,
    }

    impl KnxConnection for FakeEventConnection {
        fn send(&self, _frame: CemiFrame) -> KnxFuture<'_, KnxResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn recv(&mut self) -> KnxFuture<'_, Option<CemiFrame>> {
            Box::pin(async { None })
        }

        fn recv_event(&mut self) -> KnxFuture<'_, Option<KnxReceiveEvent>> {
            Box::pin(async {
                match self.events.pop_front() {
                    Some(event) => Some(event),
                    None => std::future::pending().await,
                }
            })
        }

        fn close(&mut self) -> KnxFuture<'_, ()> {
            Box::pin(async {
                self.closed.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    fn frame(value: u8) -> CemiFrame {
        CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x80, value],
        )
    }

    fn spec() -> ConnectionSpec {
        ConnectionSpec::Tunnel("192.0.2.1:3671".parse().unwrap())
    }

    fn router_spec() -> ConnectionSpec {
        ConnectionSpec::Router("224.0.23.12:3671".parse().unwrap())
    }

    fn capacity(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[tokio::test]
    async fn router_loss_is_committed_before_its_distinct_live_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("loss.sqlite");
        let store = CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap();
        let mut service = CaptureService::new(
            router_spec(),
            Some(store),
            ReconnectPolicy::default(),
            capacity(8),
        );
        let mut losses = service.subscribe_routing_losses();
        let mut frames = service.subscribe_frames();
        let closed = Arc::new(AtomicUsize::new(0));
        let report = RoutingLostMessage {
            source: "192.0.2.2:3671".parse().unwrap(),
            device_state: 1,
            lost_messages: 258,
        };
        let connector = {
            let closed = Arc::clone(&closed);
            move || {
                let closed = Arc::clone(&closed);
                async move {
                    Ok::<_, &'static str>(FakeEventConnection {
                        events: VecDeque::from([KnxReceiveEvent::RoutingLostMessage(report)]),
                        closed,
                    })
                }
            }
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        let observer = async {
            let live = losses.recv().await.unwrap();
            assert_eq!(live.id, Some(1));
            assert_eq!(live.event.report(), report);
            let reader = CaptureStore::open_existing(&path).unwrap();
            assert_eq!(
                reader
                    .read_routing_losses_after(0, NonZeroU32::new(10).unwrap())
                    .unwrap()[0]
                    .event
                    .report(),
                report
            );
            assert!(
                reader
                    .read_after(0, NonZeroU32::new(10).unwrap())
                    .unwrap()
                    .is_empty()
            );
            assert!(matches!(
                frames.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
            stop_tx.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.run_with(connector, async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn loopback_router_diagnostic_reaches_durable_service_stream() {
        use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

        // Local unicast injection into the router socket tests the complete
        // parser-to-store path; it is not a physical multicast qualification.
        let port_probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = port_probe.local_addr().unwrap().port();
        drop(port_probe);
        let endpoint = SocketAddrV4::new(Ipv4Addr::new(239, 255, 23, 12), port);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("router.sqlite");
        let store = CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap();
        let mut service = CaptureService::new(
            ConnectionSpec::Router(endpoint.into()),
            Some(store),
            ReconnectPolicy::default(),
            capacity(8),
        );
        let mut states = service.subscribe_state();
        let mut losses = service.subscribe_routing_losses();
        let mut frames = service.subscribe_frames();
        let (stop_tx, stop_rx) = oneshot::channel();
        let observer = async {
            loop {
                states.changed().await.unwrap();
                if matches!(
                    &*states.borrow_and_update(),
                    ConnectionState::Connected { .. }
                ) {
                    break;
                }
            }
            let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            sender
                .send_to(
                    &[0x06, 0x10, 0x05, 0x31, 0x00, 0x0a, 0x03, 0x03, 0x01, 0x02],
                    target,
                )
                .unwrap(); // Invalid structure length must not become an event.
            sender
                .send_to(
                    &[0x06, 0x10, 0x05, 0x31, 0x00, 0x0a, 0x04, 0x03, 0x01, 0x02],
                    target,
                )
                .unwrap();
            let live = losses.recv().await.unwrap();
            assert_eq!(live.id, Some(1));
            assert_eq!(live.event.report().device_state, 3);
            assert_eq!(live.event.report().lost_messages, 258);
            assert_eq!(
                live.event.report().source.ip(),
                sender.local_addr().unwrap().ip()
            );
            assert!(matches!(
                frames.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ));
            let reader = CaptureStore::open_existing(&path).unwrap();
            assert_eq!(
                reader
                    .read_routing_losses_after(0, NonZeroU32::new(10).unwrap())
                    .unwrap()
                    .len(),
                1
            );
            stop_tx.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(5), async {
            tokio::join!(
                service.run_until(async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .expect("router diagnostic reaches service");
        result.unwrap();
    }

    #[tokio::test]
    async fn real_loopback_tunnel_commits_and_replays_across_owner_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("loopback.sqlite");
        let mut originals = Vec::new();
        for (id, value) in [(1, 42), (2, 43)] {
            // A fresh gateway stub avoids platform-specific UDP reset behavior
            // after the previous tunnel closes; the persistent database is shared.
            let server = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap();
            let address = server.local_addr();
            let store = CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap();
            let mut service = CaptureService::new(
                ConnectionSpec::Tunnel(address),
                Some(store),
                ReconnectPolicy::default(),
                capacity(8),
            );
            let mut states = service.subscribe_state();
            let mut frames = service.subscribe_frames();
            let sent = frame(value);
            let original = sent.as_bytes().to_vec();
            let (stop_tx, stop_rx) = oneshot::channel();
            let observer = async {
                loop {
                    states.changed().await.unwrap();
                    if matches!(
                        &*states.borrow_and_update(),
                        ConnectionState::Connected { .. }
                    ) {
                        break;
                    }
                }
                server.send_frame(sent).await.unwrap();
                let live = frames.recv().await.unwrap();
                assert_eq!(live.id, Some(id));
                assert_eq!(live.event.frame().as_bytes(), original);
                stop_tx.send(()).unwrap();
            };
            let (result, ()) = timeout(Duration::from_secs(5), async {
                tokio::join!(
                    service.run_until(async {
                        let _ = stop_rx.await;
                    }),
                    observer
                )
            })
            .await
            .expect("loopback frame arrives");
            result.unwrap();
            assert_eq!(*states.borrow(), ConnectionState::Stopped);
            originals.push(original);
            server.stop().await;
        }
        let history = CaptureStore::open_existing(&path).unwrap();
        assert_eq!(
            history
                .read_after(0, NonZeroU32::new(10).unwrap())
                .unwrap()
                .into_iter()
                .map(|capture| capture.event.frame().as_bytes().to_vec())
                .collect::<Vec<_>>(),
            originals
        );
    }

    #[tokio::test]
    async fn reconnects_after_failure_and_commits_before_broadcast() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sqlite");
        let store = CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap();
        let policy =
            ReconnectPolicy::new(Duration::from_millis(10), Duration::from_millis(20)).unwrap();
        let mut service = CaptureService::new(spec(), Some(store), policy, capacity(8));
        let mut states = service.subscribe_state();
        let mut frames = service.subscribe_frames();
        let attempts = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let original = frame(7);
        let raw = original.as_bytes().to_vec();
        let (stop_tx, stop_rx) = oneshot::channel();
        let connector = {
            let attempts = Arc::clone(&attempts);
            let closed = Arc::clone(&closed);
            let received = Arc::clone(&received);
            move || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let frame = original.clone();
                let closed = Arc::clone(&closed);
                let received = Arc::clone(&received);
                async move {
                    if attempt == 0 {
                        Err("gateway unavailable")
                    } else {
                        Ok(FakeConnection {
                            frames: VecDeque::from([frame]),
                            close_when_empty: false,
                            closed,
                            received,
                        })
                    }
                }
            }
        };
        let observer = async {
            loop {
                states.changed().await.unwrap();
                if let ConnectionState::WaitingRetry { reason, .. } = &*states.borrow() {
                    assert_eq!(reason, "gateway unavailable");
                    break;
                }
            }
            let live = frames.recv().await.unwrap();
            assert_eq!(live.id, Some(1));
            assert_eq!(live.event.frame().as_bytes(), raw);
            stop_tx.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.run_with(connector, async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(*states.borrow(), ConnectionState::Stopped);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(received.load(Ordering::SeqCst), 1);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        drop(service);
        let history = CaptureStore::open_existing(&path).unwrap();
        assert_eq!(
            history.read_after(0, NonZeroU32::new(1).unwrap()).unwrap()[0]
                .event
                .frame()
                .as_bytes(),
            raw
        );
    }

    #[tokio::test]
    async fn closed_connection_enters_retry_state() {
        let policy =
            ReconnectPolicy::new(Duration::from_millis(10), Duration::from_millis(40)).unwrap();
        let mut service = CaptureService::new(spec(), None, policy, capacity(1));
        let mut states = service.subscribe_state();
        let closed = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = oneshot::channel();
        let connector = {
            let closed = Arc::clone(&closed);
            let received = Arc::clone(&received);
            move || {
                let closed = Arc::clone(&closed);
                let received = Arc::clone(&received);
                async move {
                    Ok::<_, &'static str>(FakeConnection {
                        frames: VecDeque::new(),
                        close_when_empty: true,
                        closed,
                        received,
                    })
                }
            }
        };
        let observer = async {
            let mut retry_delays = Vec::new();
            loop {
                states.changed().await.unwrap();
                if let ConnectionState::WaitingRetry { reason, delay } = &*states.borrow() {
                    assert_eq!(reason, "connection closed");
                    retry_delays.push(*delay);
                    if retry_delays.len() == 2 {
                        assert_eq!(
                            retry_delays,
                            [Duration::from_millis(10), Duration::from_millis(20)]
                        );
                        stop_tx.send(()).unwrap();
                        break;
                    }
                }
            }
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.run_with(connector, async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(closed.load(Ordering::SeqCst), 2);
        assert_eq!(*states.borrow(), ConnectionState::Stopped);
    }

    #[tokio::test]
    async fn queued_operation_is_rejected_during_retry_not_sent_after_reconnect() {
        let policy =
            ReconnectPolicy::new(Duration::from_millis(100), Duration::from_millis(100)).unwrap();
        let mut service = CaptureService::new(spec(), None, policy, capacity(1));
        let mut states = service.subscribe_state();
        let operations = service.operation_sender();
        let (stop_tx, stop_rx) = oneshot::channel();
        let connector = || async {
            Ok::<_, &'static str>(FakeConnection {
                frames: VecDeque::new(),
                close_when_empty: true,
                closed: Arc::new(AtomicUsize::new(0)),
                received: Arc::new(AtomicUsize::new(0)),
            })
        };
        let observer = async {
            loop {
                states.changed().await.unwrap();
                if matches!(
                    &*states.borrow_and_update(),
                    ConnectionState::WaitingRetry { .. }
                ) {
                    break;
                }
            }
            let (response, receiver) = oneshot::channel();
            operations
                .send(OperationEnvelope {
                    request: OperationRequest::RawWrite {
                        address_raw: 0x0a03,
                        payload: crate::operations::RawPayload::Inline(1),
                    },
                    response,
                })
                .await
                .unwrap();
            assert!(
                receiver
                    .await
                    .unwrap()
                    .unwrap_err()
                    .contains("disconnected")
            );
            stop_tx.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.run_with(connector, async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn slow_subscriber_gets_explicit_application_lag_count() {
        let mut service =
            CaptureService::new(spec(), None, ReconnectPolicy::default(), capacity(1));
        let mut frames = service.subscribe_frames();
        let closed = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = oneshot::channel();
        let connector = {
            let closed = Arc::clone(&closed);
            let received = Arc::clone(&received);
            move || {
                let closed = Arc::clone(&closed);
                let received = Arc::clone(&received);
                async move {
                    Ok::<_, &'static str>(FakeConnection {
                        frames: VecDeque::from([frame(1), frame(2), frame(3)]),
                        close_when_empty: false,
                        closed,
                        received,
                    })
                }
            }
        };
        let observer = async {
            while received.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
            assert!(matches!(
                frames.recv().await,
                Err(broadcast::error::RecvError::Lagged(2))
            ));
            assert_eq!(
                frames.recv().await.unwrap().event.frame().payload(),
                &[0, 0x80, 3]
            );
            stop_tx.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(
                service.run_with(connector, async {
                    let _ = stop_rx.await;
                }),
                observer
            )
        })
        .await
        .unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn storage_failure_stops_instead_of_silently_dropping_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("readonly.sqlite");
        drop(CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap());
        let readonly = CaptureStore::open_existing(&path).unwrap();
        let mut service = CaptureService::new(
            spec(),
            Some(readonly),
            ReconnectPolicy::default(),
            capacity(1),
        );
        let states = service.subscribe_state();
        let closed = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let connector = {
            let closed = Arc::clone(&closed);
            let received = Arc::clone(&received);
            move || {
                let closed = Arc::clone(&closed);
                let received = Arc::clone(&received);
                async move {
                    Ok::<_, &'static str>(FakeConnection {
                        frames: VecDeque::from([frame(1)]),
                        close_when_empty: false,
                        closed,
                        received,
                    })
                }
            }
        };
        let result = timeout(
            Duration::from_secs(2),
            service.run_with(connector, std::future::pending()),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(ServiceError::Storage(StorageError::ReadOnly))
        ));
        assert!(matches!(
            &*states.borrow(),
            ConnectionState::StorageFailed { .. }
        ));
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rejects_invalid_backoff() {
        assert!(ReconnectPolicy::new(Duration::ZERO, Duration::from_secs(1)).is_err());
        assert!(ReconnectPolicy::new(Duration::from_secs(2), Duration::from_secs(1)).is_err());
        assert!(ReconnectPolicy::new(Duration::from_secs(1), Duration::from_secs(2)).is_ok());
    }
}
