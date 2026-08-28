//! The app-UID control conversation: the start call that owns a session, the dataplane it brings up, and the
//! one path that ends all of it.
//!
//! The app UID launches this binary directly, so there is no root shell and no privileged dataplane. It
//! speaks the same `ClientEnvelope`/`DaemonEnvelope` conversation the root daemon does and none of root's
//! commands: the start call carries the TUN over `SCM_RIGHTS`, its call ID owns the dataplane for as long as
//! the session runs, and the level-triggered configs in [crate::shizuku::app_config] are keyed to it.
//!
//! What that buys is attribution. From the moment a frame names a call, every failure this file can produce -
//! a descriptor that is not the TUN it was said to be, a budget that cannot be measured, a dataplane task
//! that died - is answered on that call as an `ErrorFrame` carrying the structured report, rather than
//! reaching the app as a closed socket it has to guess about. The two failures that cannot be answered are
//! named as such: a frame malformed before any call ID could be read, and a control writer that can no longer
//! carry a frame at all.
//!
//! Nothing here coordinates with root mode, and nothing needs to: this daemon relays a TUN that Android's
//! tethering may or may not have selected as its upstream, and root mode's own per-interface routing is
//! installed independently of it. When both are running, root's routing takes precedence over whatever
//! upstream Android picked, by the ordinary root design and without either side being told.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use prost::Message;
use tokio::io::unix::AsyncFd;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::app_control::{self, Rejected};
use vpnhotspotd::shared::app_terminal::Terminal;
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::{
    ack_event_frame, describe_io_error, IoErrorReportExt, IoResultReportExt,
};
use vpnhotspotd::shared::reporter::ReporterGuard;
use vpnhotspotd::shared::tasks::{combine, Tasks, Watched};

use crate::control_wire::{connect_control_socket, spawn_writer};
use crate::report::{self, ControllerSender, ControllerSenderExt};
use crate::shizuku::app_config::{self, Ended};
use crate::shizuku::budget;
use crate::shizuku::handoff;
use crate::shizuku::tun_reader;
use crate::shizuku::tun_writer;

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let stream = connect_control_socket(&socket_name).await?;
    // The descriptor rides on the start call's frame, so that frame is read with `recvmsg` before the stream
    // is split. Nothing can be answered yet and nothing tries to be: a frame malformed at this level names no
    // call, so a report about it would have no call to belong to.
    let (payload, received) = handoff::recv_start_frame(&stream).await?;
    // Split, and the writer started, before the start call's own contents are looked at. That ordering is the
    // point: from here on there is somewhere to put a failure, so everything the app could have got wrong -
    // the command, the descriptor, the interface it names - comes back to it as this call's error.
    let (mut reader, writer) = stream.into_split();
    // Created before the writer so the writer can cancel it: a control connection that cannot carry a frame
    // ends the session, and every other task has to stop with it rather than at its next deadline.
    let cancel = CancellationToken::new();
    let (control, writing) = spawn_writer(writer, cancel.clone());
    // From here on there is a task running, so every exit goes through [Session::finish] rather than
    // returning.
    let mut session = Session {
        dataplane: Tasks::new(cancel.clone()),
        cancel,
        reporter: None,
        control: Some(control),
        writing,
        terminal: Terminal::Nobody,
    };
    let result = session.serve(&mut reader, payload, received).await;
    session.finish(result).await
}

/// Everything this conversation started, and the single path that ends all of it.
struct Session {
    cancel: CancellationToken,
    /// The two halves of the dataplane. Owned rather than detached because their completion is an event this
    /// session has to observe: a control socket can be quiet indefinitely, so a dataplane that died would
    /// otherwise go unnoticed until the app spoke again.
    dataplane: Tasks,
    /// Installed once the writer exists, because a report can only be carried by it. `None` until then, and
    /// on the path where installing it failed.
    reporter: Option<ReporterGuard>,
    /// Taken by [Session::finish]: the writer task ends when the last sender is dropped, and that may only
    /// happen after the reporter has flushed through it.
    control: Option<ControllerSender>,
    writing: JoinHandle<io::Result<()>>,
    /// Which call is owed this session's one terminal frame, and what that frame carries - see [Terminal].
    ///
    /// [Terminal::Nobody] until a frame has named a call, and again for the endings that owe nothing:
    /// control-socket EOF and a cancel leave nobody waiting. A refused config moves it to the call that was
    /// refused, so the start call is no longer owed anything and that one failure reaches the app once.
    terminal: Terminal,
}

impl Session {
    /// Reads the start call, brings up the dataplane it asked for, acknowledges it, and then serves configs
    /// until something ends the session.
    ///
    /// Anything that fails here returns, and [Session::finish] is what cleans up after it - which is why
    /// this is a method rather than the body of [run].
    async fn serve(
        &mut self,
        reader: &mut OwnedReadHalf,
        payload: Vec<u8>,
        received: Vec<OwnedFd>,
    ) -> io::Result<()> {
        let control = self
            .control
            .clone()
            .expect("the control sender is taken only by finish");
        let envelope = daemon::ClientEnvelope::decode(payload.as_slice()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e)
                .with_report_context("shizuku.control.parse_frame")
        })?;
        // Every descriptor that arrived is still owned by this frame, so returning here closes all of them.
        let start = match app_control::read_start_call(envelope) {
            Ok(start) => start,
            Err(Rejected { call_id, error }) => {
                // Left as [Terminal::Nobody] when the frame carried no usable call ID, which is the one
                // refusal that cannot be delivered; [Session::finish] then ends the conversation without
                // pretending otherwise.
                if let Some(call_id) = call_id {
                    self.terminal = Terminal::Start {
                        call_id,
                        report: None,
                    };
                }
                return Err(error);
            }
        };
        self.terminal = Terminal::Start {
            call_id: start.call_id,
            report: None,
        };
        // Only now, with a call ID accepted: a nonfatal needs a conversation to belong to, and until this
        // line there was none. The fallible steps below do not depend on it - they are answered on the start
        // call, and a terminal frame goes out on the writer that has been up since before this call was even
        // decoded - so what this adds is the background half: a report raised once the session is running
        // has somewhere to go other than the control socket falling quiet.
        self.reporter = Some(report::init_owned(control.clone())?);
        // The app cannot prove what it transferred, so the descriptor is checked against itself here. A
        // refusal closes every descriptor that arrived.
        let (tun, gateway) = handoff::verify_tun(received, &start.interface_name, start.mtu)?;
        // One descriptor, two owners: the reader waits on readability and the writer on writability, and
        // AsyncFd's readiness methods take &self, so sharing it avoids duplicating the descriptor and keeps
        // "exactly one owner closes it" true.
        let fd = Arc::new(AsyncFd::new(tun).with_report_context("shizuku.control.tun_register")?);
        // Measured here rather than at process start, so the fixed descriptors this session owns - the
        // control socket and the TUN - are already counted among what is open. Named at this boundary, so a
        // start refused because the descriptor budget could not be read says that rather than arriving at
        // teardown as an unattributed session failure.
        let measured = budget::measure()
            .await
            .with_report_context("shizuku.control.budget")?;
        let mtu = start.mtu as usize;
        // Reserved, then built, and in that order because this call owns the order - see
        // [tun_reader::prepare]. Nothing here holds a writer or a queue of its own: the ingress task gets its
        // half inside the bundle and the egress task gets the other, so the sequence this replaced, where
        // the channels existed before the aggregate that pays for them, is not expressible from here.
        //
        // Every fallible construction a usable dataplane needs is inside this call, which is what makes the
        // ACK below honest: a denial fails the start rather than being answered as ready and terminating a
        // moment later.
        let (dataplane, queue) = tun_reader::prepare(measured, mtu).await?;
        // One config in flight at a time, which is all the app ever sends: it coalesces to a single pending
        // slot and waits for the reply before sending the next.
        let (configs, requests) = mpsc::channel(1);
        self.dataplane.admit(
            "shizuku.tun_ingress",
            tokio::spawn(tun_reader::run(
                Arc::clone(&fd),
                dataplane,
                requests,
                self.cancel.clone(),
            )),
        );
        self.dataplane.admit(
            "shizuku.tun_egress",
            tokio::spawn(tun_writer::run(fd, mtu, queue, self.cancel.clone())),
        );
        // Readiness, and not one step earlier: the descriptor has been validated, the dataplane owns it, and
        // everything that could have refused this start has already run. A start that failed above never
        // reaches this line, and the app is told so as this call's error rather than as a socket that closed.
        if !control.send_frame(ack_event_frame(start.call_id)) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the control writer stopped before the session started",
            )
            .with_report_context("shizuku.control.start_session"));
        }
        // Three ways this session ends, and all three are here rather than only the last. Cancellation is the
        // control writer having failed, after which reading the other half of a desynchronized stream would
        // be reading nothing trustworthy. A dataplane task finishing - cleanly, failing, or not completing at
        // all - ends the session too: the app's configs are answered against tasks that own the TUN, so a
        // session that outlived one of them would be answering for nothing.
        tokio::select! {
            biased;
            watched = self.dataplane.watch() => match watched {
                Watched::Cancelled => Ok(()),
                Watched::Finished { name, result } => {
                    report::stderr!("{name} finished, ending the session");
                    result
                }
            },
            ended = app_config::serve(
                reader,
                start.call_id,
                &start.interface_name,
                gateway,
                &configs,
                &control,
            ) => match ended {
                Ok(Ended::Silent) => {
                    self.terminal = Terminal::Nobody;
                    Ok(())
                }
                Ok(Ended::Refused {
                    call_id,
                    report,
                    error,
                }) => {
                    self.terminal = Terminal::Refused { call_id, report };
                    Err(error)
                }
                Err(e) => Err(e),
            },
        }
    }

    /// The one cleanup path, whatever ended the session.
    ///
    /// The order is the contract. Cancelling and joining the dataplane first is what makes "the TUN is closed
    /// by exactly one owner" true, because the session holds a share of that descriptor until both halves are
    /// gone. The reporter is finished before *whichever* call is owed a terminal frame gets it - see
    /// [Terminal::answer], which holds those two together for that reason - so a report coalesced into a
    /// pending window, or raised by this teardown itself, still reaches the app before the frame that makes it
    /// stop listening. That frame is sent before the sender is dropped, since dropping it is what ends the
    /// writer and turns "nothing follows the terminal frame" into the EOF the app reads. A session ending
    /// *because* the writer failed has no such guarantee: that writer has already closed and drained its
    /// queue, so these last frames go nowhere, and saying so on stderr is all that is left. The writer is
    /// joined last. Every one of those results is folded in rather than replaced: a session that failed *and*
    /// could not shut down cleanly is not the same as either alone.
    async fn finish(mut self, result: io::Result<()>) -> io::Result<()> {
        self.cancel.cancel();
        let tasks = self.dataplane.shutdown().await;
        let mut terminal = self.terminal;
        // One failure, one delivery, and this is where each is routed to its own. Done before anything is
        // folded and before the reporter is finished, because this is the last point at which each failure
        // still exists on its own: [combine] below turns two into one message, and neither an errno nor an
        // attached report survives that.
        //
        // Three destinations. A failure the terminal frame is still looking for something to carry becomes
        // that frame - at most one does, and the session's own is offered first because a dataplane task that
        // ended when the session did is the consequence rather than the cause. A failure already carried by
        // the frame this session owes is left alone. Everything else has no call to belong to and goes out as
        // a nonfatal. Exactly one of the three runs for each failure that arrives here, because a failure an
        // owner returned is one it did not also emit.
        //
        // A refused config is the one ending whose failure the frame already describes: [app_config::refuse]
        // built its report where the refusal happened, so offering it again would answer the same call twice
        // and reporting it would deliver it as a nonfatal beside its own `ErrorFrame`.
        if !matches!(terminal, Terminal::Refused { .. }) {
            route(&mut terminal, "shizuku.app_session", &result);
        }
        for (name, task) in &tasks {
            route(&mut terminal, name, task);
        }
        // A loop rather than `fold`, and never `try_fold`: short-circuiting on the first failing task is
        // exactly what [combine] exists to avoid, since both halves have to survive into the message.
        let mut dataplane = Ok(());
        for (_, task) in tasks {
            dataplane = combine(dataplane, task);
        }
        let ending = terminal
            .answer(self.reporter.take(), |call_id, frame| {
                if !self
                    .control
                    .as_ref()
                    .is_some_and(|control| control.send_frame(frame))
                {
                    report::stderr!(
                        "call {call_id} could not be answered: the controller send failed"
                    );
                }
            })
            .await;
        drop(self.control.take());
        let writing = match self.writing.await {
            Ok(result) => result,
            Err(e) => Err(io::Error::other(format!("control writer task failed: {e}"))),
        };
        combine(combine(result, dataplane), combine(ending, writing))
    }
}

/// Routes one independently-owned failure to its one destination: the terminal frame this session owes when
/// nothing has claimed it yet, and a structured nonfatal when nothing else is going to answer for it -
/// control-socket EOF and a cancel leave nobody waiting, and a dataplane task's failure has no call of its
/// own.
///
/// [describe_io_error] hands back the report an owner already attached rather than rebuilding one, so the
/// errno, the details and the Rust source location are the failing site's own rather than this line's.
///
/// An owner never both returns a failure and emits it - [crate::shizuku::tun_writer] and the session seed
/// attach a report and return it, and nothing else - which is what makes "one failure, one delivery" hold
/// whichever of the two paths a failure takes. Emitting at the owner *and* answering a call with the same
/// attached report is exactly the duplicate this shape removes.
///
/// That is a rule about each failure, not about each owner. A result carries one error, so an owner that
/// observes a *second*, independently caused failure has nothing left to return it on and routes that one
/// itself, once - see [crate::shizuku::virtual_dns] for the DNS drain and the query task whose owner is
/// already gone. Those never reach here, and the one that does is still the only failure this frame
/// describes.
///
/// `#[track_caller]` so a failure arriving without an attached report is located at the caller naming it
/// rather than at this line, which every one of them would otherwise share and the coalescer would then read
/// as a single site.
#[track_caller]
fn route(terminal: &mut Terminal, context: &'static str, result: &io::Result<()>) {
    let Err(error) = result else {
        return;
    };
    if let Some(report) = terminal.claim(describe_io_error(
        context,
        error,
        std::iter::empty::<(&str, &str)>(),
    )) {
        report::report(report);
    }
}
