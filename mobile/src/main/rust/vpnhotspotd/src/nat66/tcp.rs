use std::io;
use std::net::{SocketAddr, SocketAddrV6, TcpListener};
use std::sync::Arc;

use socket2::SockRef;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{self, DNS_PORT};
use crate::report;
use crate::socket::is_connection_closed;
use crate::upstream::{connect_tcp, TcpConnectError};
use vpnhotspotd::shared::model::{select_upstream_network, SelectedNetwork, SessionConfig};

pub(crate) fn spawn_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    spawn(async move {
        loop {
            select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((socket, client)) => {
                            let config = config.clone();
                            let connection_stop = stop.child_token();
                            spawn(async move {
                                select! {
                                    _ = connection_stop.cancelled() => {}
                                    result = handle_connection(socket, client, config) => if let Err(e) = result {
                                        if is_connection_closed(&e) {
                                            report::stdout!("tcp proxy connection closed: client={client}: {e}");
                                        } else {
                                            report::io("nat66.tcp_connection", e);
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
        if let Err(e) = dns::handle_tcp_connection(inbound, snapshot).await {
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
            log_connection_error("connect", client, destination, selection, &e);
            return Ok(());
        }
        Err(TcpConnectError::Setup(e)) => return Err(e),
    };
    if let Err(e) = relay(inbound, outbound).await {
        if is_connection_closed(&e) {
            log_connection_error("relay", client, destination, selection, &e);
        } else {
            return Err(e);
        }
    }
    Ok(())
}

async fn relay(mut inbound: TokioTcpStream, mut outbound: TokioTcpStream) -> io::Result<()> {
    copy_bidirectional(&mut inbound, &mut outbound)
        .await
        .map(|_| ())
}

fn log_connection_error(
    operation: &str,
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
        "tcp proxy {operation} {outcome}: client={client} destination={destination} network={} role={:?}: {error}",
        selection.network, selection.role
    );
}
