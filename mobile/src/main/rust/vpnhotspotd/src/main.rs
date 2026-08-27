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
    if app_uid {
        // Only this path, and deliberately: it is the one this app forks from its own coroutine dispatcher
        // thread, so it is the one that inherits whatever policy that thread was running under. The root
        // daemon is forked by `RunDaemon.execute` inside a separate root service process, so its policy is
        // not this app's to explain and is left exactly as it was.
        //
        // Written to this process's own startup output rather than raised as a structured report, and that
        // is a statement about *when* this runs: the runtime, the control socket and the conversation's
        // reporter all come after it, so there is no call to attach a report to and no framing to send one
        // on. The app drains this output, so it reaches the same log a report would. The failure is an
        // expected environment difference - a kernel or seccomp policy that refuses the change - and the
        // dataplane is correct under the inherited policy and only less responsive, so it is a line rather
        // than a session that does not start.
        match shizuku::scheduling::normalize() {
            Ok(None) => {}
            Ok(Some(inherited)) => report::stdout!(
                "scheduling policy {inherited} inherited from the launching thread, normalized before the \
                 runtime started"
            ),
            Err(e) => report::stderr!(
                "the inherited scheduling policy could not be normalized, so this dataplane runs under it: \
                 {e}"
            ),
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            if app_uid {
                shizuku::run(socket_name).await
            } else {
                root::run(socket_name).await
            }
        })
}
