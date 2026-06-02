use std::io;
use std::net::{SocketAddr, SocketAddrV6, TcpListener};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use socket2::SockRef;
use tokio::io::{copy, AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{self, DNS_PORT};
use crate::report;
use crate::socket::is_connection_closed;
use crate::upstream::{connect_tcp, is_selected_network_missing, UpstreamConnectError};
use vpnhotspotd::shared::model::{
    mac_string, select_upstream_network, SelectedNetwork, SessionConfig,
};
use vpnhotspotd::shared::nat66_counter::{Nat66CounterSource, Nat66Counters};
use vpnhotspotd::shared::protocol::IoErrorReportExt;

pub(crate) fn spawn_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    counters: Nat66Counters,
    dns: dns::CounterSink,
    mac: [u8; 6],
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    spawn(async move {
        loop {
            select! {
                biased;
                _ = stop.cancelled() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((socket, client)) => {
                            let config = config.clone();
                            let counters = counters.clone();
                            let dns = dns.clone();
                            let connection_stop = stop.child_token();
                            spawn(async move {
                                select! {
                                    biased;
                                    _ = connection_stop.cancelled() => {}
                                    result = handle_connection(socket, client, config, counters, dns, mac) => if let Err(e) = result {
                                        if is_connection_closed(&e) {
                                            report::stdout!("tcp proxy connection closed: client={client}: {e}");
                                        } else {
                                            report::io_with_details(
                                                "nat66.tcp_connection",
                                                e,
                                                [
                                                    ("mac", mac_string(&mac)),
                                                    ("client", client.to_string()),
                                                ],
                                            );
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => report::io("nat66.tcp_accept", e),
                    }
                }
            }
        }
    });
    Ok(())
}

async fn handle_connection(
    inbound: TokioTcpStream,
    client: SocketAddr,
    config: Arc<Mutex<SessionConfig>>,
    counters: Nat66Counters,
    dns: dns::CounterSink,
    mac: [u8; 6],
) -> io::Result<()> {
    let destination = match inbound.local_addr()? {
        SocketAddr::V6(destination) => destination,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected ipv6 destination",
            ))
        }
    };
    let snapshot = config.lock().await.clone();
    SockRef::from(&inbound).set_mark(snapshot.reply_mark)?;
    let ipv6_nat = snapshot
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    if *destination.ip() == ipv6_nat.gateway.address() && destination.port() == DNS_PORT {
        if let Err(e) =
            dns::handle_tcp_connection_counted_with_sink(inbound, snapshot, dns, mac).await
        {
            if is_connection_closed(&e) {
                report::stdout!(
                    "tcp proxy dns closed: client={client} destination={destination}: {e}"
                );
            } else {
                return Err(e);
            }
        }
        return Ok(());
    }
    let Some(selection) = select_upstream_network(&snapshot, *destination.ip()) else {
        report::stdout!("tcp proxy connect failed: client={client} destination={destination}: no upstream route");
        return Ok(());
    };
    let outbound = match connect_tcp(selection.network, destination).await {
        Ok(outbound) => outbound,
        Err(UpstreamConnectError::Connect(e)) => {
            log_connection_error("connect", mac, client, destination, selection, &e);
            return Ok(());
        }
        Err(UpstreamConnectError::Setup(e)) if is_selected_network_missing(&e) => {
            log_connection_error("setup", mac, client, destination, selection, &e);
            return Ok(());
        }
        Err(UpstreamConnectError::Setup(e)) => {
            report::io_with_details(
                "nat66.tcp_setup",
                e,
                [
                    ("mac", mac_string(&mac)),
                    ("client", client.to_string()),
                    ("destination", destination.to_string()),
                    ("network", selection.network.to_string()),
                    ("role", format!("{:?}", selection.role)),
                ],
            );
            return Ok(());
        }
    };
    if let Err(e) = counters.add_tcp_connection(mac) {
        report::io("nat66.tcp_counter", e);
    }
    if let Err(e) = relay(
        inbound,
        outbound,
        counters,
        mac,
        client,
        destination,
        selection,
    )
    .await
    {
        if is_connection_closed(&e) {
            log_connection_error("relay", mac, client, destination, selection, &e);
        } else {
            return Err(e);
        }
    }
    Ok(())
}

async fn relay(
    inbound: TokioTcpStream,
    outbound: TokioTcpStream,
    counters: Nat66Counters,
    mac: [u8; 6],
    client: SocketAddr,
    destination: SocketAddrV6,
    selection: SelectedNetwork,
) -> io::Result<()> {
    let (inbound_read, inbound_write) = inbound.into_split();
    let (outbound_read, outbound_write) = outbound.into_split();
    let inbound_to_outbound = RelayContext::new(
        RelayDirection::InboundToOutbound,
        mac,
        client,
        destination,
        selection,
    );
    let outbound_to_inbound = RelayContext::new(
        RelayDirection::OutboundToInbound,
        mac,
        client,
        destination,
        selection,
    );
    tokio::try_join!(
        relay_direction(
            RelayReader::new(inbound_read, inbound_to_outbound),
            RelayWriter::new(
                outbound_write,
                counters.clone(),
                CountedDirection::Sent,
                inbound_to_outbound,
            ),
        ),
        relay_direction(
            RelayReader::new(outbound_read, outbound_to_inbound),
            RelayWriter::new(
                inbound_write,
                counters,
                CountedDirection::Received,
                outbound_to_inbound,
            ),
        ),
    )?;
    Ok(())
}

async fn relay_direction(mut reader: RelayReader, mut writer: RelayWriter) -> io::Result<()> {
    copy(&mut reader, &mut writer).await?;
    writer.shutdown().await
}

struct RelayReader {
    inner: OwnedReadHalf,
    context: RelayContext,
}

struct RelayWriter {
    inner: OwnedWriteHalf,
    counters: Nat66Counters,
    count_direction: CountedDirection,
    context: RelayContext,
}

#[derive(Clone, Copy)]
struct RelayContext {
    direction: RelayDirection,
    mac: [u8; 6],
    client: SocketAddr,
    destination: SocketAddrV6,
    selection: SelectedNetwork,
}

#[derive(Clone, Copy)]
enum CountedDirection {
    Sent,
    Received,
}

#[derive(Clone, Copy)]
enum RelayDirection {
    InboundToOutbound,
    OutboundToInbound,
}

#[derive(Clone, Copy)]
enum RelayOperation {
    Read,
    Write,
    Flush,
    Shutdown,
}

impl RelayReader {
    fn new(inner: OwnedReadHalf, context: RelayContext) -> Self {
        Self { inner, context }
    }
}

impl RelayWriter {
    fn new(
        inner: OwnedWriteHalf,
        counters: Nat66Counters,
        count_direction: CountedDirection,
        context: RelayContext,
    ) -> Self {
        Self {
            inner,
            counters,
            count_direction,
            context,
        }
    }
}

impl RelayDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::InboundToOutbound => "inbound_to_outbound",
            Self::OutboundToInbound => "outbound_to_inbound",
        }
    }
}

impl RelayOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Shutdown => "shutdown",
        }
    }
}

impl RelayContext {
    fn new(
        direction: RelayDirection,
        mac: [u8; 6],
        client: SocketAddr,
        destination: SocketAddrV6,
        selection: SelectedNetwork,
    ) -> Self {
        Self {
            direction,
            mac,
            client,
            destination,
            selection,
        }
    }

    fn report_context(self, operation: RelayOperation) -> &'static str {
        match (self.direction, operation) {
            (RelayDirection::InboundToOutbound, RelayOperation::Read) => {
                "nat66.tcp_relay.inbound_to_outbound.read"
            }
            (RelayDirection::InboundToOutbound, RelayOperation::Write) => {
                "nat66.tcp_relay.inbound_to_outbound.write"
            }
            (RelayDirection::InboundToOutbound, RelayOperation::Flush) => {
                "nat66.tcp_relay.inbound_to_outbound.flush"
            }
            (RelayDirection::InboundToOutbound, RelayOperation::Shutdown) => {
                "nat66.tcp_relay.inbound_to_outbound.shutdown"
            }
            (RelayDirection::OutboundToInbound, RelayOperation::Read) => {
                "nat66.tcp_relay.outbound_to_inbound.read"
            }
            (RelayDirection::OutboundToInbound, RelayOperation::Write) => {
                "nat66.tcp_relay.outbound_to_inbound.write"
            }
            (RelayDirection::OutboundToInbound, RelayOperation::Flush) => {
                "nat66.tcp_relay.outbound_to_inbound.flush"
            }
            (RelayDirection::OutboundToInbound, RelayOperation::Shutdown) => {
                "nat66.tcp_relay.outbound_to_inbound.shutdown"
            }
        }
    }

    fn attach(self, error: io::Error, operation: RelayOperation) -> io::Error {
        error.with_report_context_details(
            self.report_context(operation),
            [
                ("stage", "relay".to_owned()),
                ("direction", self.direction.as_str().to_owned()),
                ("operation", operation.as_str().to_owned()),
                ("mac", mac_string(&self.mac)),
                ("client", self.client.to_string()),
                ("destination", self.destination.to_string()),
                ("network", self.selection.network.to_string()),
                ("role", format!("{:?}", self.selection.role)),
            ],
        )
    }
}

impl AsyncRead for RelayReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let relay_context = self.context;
        Pin::new(&mut self.inner)
            .poll_read(context, buffer)
            .map(|result| result.map_err(|error| relay_context.attach(error, RelayOperation::Read)))
    }
}

impl AsyncWrite for RelayWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let relay_context = self.context;
        match Pin::new(&mut self.inner).poll_write(context, buffer) {
            Poll::Ready(Ok(size)) => {
                if size > 0 {
                    let mac = self.context.mac;
                    let result = match self.count_direction {
                        CountedDirection::Sent => {
                            self.counters
                                .add_sent_bytes(mac, Nat66CounterSource::Tcp, size)
                        }
                        CountedDirection::Received => {
                            self.counters
                                .add_received_bytes(mac, Nat66CounterSource::Tcp, size)
                        }
                    };
                    if let Err(e) = result {
                        report::io("nat66.tcp_counter", e);
                    }
                }
                Poll::Ready(Ok(size))
            }
            result => result.map(|result| {
                result.map_err(|error| relay_context.attach(error, RelayOperation::Write))
            }),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let relay_context = self.context;
        Pin::new(&mut self.inner).poll_flush(context).map(|result| {
            result.map_err(|error| relay_context.attach(error, RelayOperation::Flush))
        })
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let relay_context = self.context;
        Pin::new(&mut self.inner)
            .poll_shutdown(context)
            .map(|result| {
                result.map_err(|error| relay_context.attach(error, RelayOperation::Shutdown))
            })
    }
}

fn log_connection_error(
    operation: &str,
    mac: [u8; 6],
    client: SocketAddr,
    destination: SocketAddrV6,
    selection: SelectedNetwork,
    error: &io::Error,
) {
    let outcome = match error.kind() {
        io::ErrorKind::TimedOut => "timed out",
        _ if is_connection_closed(error) => "closed",
        _ => "failed",
    };
    report::stdout!(
        "tcp proxy {operation} {outcome}: mac={} client={client} destination={destination} network={} role={:?}: {error}",
        mac_string(&mac), selection.network, selection.role
    );
}
