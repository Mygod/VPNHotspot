mod control;
mod dns;
mod model;
mod protocol;
mod ra;
mod session;
mod socket;
mod tcp;
mod udp;
mod upstream;

use std::env;
use std::io;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let mut socket_name = None;
    let mut connection_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = args.next(),
            "--connection-file" => connection_file = args.next().map(PathBuf::from),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg}"),
                ))
            }
        }
    }
    control::run(
        socket_name
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --socket-name"))?,
        connection_file.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing --connection-file")
        })?,
    )
    .await
}
