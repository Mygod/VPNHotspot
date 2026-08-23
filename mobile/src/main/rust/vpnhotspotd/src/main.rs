mod app_session;
mod bootstrap;
mod budget;
mod control;
mod dispatch;
mod dns;
mod downstream;
mod echo;
mod echo_session;
mod echo_socket;
mod egress;
mod firewall;
mod flow_setup;
mod gateway;
mod ipsec;
mod mailbox;
mod nat66;
mod neighbour;
mod netlink;
mod output;
mod owned;
mod platform;
mod process_io;
mod reply;
mod report;
mod resolver;
mod routing;
mod send_failure;
mod session;
mod socket;
mod tcp;
mod tcp_device;
mod tcp_dns;
mod tcp_flow;
mod traffic;
mod tun_reader;
mod tun_writer;
mod udp;
mod upstream;
mod virtual_dns;
mod workers;

use std::env;
use std::io;

/// One argument is the root-side control socket. A second argument is a Shizuku-mode bootstrap nonce,
/// which selects the app-UID path instead: that path owns a TUN and no system state, because nothing
/// the root control loop does is permitted at the app UID.
#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let socket_name = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket name"))?;
    let nonce = match args.next() {
        None => return control::run(socket_name).await,
        Some(nonce) => nonce
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad nonce: {e}")))?,
    };
    if let Some(arg) = args.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected argument {arg}"),
        ));
    }
    bootstrap::run(socket_name, nonce).await
}
