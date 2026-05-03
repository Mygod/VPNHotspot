mod control;
mod dns;
mod nat66;
mod session;
mod socket;
mod upstream;

use std::env;
use std::io;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let socket_name = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing socket name"))?;
    if let Some(arg) = args.next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unexpected argument {arg}"),
        ));
    }
    control::run(socket_name).await
}
