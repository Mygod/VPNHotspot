mod android_network;
mod control_wire;
mod report;
mod root;
mod shizuku;
mod socket;

use std::env;
use std::io;

use vpnhotspotd::shared::protocol::daemon_io_error_report;

/// One argument is the root-side control socket. `--app-uid` followed by a socket selects the app-UID path
/// instead: that path owns a TUN and no system state, because nothing the root control loop does is permitted
/// at the app UID.
///
/// The runtime is built here rather than by `#[tokio::main]` for one reason: the app-UID path normalizes the
/// scheduling policy it was launched under, and a Tokio worker thread inherits the policy of the thread that
/// created it. The multi-threaded builder creates all of them inside `build()`, so the normalization has to
/// happen before that call - see [shizuku::scheduling]. Everything else about the runtime is what the macro
/// would have built: a multi-threaded scheduler with every driver enabled, one worker per CPU.
fn main() -> io::Result<()> {
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
    // Only the app-UID path inherits the launching coroutine dispatcher's policy. Normalize it before Tokio
    // creates its worker threads; the separately launched root daemon keeps its policy unchanged.
    let scheduling = if app_uid {
        match shizuku::scheduling::normalize() {
            Ok(None) => None,
            Ok(Some(inherited)) => {
                report::stdout!(
                    "scheduling policy {inherited} inherited from the launching thread, normalized \
                     before the runtime started"
                );
                None
            }
            Err(e) => {
                report::stderr!(
                    "the inherited scheduling policy could not be normalized, so this dataplane runs \
                     under it: {e}"
                );
                Some(daemon_io_error_report("shizuku.scheduling", e))
            }
        }
    } else {
        None
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            if app_uid {
                shizuku::run(socket_name, scheduling).await
            } else {
                root::run(socket_name).await
            }
        })
}
