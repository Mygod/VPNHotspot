mod android_network;
mod control_wire;
mod report;
mod root;
mod shizuku;
mod socket;

use std::env;
use std::io;

/// One argument is the root-side control socket. `--app-uid` followed by a socket selects the app-UID path
/// instead: that path owns a TUN and no system state, because nothing the root control loop does is permitted
/// at the app UID.
#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let first = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket name"))?;
    let (app_uid, socket_name) = if first == "--app-uid" {
        (
            true,
            args.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing socket name")
            })?,
        )
    } else {
        (false, first)
    };
    if let Some(arg) = args.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected argument {arg}"),
        ));
    }
    if app_uid {
        shizuku::run(socket_name).await
    } else {
        root::run(socket_name).await
    }
}
