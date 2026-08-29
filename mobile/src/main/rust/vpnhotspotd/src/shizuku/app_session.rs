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
    terminal: Terminal,
}

impl Session {
    /// Reads the start call, brings up the dataplane it asked for, acknowledges it, and then serves configs
    /// until something ends the session.
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
                // A frame without a usable call ID has no route for a terminal reply.
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
        // Install background reporting only after the conversation has an accepted call ID.
        self.reporter = Some(report::init(control.clone())?);
        // The app cannot prove what it transferred, so the descriptor is checked against itself here. A
        // refusal closes every descriptor that arrived.
        let (tun, gateway) = handoff::verify_tun(received, &start.interface_name, start.mtu)?;
        if let Some(declared) = start
            .gateway_addresses
            .iter()
            .find_map(|address| match address {
                std::net::IpAddr::V4(address) => Some(*address),
                std::net::IpAddr::V6(_) => None,
            })
        {
            if declared != gateway {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} has address {gateway} but {declared} was declared",
                        start.interface_name
                    ),
                )
                .with_report_context("shizuku.control.start_session"));
            }
        }
        let virtual_addresses = Arc::new(start.virtual_addresses);
        let gateway_addresses = start.gateway_addresses;
        // Reader and writer share one AsyncFd so exactly one owner closes the TUN descriptor.
        let fd = Arc::new(AsyncFd::new(tun).with_report_context("shizuku.control.tun_register")?);
        // Measure after opening the control socket and TUN so both count against this session's budget.
        let measured = budget::measure()
            .await
            .with_report_context("shizuku.control.budget")?;
        let mtu = start.mtu as usize;
        // `prepare` reserves the aggregate before building queues or buffers.
        let (dataplane, queue) = tun_reader::prepare(measured, mtu).await?;
        // One config in flight at a time, which is all the app ever sends: it coalesces to a single pending
        // slot and waits for the reply before sending the next.
        let (configs, requests) = mpsc::channel(1);
        self.dataplane.admit(
            "shizuku.tun_ingress",
            tokio::spawn(tun_reader::run(
                Arc::clone(&fd),
                dataplane,
                virtual_addresses,
                gateway_addresses,
                requests,
                self.cancel.clone(),
            )),
        );
        self.dataplane.admit(
            "shizuku.tun_egress",
            tokio::spawn(tun_writer::run(fd, mtu, queue, self.cancel.clone())),
        );
        // Acknowledge only after the validated TUN and both dataplane tasks have owners.
        if !control.send_frame(ack_event_frame(start.call_id)) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the control writer stopped before the session started",
            )
            .with_report_context("shizuku.control.start_session"));
        }
        // Writer cancellation, any dataplane terminal, or the config loop ends the session.
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
    async fn finish(mut self, result: io::Result<()>) -> io::Result<()> {
        self.cancel.cancel();
        let tasks = self.dataplane.shutdown().await;
        let mut terminal = self.terminal;
        // One failure, one delivery, and this is where each is routed to its own. Done before anything is
        // folded and before the reporter is finished, because this is the last point at which each failure
        // still exists on its own: [combine] below turns two into one message, and neither an errno nor an
        // attached report survives that.
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

/// Claims the session terminal when possible; otherwise emits a structured nonfatal.
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
