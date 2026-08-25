use std::io;

use libc::c_int;
use vpnhotspotd::shared::model::Network;

#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
}

pub(crate) fn set_socket_network(network: Network, fd: c_int) -> io::Result<()> {
    if unsafe { android_setsocknetwork(network, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
