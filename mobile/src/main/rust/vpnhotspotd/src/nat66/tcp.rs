use std::io;
use std::net::{SocketAddr, SocketAddrV6, TcpListener};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use socket2::SockRef;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{self, DNS_PORT};
use crate::report;
use crate::socket::is_connection_closed;
use crate::upstream::{connect_tcp, TcpConnectError};
use vpnhotspotd::shared::model::{
    mac_string, select_upstream_network, SelectedNetwork, SessionConfig,
};
use vpnhotspotd::shared::nat66_counter::{Nat66CounterSource, Nat66Counters};

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
        Err(TcpConnectError::Connect(e)) => {
            log_connection_error("connect", mac, client, destination, selection, &e);
            return Ok(());
        }
        Err(TcpConnectError::Setup(e)) => return Err(e),
    };
    if let Err(e) = counters.add_tcp_connection(mac) {
        report::io("nat66.tcp_counter", e);
    }
    if let Err(e) = relay(inbound, outbound, counters, mac).await {
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
) -> io::Result<()> {
    let mut inbound =
        CountedStream::new(inbound, counters.clone(), mac, CountedDirection::Received);
    let mut outbound = CountedStream::new(outbound, counters, mac, CountedDirection::Sent);
    copy_bidirectional(&mut inbound, &mut outbound)
        .await
        .map(|_| ())
}

struct CountedStream {
    inner: TokioTcpStream,
    counters: Nat66Counters,
    mac: [u8; 6],
    direction: CountedDirection,
}

#[derive(Clone, Copy)]
enum CountedDirection {
    Sent,
    Received,
}

impl CountedStream {
    fn new(
        inner: TokioTcpStream,
        counters: Nat66Counters,
        mac: [u8; 6],
        direction: CountedDirection,
    ) -> Self {
        Self {
            inner,
            counters,
            mac,
            direction,
        }
    }
}

impl AsyncRead for CountedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for CountedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(context, buffer) {
            Poll::Ready(Ok(size)) => {
                if size > 0 {
                    let result = match self.direction {
                        CountedDirection::Sent => {
                            self.counters
                                .add_sent_bytes(self.mac, Nat66CounterSource::Tcp, size)
                        }
                        CountedDirection::Received => self.counters.add_received_bytes(
                            self.mac,
                            Nat66CounterSource::Tcp,
                            size,
                        ),
                    };
                    if let Err(e) = result {
                        report::io("nat66.tcp_counter", e);
                    }
                }
                Poll::Ready(Ok(size))
            }
            result => result,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
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
