// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Raw-preserving KNXnet/IP receive stream.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::SystemTime;

use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::message::ApduType;
use knx_rs_ip::{ConnectionSpec, KnxConnection, RoutingLostMessage};

/// Transport and remote endpoint from which a frame was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEndpoint {
    /// Unicast KNXnet/IP tunnel.
    Tunnel(SocketAddr),
    /// Multicast KNXnet/IP routing.
    Router(SocketAddr),
}

impl From<ConnectionSpec> for CaptureEndpoint {
    fn from(spec: ConnectionSpec) -> Self {
        match spec {
            ConnectionSpec::Tunnel(addr) => Self::Tunnel(addr),
            ConnectionSpec::Router(addr) => Self::Router(addr),
        }
    }
}

impl std::fmt::Display for CaptureEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tunnel(addr) => write!(f, "tunnel://{addr}"),
            Self::Router(addr) => write!(f, "router://{addr}"),
        }
    }
}

/// Group-value service decoded from a frame without discarding its raw bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupService {
    /// A group-value read request.
    Read,
    /// A group-value response.
    Response,
    /// A group-value write.
    Write,
    /// The frame has a different or undecodable service.
    Other,
}

/// Direction relative to this application, not a claim of bus delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureDirection {
    /// Received from a KNXnet/IP connection.
    Received,
    /// Locally transmitted; no device response is implied.
    Sent,
}

impl CaptureDirection {
    /// Stable lowercase representation for storage and text output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Sent => "sent",
        }
    }
}

impl std::fmt::Display for CaptureDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One received telegram, with the exact cEMI frame retained for replay and export.
#[derive(Clone, Debug)]
pub struct CaptureEvent {
    observed_at: SystemTime,
    endpoint: CaptureEndpoint,
    direction: CaptureDirection,
    frame: CemiFrame,
}

/// A router-reported loss of KNXnet/IP routing frames, distinct from local
/// subscriber lag and from an unquantified connection interruption.
#[derive(Clone, Debug)]
pub struct RoutingLossEvent {
    observed_at: SystemTime,
    endpoint: CaptureEndpoint,
    report: RoutingLostMessage,
}

impl RoutingLossEvent {
    /// Record a diagnostic received from a multicast router.
    #[must_use]
    pub fn received(endpoint: CaptureEndpoint, report: RoutingLostMessage) -> Self {
        Self {
            observed_at: SystemTime::now(),
            endpoint,
            report,
        }
    }

    pub(crate) const fn from_stored(
        observed_at: SystemTime,
        endpoint: CaptureEndpoint,
        report: RoutingLostMessage,
    ) -> Self {
        Self {
            observed_at,
            endpoint,
            report,
        }
    }

    /// Wall-clock time at which the report was received.
    #[must_use]
    pub const fn observed_at(&self) -> SystemTime {
        self.observed_at
    }

    /// Multicast endpoint on which the report was received.
    #[must_use]
    pub const fn endpoint(&self) -> CaptureEndpoint {
        self.endpoint
    }

    /// Source, device state and count as reported by the router.
    #[must_use]
    pub const fn report(&self) -> RoutingLostMessage {
        self.report
    }
}

impl CaptureEvent {
    /// Construct an inbound event using the current wall-clock time.
    #[must_use]
    pub fn received(endpoint: CaptureEndpoint, frame: CemiFrame) -> Self {
        Self {
            observed_at: SystemTime::now(),
            endpoint,
            direction: CaptureDirection::Received,
            frame,
        }
    }

    /// Restore a validated event from durable storage.
    pub(crate) const fn from_stored(
        observed_at: SystemTime,
        endpoint: CaptureEndpoint,
        direction: CaptureDirection,
        frame: CemiFrame,
    ) -> Self {
        Self {
            observed_at,
            endpoint,
            direction,
            frame,
        }
    }

    /// Wall-clock time at which the application received the frame.
    #[must_use]
    pub const fn observed_at(&self) -> SystemTime {
        self.observed_at
    }

    /// Transport and endpoint associated with this frame.
    #[must_use]
    pub const fn endpoint(&self) -> CaptureEndpoint {
        self.endpoint
    }

    /// Direction relative to this application.
    #[must_use]
    pub const fn direction(&self) -> CaptureDirection {
        self.direction
    }

    /// Parsed cEMI frame; [`CemiFrame::as_bytes`] returns its original wire bytes.
    #[must_use]
    pub const fn frame(&self) -> &CemiFrame {
        &self.frame
    }

    /// Classify the group-value service, leaving other telegrams unmodified.
    #[must_use]
    pub fn group_service(&self) -> GroupService {
        let apdu_type = self
            .frame
            .tpdu()
            .and_then(|tpdu| tpdu.apdu().map(|apdu| apdu.apdu_type));
        match apdu_type {
            Some(ApduType::GroupValueRead) => GroupService::Read,
            Some(ApduType::GroupValueResponse) => GroupService::Response,
            Some(ApduType::GroupValueWrite) => GroupService::Write,
            _ => GroupService::Other,
        }
    }
}

/// Reason the receive loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureExit {
    /// The caller requested a graceful stop.
    Stopped,
    /// The connection closed without a stop request.
    Disconnected,
}

/// Receive frames until shutdown or disconnection and forward each event to a sink.
///
/// The connection is closed on every exit path, including a sink or signal error.
/// A closed connection is reported distinctly so the caller cannot mistake it
/// for a successful monitoring session.
///
/// # Errors
///
/// Returns an I/O error from the event sink or shutdown signal.
pub async fn capture_until<C, S, F>(
    connection: &mut C,
    endpoint: CaptureEndpoint,
    mut sink: S,
    shutdown: F,
) -> io::Result<CaptureExit>
where
    C: KnxConnection,
    S: FnMut(CaptureEvent) -> io::Result<()>,
    F: Future<Output = io::Result<()>>,
{
    let mut shutdown = Box::pin(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            signal = &mut shutdown => break signal.map(|()| CaptureExit::Stopped),
            frame = connection.recv() => {
                match frame {
                    Some(frame) => {
                        if let Err(error) = sink(CaptureEvent::received(endpoint, frame)) {
                            break Err(error);
                        }
                    }
                    None => break Ok(CaptureExit::Disconnected),
                }
            }
        }
    };
    connection.close().await;
    result
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::knxip::KnxIpFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use knx_rs_ip::{KnxFuture, Result as KnxResult, connect, tunnel_server::DeviceServer};
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    struct FakeConnection {
        frames: VecDeque<CemiFrame>,
        closed: Arc<AtomicBool>,
    }

    impl KnxConnection for FakeConnection {
        fn send(&self, _frame: CemiFrame) -> KnxFuture<'_, KnxResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn recv(&mut self) -> KnxFuture<'_, Option<CemiFrame>> {
            Box::pin(async { self.frames.pop_front() })
        }

        fn close(&mut self) -> KnxFuture<'_, ()> {
            Box::pin(async { self.closed.store(true, Ordering::SeqCst) })
        }
    }

    fn frame(payload: &[u8]) -> CemiFrame {
        CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            payload,
        )
    }

    #[tokio::test]
    async fn forwards_wire_bytes_and_reports_disconnect() {
        let expected = frame(&[0x00, 0x80, 0x01]);
        let original = expected.as_bytes().to_vec();
        let closed = Arc::new(AtomicBool::new(false));
        let mut connection = FakeConnection {
            frames: VecDeque::from([expected]),
            closed: Arc::clone(&closed),
        };
        let endpoint = CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap());
        let mut events = Vec::new();

        let exit = capture_until(
            &mut connection,
            endpoint,
            |event| {
                events.push(event);
                Ok(())
            },
            std::future::pending(),
        )
        .await
        .unwrap();

        assert_eq!(exit, CaptureExit::Disconnected);
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].frame().as_bytes(), original);
        assert_eq!(events[0].endpoint(), endpoint);
        assert_eq!(events[0].group_service(), GroupService::Write);
        assert!(events[0].observed_at() <= SystemTime::now());
    }

    #[tokio::test]
    async fn stops_cleanly_without_consuming_a_frame() {
        let closed = Arc::new(AtomicBool::new(false));
        let mut connection = FakeConnection {
            frames: VecDeque::from([frame(&[0x00, 0x00])]),
            closed: Arc::clone(&closed),
        };
        let endpoint = CaptureEndpoint::Router("224.0.23.12:3671".parse().unwrap());
        let exit = capture_until(
            &mut connection,
            endpoint,
            |_| panic!("unexpected event"),
            async { Ok(()) },
        )
        .await
        .unwrap();

        assert_eq!(exit, CaptureExit::Stopped);
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn closes_connection_after_sink_failure() {
        let closed = Arc::new(AtomicBool::new(false));
        let mut connection = FakeConnection {
            frames: VecDeque::from([frame(&[0x00, 0x40])]),
            closed: Arc::clone(&closed),
        };
        let endpoint = CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap());
        let result = capture_until(
            &mut connection,
            endpoint,
            |_| Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed")),
            std::future::pending(),
        )
        .await;

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(closed.load(Ordering::SeqCst));
    }

    #[test]
    fn classifies_group_read_response_and_write() {
        let endpoint = CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap());
        for (payload, service) in [
            (&[0x00, 0x00][..], GroupService::Read),
            (&[0x00, 0x40][..], GroupService::Response),
            (&[0x00, 0x80][..], GroupService::Write),
        ] {
            assert_eq!(
                CaptureEvent::received(endpoint, frame(payload)).group_service(),
                service
            );
        }
    }

    #[tokio::test]
    async fn captures_a_real_loopback_tunnel_frame() {
        let server = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let address = server.local_addr();
        let mut connection = connect(ConnectionSpec::Tunnel(address)).await.unwrap();
        let sent = frame(&[0x00, 0x40, 0x01]);
        let original = sent.as_bytes().to_vec();
        let (stop_tx, stop_rx) = oneshot::channel();
        let mut stop_tx = Some(stop_tx);
        let mut events = Vec::new();
        server.send_frame(sent).await.unwrap();

        let exit = timeout(
            Duration::from_secs(5),
            capture_until(
                &mut connection,
                CaptureEndpoint::Tunnel(address),
                |event| {
                    events.push(event);
                    if let Some(sender) = stop_tx.take() {
                        let _ = sender.send(());
                    }
                    Ok(())
                },
                async {
                    stop_rx
                        .await
                        .map_err(|_| io::Error::other("stop signal dropped"))
                },
            ),
        )
        .await
        .expect("loopback frame arrives")
        .expect("capture succeeds");

        assert_eq!(exit, CaptureExit::Stopped);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].frame().as_bytes(), original);
        assert_eq!(events[0].group_service(), GroupService::Response);
        server.stop().await;
    }

    #[tokio::test]
    async fn router_ignores_malformed_datagrams_and_preserves_group_services() {
        use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

        // The router joins a multicast group, but the fixture injects into its
        // bound UDP port over loopback so it also works on CI hosts with no
        // multicast route. A physical multicast route remains a hardware gate.
        let port_probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = port_probe.local_addr().unwrap().port();
        drop(port_probe);
        let multicast = SocketAddrV4::new(Ipv4Addr::new(239, 255, 23, 12), port);
        let endpoint = CaptureEndpoint::Router(multicast.into());
        let mut connection = connect(ConnectionSpec::Router(multicast.into()))
            .await
            .unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let malformed_cemi = KnxIpFrame::routing_indication(&[0]);
        sender.send_to(&[0x06, 0x10, 0x05], target).unwrap();
        sender
            .send_to(&malformed_cemi.try_to_bytes().unwrap(), target)
            .unwrap();

        let mut originals = Vec::new();
        for payload in [[0x00, 0x00], [0x00, 0x40], [0x00, 0x80]] {
            let cemi = frame(&payload);
            originals.push(cemi.as_bytes().to_vec());
            let routing = KnxIpFrame::routing_indication(cemi.as_bytes());
            sender
                .send_to(&routing.try_to_bytes().unwrap(), target)
                .unwrap();
        }

        let mut events = Vec::new();
        let (stop_tx, stop_rx) = oneshot::channel();
        let mut stop_tx = Some(stop_tx);
        let exit = timeout(
            Duration::from_secs(5),
            capture_until(
                &mut connection,
                endpoint,
                |event| {
                    events.push(event);
                    if events.len() == 3
                        && let Some(sender) = stop_tx.take()
                    {
                        let _ = sender.send(());
                    }
                    Ok(())
                },
                async {
                    stop_rx
                        .await
                        .map_err(|_| io::Error::other("stop signal dropped"))
                },
            ),
        )
        .await
        .expect("router frames arrive")
        .expect("router capture succeeds");
        assert_eq!(exit, CaptureExit::Stopped);
        assert_eq!(events.len(), 3);
        for (index, (event, original)) in events.iter().zip(originals).enumerate() {
            assert_eq!(event.frame().as_bytes(), original);
            assert_eq!(event.endpoint(), endpoint);
            assert_eq!(
                event.group_service(),
                [
                    GroupService::Read,
                    GroupService::Response,
                    GroupService::Write
                ][index]
            );
        }
    }
}
