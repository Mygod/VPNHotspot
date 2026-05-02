use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use socket2::SockRef;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{self, DNS_PORT};
use crate::upstream::connect_tcp;
use vpnhotspotd::shared::model::{select_network, SessionConfig};

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
                        Ok((socket, _)) => {
                            let config = config.clone();
                            let connection_stop = stop.child_token();
                            spawn(async move {
                                select! {
                                    _ = connection_stop.cancelled() => {}
                                    result = handle_connection(socket, config) => if let Err(e) = result {
                                        eprintln!("tcp proxy failed: {e}");
                                    }
                                }
                            });
                        }
                        Err(e) => eprintln!("tcp accept failed: {e}"),
                    }
                }
            }
        }
    });
    Ok(())
}

async fn handle_connection(
    inbound: TokioTcpStream,
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
    if destination.ip() == &ipv6_nat.gateway && destination.port() == DNS_PORT {
        return dns::handle_tcp_connection(inbound, snapshot).await;
    }
    let network = select_network(&snapshot, *destination.ip())
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no upstream route"))?;
    let outbound = connect_tcp(network, destination).await?;
    relay(inbound, outbound).await
}

async fn relay(mut inbound: TokioTcpStream, mut outbound: TokioTcpStream) -> io::Result<()> {
    copy_bidirectional(&mut inbound, &mut outbound)
        .await
        .map(|_| ())
}
