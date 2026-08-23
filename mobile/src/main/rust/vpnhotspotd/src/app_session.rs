//! The app-UID session's control loop, which is what the bootstrap hands off to.
//!
//! Its whole contract is level-triggered: the newest [ShizukuSessionConfig] is the truth, and the daemon
//! answers each one with the two axes it has actually applied. Nothing here replays history, because the app
//! coalesces and a superseded config is never sent twice.
//!
//! Retirement happens strictly before the acknowledgement. That ordering is the reason the app can reopen
//! admission safely: seeing an [ShizukuApplied] carrying an epoch means everything keyed to the previous one
//! is already gone, so a later `ACTIVE` cannot reuse state the daemon has not retired. The ingress task owns
//! that state, so it is the ingress task that reports the retirement finished - this loop only refuses to
//! acknowledge until it has.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use prost::Message;
use tokio::io::unix::AsyncFd;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::proto::daemon::{
    shizuku_daemon_frame, DaemonErrorReport, ShizukuApplied, ShizukuDaemonFrame,
    ShizukuSessionConfig,
};
use vpnhotspotd::shared::protocol::{describe_io_error, reported_io_error_report};
use vpnhotspotd::shared::reporter::ReporterGuard;
use vpnhotspotd::shared::session_config;
use vpnhotspotd::shared::tasks::{combine, Tasks, Watched};

use crate::budget;
use crate::control::wire::{recv_packet, spawn_writer};
use crate::report::{self, ControllerSender, ControllerSenderExt};
use crate::tun_reader::{self, Applied};
use crate::tun_writer;
use vpnhotspotd::shared::egress;

/// This conversation's report encoder. The call ID is dropped rather than carried: there are no calls at
/// the app UID, so nothing could own one.
fn report_encoder(_call_id: Option<u64>, report: DaemonErrorReport) -> Vec<u8> {
    ShizukuDaemonFrame {
        frame: Some(shizuku_daemon_frame::Frame::Report(report)),
    }
    .encode_to_vec()
}

pub(crate) async fn run(
    stream: UnixStream,
    tun: OwnedFd,
    interface_name: String,
    gateway: Ipv4Addr,
    mtu: usize,
) -> io::Result<()> {
    // Nothing fallible about the dataplane happens before the writer and the reporter exist, and that
    // ordering is the point: BootstrapReady has already been sent, so a failure from here on is a failure
    // the app is waiting to hear about, and one raised before the reporter was installed could only reach it
    // as a closed socket. The two steps below cannot fail, so there is nothing to unwind past them.
    //
    // The write half becomes a task of its own because a report has to be able to leave whenever it happens:
    // the loop below blocks on the read half for as long as the app is quiet, which is exactly when a
    // background failure would otherwise have nobody to tell.
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
    };
    let result = session
        .serve(&mut reader, tun, &interface_name, gateway, mtu)
        .await;
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
}

impl Session {
    /// Starts the reporter and the dataplane, then serves configs until something ends the session.
    ///
    /// Anything that fails here returns, and [Session::finish] is what cleans up after it - which is why
    /// this is a method rather than the body of [run].
    async fn serve(
        &mut self,
        reader: &mut OwnedReadHalf,
        tun: OwnedFd,
        interface_name: &str,
        gateway: Ipv4Addr,
        mtu: usize,
    ) -> io::Result<()> {
        let control = self
            .control
            .clone()
            .expect("the control sender is taken only by finish");
        // First, so that every fallible step after it is one the app hears about as a structured report
        // rather than as the control socket going quiet.
        self.reporter = Some(report::init_owned(control.clone(), report_encoder)?);
        // One descriptor, two owners: the reader waits on readability and the writer on writability, and
        // AsyncFd's readiness methods take &self, so sharing it avoids duplicating the descriptor and keeps
        // "exactly one owner closes it" true.
        let fd = Arc::new(AsyncFd::new(tun)?);
        // Measured here rather than at process start, so the fixed descriptors this session owns - the
        // control socket and the TUN - are already counted among what is open.
        let measured = budget::measure().await?;
        // Reserved, then built, and in that order because this call owns the order - see
        // [tun_reader::prepare]. Nothing here holds a writer or a queue of its own: the ingress task gets its
        // half inside the bundle and the egress task gets the other, so the sequence this replaced, where
        // the channels existed before the aggregate that pays for them, is not expressible from here.
        let (dataplane, queue) = tun_reader::prepare(measured, mtu)?;
        // One config in flight at a time, which is all the app ever sends: it coalesces to a single pending
        // slot and waits for the acknowledgement before sending the next.
        let (configs, requests) = mpsc::channel(1);
        self.dataplane.admit(
            "shizuku.tun_ingress",
            tokio::spawn(tun_reader::run(
                Arc::clone(&fd),
                mtu,
                dataplane,
                requests,
                self.cancel.clone(),
            )),
        );
        self.dataplane.admit(
            "shizuku.tun_egress",
            tokio::spawn(tun_writer::run(fd, mtu, queue, self.cancel.clone())),
        );
        // Three ways this session ends, and all three are here rather than only the last. Cancellation is the
        // control writer having failed, after which reading the other half of a desynchronized stream would
        // be reading nothing trustworthy. A dataplane task finishing - cleanly, failing, or not completing at
        // all - ends the session too: the app's configs are acknowledged against tasks that own the TUN, so a
        // session that outlived one of them would be acknowledging nothing.
        tokio::select! {
            biased;
            watched = self.dataplane.watch() => match watched {
                Watched::Cancelled => Ok(()),
                Watched::Finished { name, result } => {
                    report::stderr!("{name} finished, ending the session");
                    result
                }
            },
            result = serve(reader, interface_name, gateway, &configs, &control) => result,
        }
    }

    /// The one cleanup path, whatever ended the session.
    ///
    /// The order is the contract. Cancelling and joining the dataplane first is what makes "the TUN is closed
    /// by exactly one owner" true, because the session holds a share of that descriptor until both halves are
    /// gone. The reporter is finished before the sender is dropped, so a report coalesced into a pending
    /// window still reaches the app instead of dying with the writer - when the writer is still able to
    /// carry one. A session ending *because* the writer failed has no such guarantee: that writer has
    /// already closed and drained its queue, so these last reports go nowhere. The writer is joined last, since it
    /// ends when the last sender does. Every one of those results is folded in rather than replaced: a
    /// session that failed *and* could not shut down cleanly is not the same as either alone.
    async fn finish(mut self, result: io::Result<()>) -> io::Result<()> {
        self.cancel.cancel();
        let tasks = self.dataplane.shutdown().await;
        // Described here, before anything is folded and before the reporter is finished, because this is the
        // last point at which each failure still exists on its own: [combine] below turns two into one
        // message, and an errno and an attached report survive neither. Nothing is described twice - a task
        // that converged on its own failure carries what it emitted, and [describe_unreported] skips it -
        // and nothing here changes what is returned.
        describe_unreported("shizuku.app_session", &result);
        for (name, task) in &tasks {
            describe_unreported(name, task);
        }
        // A loop rather than `fold`, and never `try_fold`: short-circuiting on the first failing task is
        // exactly what [combine] exists to avoid, since both halves have to survive into the message.
        let mut dataplane = Ok(());
        for (_, task) in tasks {
            dataplane = combine(dataplane, task);
        }
        let reported = match self.reporter.take() {
            Some(reporter) => reporter.finish().await,
            None => Ok(()),
        };
        drop(self.control.take());
        let writing = match self.writing.await {
            Ok(result) => result,
            Err(e) => Err(io::Error::other(format!("control writer task failed: {e}"))),
        };
        combine(combine(result, dataplane), combine(reported, writing))
    }
}

/// Emits the structured form of a terminal session failure, unless it is already carrying one.
///
/// The exception is what keeps this from doubling up. [crate::tun_writer] converges every fatal way out of
/// its loop on one report with counters this frame does not have, and the session seed does the same for
/// entropy; both attach what they emitted, and describing them again here would put one failure in front of
/// the app twice - as one report with two occurrences, since the coalescer keys on the site.
///
/// `#[track_caller]` for that same reason, and it is load-bearing rather than tidy: without it both calls
/// below resolve to the one line inside this function, the coalescer reads them as one site, and the second
/// failure arrives folded into the first as an occurrence instead of as itself.
#[track_caller]
fn describe_unreported(context: &'static str, result: &io::Result<()>) {
    let Err(error) = result else {
        return;
    };
    if reported_io_error_report(error).is_some() {
        return;
    }
    report::report(describe_io_error(
        context,
        error,
        std::iter::empty::<(&str, &str)>(),
    ));
}

async fn serve(
    stream: &mut OwnedReadHalf,
    interface_name: &str,
    gateway: Ipv4Addr,
    configs: &mpsc::Sender<Applied>,
    control: &ControllerSender,
) -> io::Result<()> {
    // Zero is not a valid configured value, so the first config always looks like a change and its
    // retirement runs once against empty state rather than being skipped. [session_config::check] is what
    // holds the peer to that, along with every other shape rule the contract has - including that neither
    // axis moves backwards, which is why nothing here has to remember them separately.
    let mut configs_owner = Configs {
        previous: None,
        interface_name: interface_name.to_owned(),
        gateway,
    };
    loop {
        let packet = match recv_packet(stream).await {
            Ok(packet) => packet,
            // EOF on the control socket is the authoritative cancellation signal
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        let config = ShizukuSessionConfig::decode(packet.as_slice())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let sequence = config.sequence;
        let upstream_generation = config.upstream_generation;
        let downstream_epoch = config.downstream_epoch;
        let admit = config.admit;
        let applied = configs_owner.publish(config, configs).await?;
        // The acknowledgement waits on this, which is the whole ordering guarantee: the app may only believe
        // the previous epoch is gone once the task that owns its state says so.
        if applied.await.is_err() {
            return Err(io::Error::other("tun ingress dropped a config it accepted"));
        }
        let reply = ShizukuDaemonFrame {
            frame: Some(shizuku_daemon_frame::Frame::Applied(ShizukuApplied {
                sequence,
                upstream_generation,
                downstream_epoch,
                admitting: admit,
            })),
        }
        .encode_to_vec();
        if !control.send_frame(reply) {
            return Err(io::Error::other(
                "the control writer stopped before the session",
            ));
        }
    }
    Ok(())
}

/// What one config has to survive before the dataplane is allowed to see it, and the only thing that decides
/// what "the previous config" means.
///
/// Apart from the loop because the loop is I/O and this is not: every refusal below is a comparison or a
/// decode, and each of them is terminal. Terminal has to mean *nothing downstream saw it* - so a config that
/// fails any of them must leave [Configs::previous] where it was and publish nothing at all, or the next
/// config would be checked against a predecessor this daemon never accepted.
struct Configs {
    /// Zero is not a valid configured value, so the first config always looks like a change and its
    /// retirement runs once against empty state rather than being skipped. [session_config::check] is what
    /// holds the peer to that, along with every other shape rule the contract has - including that neither
    /// axis moves backwards, which is why nothing here has to remember them separately.
    previous: Option<ShizukuSessionConfig>,
    interface_name: String,
    /// The IPv4 address the TUN really has, which a declared gateway is checked against.
    gateway: Ipv4Addr,
}

impl Configs {
    /// Validates one config and publishes it, or refuses it having published nothing and advanced nothing.
    ///
    /// The order is the property. Every fallible step runs before [Configs::previous] is assigned, and the
    /// send runs after - so there is no config that was half-accepted, and no shape that reaches a transport
    /// or the resolver on its way to being refused.
    async fn publish(
        &mut self,
        config: ShizukuSessionConfig,
        configs: &mpsc::Sender<Applied>,
    ) -> io::Result<oneshot::Receiver<()>> {
        if let Err(e) = session_config::check(self.previous.as_ref(), &config) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config {} is not a valid successor: {e:?}", config.sequence),
            ));
        }
        let gateway_addresses = addresses(&config.gateway_addresses)?;
        // The IPv4 half of the declaration is checked against the interface, because an ICMP error sourced
        // from an address the TUN does not hold is one the client would either ignore or believe, and neither
        // is what the daemon meant to say. The IPv6 half cannot be checked at this UID at all - netlink
        // binding and /proc/net are both denied - so it is taken as declared, which is a limit rather than a
        // decision.
        if let Some(declared) = gateway_addresses.iter().find_map(|address| match address {
            IpAddr::V4(address) => Some(*address),
            IpAddr::V6(_) => None,
        }) {
            if declared != self.gateway {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} has address {} but {declared} was declared",
                        self.interface_name, self.gateway
                    ),
                ));
            }
        }
        // Decoded here, before anything is assigned or published: a config whose egress fields disagree with
        // each other is terminal, and terminal has to mean nothing downstream ever saw it. See
        // [vpnhotspotd::shared::egress] for why a present zero is one of those disagreements.
        let egress = egress::decode(&config).map_err(|why| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config {} has no usable egress: {why:?}", config.sequence),
            )
        })?;
        let virtual_addresses = Arc::new(addresses(&config.virtual_addresses)?);
        report::stdout!(
            "applying config {} on {}: generation {}, epoch {}, admitting {}, egress {egress:?}, floor {}",
            config.sequence,
            self.interface_name,
            config.upstream_generation,
            config.downstream_epoch,
            config.admit,
            config.downstream_mtu_floor,
        );
        let (retired, applied) = oneshot::channel();
        let published = Applied {
            // Set only from the config and never inferred, because only the app knows the session is ACTIVE.
            admitting: config.admit,
            upstream_generation: config.upstream_generation,
            downstream_epoch: config.downstream_epoch,
            egress,
            virtual_addresses,
            gateway_addresses,
            downstream_mtu_floor: config.downstream_mtu_floor as usize,
            retired,
        };
        // Last, and only now: everything that could refuse this config has already run, so what the dataplane
        // receives is a config this owner has committed to.
        self.previous = Some(config);
        if configs.send(published).await.is_err() {
            return Err(io::Error::other("tun ingress stopped before the session"));
        }
        Ok(applied)
    }
}

/// Packed 4- or 16-byte addresses from a config. A malformed entry is rejected rather than ignored: silently
/// dropping one from the virtual set would turn traffic the design intends to intercept into traffic it
/// relays, and dropping one from the gateway set would silence errors the daemon owes a client.
fn addresses(packed: &[Vec<u8>]) -> io::Result<Vec<IpAddr>> {
    packed
        .iter()
        .map(|address| match address.len() {
            4 => Ok(IpAddr::from(<[u8; 4]>::try_from(&address[..]).unwrap())),
            16 => Ok(IpAddr::from(<[u8; 16]>::try_from(&address[..]).unwrap())),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("virtual address has {other} bytes"),
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vpnhotspotd::shared::egress::{Egress, RelayUpstream};
    use vpnhotspotd::shared::protocol::IoErrorReportExt;

    const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 43, 1);

    /// A session with no dataplane of its own, for the teardown tests below.
    ///
    /// Everything [Session::finish] touches and nothing it does not: the control writer is a task that has
    /// already succeeded, because what is under test is which failures get described rather than how the
    /// writer ends.
    fn teardown_session(
        control: ControllerSender,
        reporter: ReporterGuard,
    ) -> (Session, CancellationToken) {
        let cancel = CancellationToken::new();
        let session = Session {
            dataplane: Tasks::new(cancel.clone()),
            cancel: cancel.clone(),
            reporter: Some(reporter),
            control: Some(control),
            writing: tokio::spawn(async { Ok(()) }),
        };
        (session, cancel)
    }

    /// Collects every nonfatal the session hands over, keyed by context.
    fn collect(
        mut published: mpsc::UnboundedReceiver<report::ControllerMessage>,
    ) -> JoinHandle<Vec<(String, Option<i32>)>> {
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(message) = published.recv().await {
                if let report::ControllerMessage::Nonfatal { report, .. } = message {
                    // An explicit list, because [report::exclusive] serializes *installation* and not
                    // emission: a test that reports without owning the registry still lands in whichever one
                    // is installed, so anything broader than the contexts these tests assert on is a race.
                    if matches!(
                        report.context.as_str(),
                        "shizuku.app_session" | "shizuku.tun_ingress" | "shizuku.tun_egress"
                    ) {
                        seen.push((report.context, report.errno));
                    }
                }
            }
            seen
        })
    }

    /// A dataplane task that failed is described to the app, not only folded into the process's exit.
    ///
    /// This is the ingress half's only route to the app. [crate::tun_reader] reports the one startup failure
    /// it converges on itself - the session seed - and returns everything else bare, so without this a read
    /// that died took its errno with it and the app saw a control socket reaching EOF.
    #[tokio::test]
    async fn a_failed_dataplane_task_is_described_before_the_reporter_finishes() {
        let _reporting = report::exclusive().await;
        let (control, published) = mpsc::unbounded_channel();
        let reporter = report::init_owned(control.clone(), |_, _| Vec::new()).expect("installed");
        let collector = collect(published);
        let (mut session, _cancel) = teardown_session(control.clone(), reporter);
        session.dataplane.admit(
            "shizuku.tun_ingress",
            tokio::spawn(async { Err(io::Error::from_raw_os_error(libc::EIO)) }),
        );
        drop(control);

        let ended = session.finish(Ok(())).await.expect_err("the task failed");
        assert_eq!(Some(libc::EIO), ended.raw_os_error());
        let seen = collector.await.expect("the collector joined");
        assert_eq!(
            vec![("shizuku.tun_ingress".to_owned(), Some(libc::EIO))],
            seen
        );
    }

    /// A session failure and a shutdown failure are two facts, and both reach the app with their own errno.
    ///
    /// [combine] folds them into one message for the process's exit, which is right for an exit and useless
    /// as a report: one of the two errnos is gone by then. Describing them before the fold is what keeps
    /// both, and the folded error is still what the caller gets.
    #[tokio::test]
    async fn a_session_failure_and_a_shutdown_failure_are_described_separately() {
        let _reporting = report::exclusive().await;
        let (control, published) = mpsc::unbounded_channel();
        let reporter = report::init_owned(control.clone(), |_, _| Vec::new()).expect("installed");
        let collector = collect(published);
        let (mut session, _cancel) = teardown_session(control.clone(), reporter);
        session.dataplane.admit(
            "shizuku.tun_egress",
            tokio::spawn(async { Err(io::Error::from_raw_os_error(libc::ENOSPC)) }),
        );
        drop(control);

        let ended = session
            .finish(Err(io::Error::from_raw_os_error(libc::ECONNRESET)))
            .await
            .expect_err("both halves failed");
        let message = ended.to_string();
        assert!(
            message.contains("104") || message.contains("reset"),
            "{message}"
        );
        assert!(
            message.contains("28") || message.contains("space"),
            "{message}"
        );

        let mut seen = collector.await.expect("the collector joined");
        seen.sort();
        assert_eq!(
            vec![
                ("shizuku.app_session".to_owned(), Some(libc::ECONNRESET)),
                ("shizuku.tun_egress".to_owned(), Some(libc::ENOSPC)),
            ],
            seen
        );
    }

    /// Both dataplane halves failing produces one report each and one error carrying both.
    ///
    /// This is the shape [Tasks::shutdown] exists to keep apart. Folding first and describing afterwards
    /// would have reported the egress failure a second time - the writer already described it - and would
    /// have dropped one of the two errnos, because [combine] builds a fresh message-only error.
    #[tokio::test]
    async fn simultaneous_ingress_and_egress_failures_keep_both() {
        let _reporting = report::exclusive().await;
        let (control, published) = mpsc::unbounded_channel();
        let reporter = report::init_owned(control.clone(), |_, _| Vec::new()).expect("installed");
        let collector = collect(published);
        let (mut session, _cancel) = teardown_session(control.clone(), reporter);
        session.dataplane.admit(
            "shizuku.tun_ingress",
            tokio::spawn(async { Err(io::Error::from_raw_os_error(libc::EIO)) }),
        );
        // Carrying its own report, exactly as [crate::tun_writer] hands one back.
        session.dataplane.admit(
            "shizuku.tun_egress",
            tokio::spawn(async {
                Err(io::Error::from_raw_os_error(libc::ENOSPC)
                    .with_report_context("shizuku.tun_egress"))
            }),
        );
        drop(control);

        let ended = session
            .finish(Ok(()))
            .await
            .expect_err("both halves failed");
        let message = ended.to_string();
        assert!(
            message.contains("5") || message.contains("I/O"),
            "{message}"
        );
        assert!(
            message.contains("28") || message.contains("space"),
            "{message}"
        );
        let seen = collector.await.expect("the collector joined");
        assert_eq!(
            vec![("shizuku.tun_ingress".to_owned(), Some(libc::EIO))],
            seen,
            "the writer described its own; only the bare one is described here"
        );
    }

    /// A failure that already carries its own report is not described a second time.
    ///
    /// [crate::tun_writer] converges every fatal way out of its loop on one report with counters this frame
    /// does not have, and attaches it to what it returns. Describing it again would reach the app as one
    /// report with two occurrences - the coalescer keys on the site - which reads as the failure having
    /// happened twice.
    #[tokio::test]
    async fn a_failure_that_already_reported_itself_is_not_described_again() {
        let _reporting = report::exclusive().await;
        let (control, published) = mpsc::unbounded_channel();
        let reporter = report::init_owned(control.clone(), |_, _| Vec::new()).expect("installed");
        let collector = collect(published);
        let (session, _cancel) = teardown_session(control.clone(), reporter);
        drop(control);

        let already =
            io::Error::from_raw_os_error(libc::EPIPE).with_report_context("shizuku.tun_egress");
        session
            .finish(Err(already))
            .await
            .expect_err("the session failed");
        assert!(
            collector.await.expect("the collector joined").is_empty(),
            "the writer's own report is the only one"
        );
    }

    fn owner() -> Configs {
        Configs {
            previous: None,
            interface_name: "testtun0".to_owned(),
            gateway: GATEWAY,
        }
    }

    /// The shape every case varies one field of, so what a case is *about* is the difference.
    fn base() -> ShizukuSessionConfig {
        ShizukuSessionConfig {
            sequence: 1,
            upstream_generation: 1,
            downstream_epoch: 1,
            admit: true,
            upstream_network: None,
            upstream_interface_index: None,
            virtual_addresses: vec![vec![192, 0, 2, 5]],
            gateway_addresses: vec![GATEWAY.octets().to_vec()],
            downstream_mtu_floor: 1500,
        }
    }

    /// Publishes one config through the real owner and answers what the dataplane would have received.
    ///
    /// The receiver is real and bounded exactly as the session's is, so "published nothing" is observed on the
    /// channel rather than inferred from a return value.
    async fn publish(
        owner: &mut Configs,
        config: ShizukuSessionConfig,
        published: &mut mpsc::Receiver<Applied>,
        configs: &mpsc::Sender<Applied>,
    ) -> io::Result<Applied> {
        let applied = owner.publish(config, configs).await?;
        let received = published.try_recv().expect("a config the owner accepted");
        // The dataplane answers the retirement; nothing here is testing that half.
        drop(applied);
        Ok(received)
    }

    /// One raw egress shape, named, with what the owners must see - or nothing at all, when the shape is
    /// terminal at this boundary.
    struct Shape {
        what: &'static str,
        network: Option<u64>,
        interface: Option<u32>,
        egress: Option<Egress>,
    }

    /// Every shape the two optional fields can take.
    ///
    /// Three of them are the three a real session goes through, and each reaches the owners differently: a
    /// handle alone is enough for the resolver and the terminating engine, because both connect, while the
    /// relays need the interface index their unconnected replies are checked against. The other six are what
    /// a truncated, default-constructed or disagreeing peer produces - and `None` and `Some(0)` are different
    /// messages there, which is the whole reason this boundary exists: zero is what the platform reads as
    /// *this process's own default network*, which is the one fallback this mode does not have.
    const SHAPES: &[Shape] = &[
        Shape {
            what: "nothing selected yet",
            network: None,
            interface: None,
            egress: Some(Egress {
                selected_network: None,
                relay_upstream: None,
            }),
        },
        Shape {
            what: "a handle whose interface is not resolved yet",
            network: Some(0x1234),
            interface: None,
            egress: Some(Egress {
                selected_network: Some(0x1234),
                relay_upstream: None,
            }),
        },
        Shape {
            what: "both halves of one network",
            network: Some(0x1234),
            interface: Some(7),
            egress: Some(Egress {
                selected_network: Some(0x1234),
                relay_upstream: Some(RelayUpstream {
                    network: 0x1234,
                    interface: 7,
                }),
            }),
        },
        Shape {
            what: "an interface with no network",
            network: None,
            interface: Some(7),
            egress: None,
        },
        Shape {
            what: "a zero interface with no network",
            network: None,
            interface: Some(0),
            egress: None,
        },
        Shape {
            what: "a zero network",
            network: Some(0),
            interface: None,
            egress: None,
        },
        Shape {
            what: "a zero network and a zero interface",
            network: Some(0),
            interface: Some(0),
            egress: None,
        },
        Shape {
            what: "a zero network with an interface",
            network: Some(0),
            interface: Some(7),
            egress: None,
        },
        Shape {
            what: "a network with a zero interface",
            network: Some(0x1234),
            interface: Some(0),
            egress: None,
        },
    ];

    /// Every raw shape as a *first* config, driven through the real publication boundary.
    ///
    /// A refusal has to mean nothing downstream ever saw it, so each terminal shape is checked three ways:
    /// nothing reached the channel a transport and the resolver read, `previous` never moved, and the owner
    /// is still one that accepts a config with no predecessor rules applied.
    #[tokio::test]
    async fn every_raw_shape_is_decided_as_a_first_config() {
        let (configs, mut published) = mpsc::channel(4);
        for shape in SHAPES {
            let mut owner = owner();
            let config = ShizukuSessionConfig {
                upstream_network: shape.network,
                upstream_interface_index: shape.interface,
                ..base()
            };
            let Some(egress) = shape.egress else {
                assert!(
                    owner.publish(config, &configs).await.is_err(),
                    "{} must be terminal",
                    shape.what
                );
                assert!(
                    published.try_recv().is_err(),
                    "{} reached a transport or the resolver",
                    shape.what
                );
                assert_eq!(
                    owner.previous, None,
                    "{} advanced what the next config is checked against",
                    shape.what
                );
                publish(&mut owner, base(), &mut published, &configs)
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", shape.what));
                continue;
            };
            let applied = publish(&mut owner, config.clone(), &mut published, &configs)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", shape.what));
            assert_eq!(applied.egress, egress, "{}", shape.what);
            assert_eq!(
                owner.previous.as_ref(),
                Some(&config),
                "{} is what the next config must be checked against",
                shape.what
            );
        }
        assert!(published.try_recv().is_err(), "one config, one publication");
    }

    /// Every raw shape again, as a *successor* - which is where the predecessor state a refusal must not
    /// disturb actually exists.
    ///
    /// The generation advances with the raw fields, because that is the axis which retires the sockets bound
    /// behind them; what is under test here is the shape, and the axes are the test below.
    #[tokio::test]
    async fn every_raw_shape_is_decided_as_a_successor() {
        let (configs, mut published) = mpsc::channel(4);
        /// The successor that is legal whatever the shape under test was, so a refusal can be shown to have
        /// left the predecessor exactly where it was.
        fn legal() -> ShizukuSessionConfig {
            ShizukuSessionConfig {
                sequence: 2,
                upstream_generation: 2,
                ..base()
            }
        }
        for shape in SHAPES {
            let mut owner = owner();
            publish(&mut owner, base(), &mut published, &configs)
                .await
                .expect("the predecessor");
            let predecessor = owner.previous.clone().expect("accepted");
            let config = ShizukuSessionConfig {
                upstream_network: shape.network,
                upstream_interface_index: shape.interface,
                ..legal()
            };
            let Some(egress) = shape.egress else {
                assert!(
                    owner.publish(config, &configs).await.is_err(),
                    "{} must be terminal",
                    shape.what
                );
                assert!(
                    published.try_recv().is_err(),
                    "{} reached a transport or the resolver",
                    shape.what
                );
                assert_eq!(
                    owner.previous.as_ref(),
                    Some(&predecessor),
                    "{} disturbed the predecessor",
                    shape.what
                );
                // And the successor that was legal before the refusal is still legal, which is only true if
                // nothing about the predecessor moved.
                publish(&mut owner, legal(), &mut published, &configs)
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", shape.what));
                continue;
            };
            let applied = publish(&mut owner, config.clone(), &mut published, &configs)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", shape.what));
            assert_eq!(applied.egress, egress, "{}", shape.what);
            assert_eq!(owner.previous.as_ref(), Some(&config), "{}", shape.what);
        }
        assert!(published.try_recv().is_err(), "one config, one publication");
    }

    /// The refusals that are not about the raw egress shape, on the first config and on a successor.
    ///
    /// "Advances nothing" is observed rather than asserted about a field: after each refusal the *same*
    /// successor that was legal before is still legal, which is only true if `previous` never moved.
    #[tokio::test]
    async fn a_refused_config_publishes_nothing_and_advances_nothing() {
        let (configs, mut published) = mpsc::channel(4);

        for (why, config) in [
            (
                "an unset counter",
                ShizukuSessionConfig {
                    downstream_epoch: 0,
                    ..base()
                },
            ),
            (
                "a gateway the interface does not hold",
                ShizukuSessionConfig {
                    gateway_addresses: vec![vec![10, 0, 0, 1]],
                    ..base()
                },
            ),
        ] {
            let mut owner = owner();
            assert!(
                owner.publish(config, &configs).await.is_err(),
                "{why} must be terminal"
            );
            assert!(published.try_recv().is_err(), "{why} reached the dataplane");
            assert!(
                owner.previous.is_none(),
                "{why} advanced what the next config is checked against"
            );
            // And the owner is still a *first*-config owner: a config with no predecessor rules applied is
            // still accepted, which a half-advanced owner would refuse.
            assert!(owner.publish(base(), &configs).await.is_ok(), "{why}");
            published.try_recv().expect("the good config published");
        }

        // The same, as a *successor*: the refusal must not disturb the predecessor already accepted. The
        // predecessor names a network but no interface, so an interface arriving on its own is expressible.
        let mut owner = owner();
        let predecessor = ShizukuSessionConfig {
            upstream_network: Some(0x1234),
            ..base()
        };
        publish(&mut owner, predecessor.clone(), &mut published, &configs)
            .await
            .expect("accepted");
        for (why, bad) in [
            // The handle moving with no generation to retire the sockets bound behind it.
            (
                "a network-only change",
                ShizukuSessionConfig {
                    sequence: 2,
                    upstream_network: Some(0x5678),
                    ..predecessor.clone()
                },
            ),
            // The index moving on its own, which is the same fact and needs the same axis.
            (
                "an interface-only change",
                ShizukuSessionConfig {
                    sequence: 2,
                    upstream_interface_index: Some(7),
                    ..predecessor.clone()
                },
            ),
            // A floor moving with no epoch to retire the queue it was sized against.
            (
                "a floor-only change",
                ShizukuSessionConfig {
                    sequence: 2,
                    downstream_mtu_floor: 1280,
                    ..predecessor.clone()
                },
            ),
            (
                "a sequence that repeated",
                ShizukuSessionConfig {
                    sequence: 1,
                    ..predecessor.clone()
                },
            ),
        ] {
            assert!(owner.publish(bad, &configs).await.is_err(), "{why}");
            assert!(
                published.try_recv().is_err(),
                "{why} reached a transport or the resolver"
            );
            assert_eq!(
                owner.previous.as_ref(),
                Some(&predecessor),
                "{why} disturbed the predecessor"
            );
        }
    }

    /// The axes, through the real owner: what needs a generation, what needs an epoch, and what needs
    /// neither.
    #[tokio::test]
    async fn the_axes_gate_exactly_what_they_retire() {
        let (configs, mut published) = mpsc::channel(4);
        let mut owner = owner();
        publish(&mut owner, base(), &mut published, &configs)
            .await
            .expect("accepted");

        // A generation that advances while the fields stay equal is legal - the app advances it on a
        // `LinkProperties` change that leaves the handle alone.
        let applied = publish(
            &mut owner,
            ShizukuSessionConfig {
                sequence: 2,
                upstream_generation: 2,
                ..base()
            },
            &mut published,
            &configs,
        )
        .await
        .expect("a generation-only advance is legal");
        assert_eq!(applied.upstream_generation, 2);

        // An admit-only change moves no axis at all.
        let applied = publish(
            &mut owner,
            ShizukuSessionConfig {
                sequence: 3,
                upstream_generation: 2,
                admit: false,
                ..base()
            },
            &mut published,
            &configs,
        )
        .await
        .expect("an admit-only change retires nothing");
        assert!(!applied.admitting);

        // The handle arriving, with the generation that retires what was bound before it. This is the same
        // change the test above refuses without one.
        let applied = publish(
            &mut owner,
            ShizukuSessionConfig {
                sequence: 4,
                upstream_generation: 3,
                upstream_network: Some(0x1234),
                ..base()
            },
            &mut published,
            &configs,
        )
        .await
        .expect("a network change with its generation is legal");
        assert_eq!(applied.egress.selected_network, Some(0x1234));
        assert_eq!(
            applied.egress.relay_upstream, None,
            "the relays are not served without the interface their replies arrive on"
        );

        // And the index arriving on its own, likewise: same handle, one more generation.
        let applied = publish(
            &mut owner,
            ShizukuSessionConfig {
                sequence: 5,
                upstream_generation: 4,
                upstream_network: Some(0x1234),
                upstream_interface_index: Some(7),
                ..base()
            },
            &mut published,
            &configs,
        )
        .await
        .expect("an interface change with its generation is legal");
        assert_eq!(
            applied.egress.relay_upstream,
            Some(RelayUpstream {
                network: 0x1234,
                interface: 7,
            })
        );

        // A floor change carried by the epoch that retires the queue.
        let applied = publish(
            &mut owner,
            ShizukuSessionConfig {
                sequence: 6,
                upstream_generation: 4,
                upstream_network: Some(0x1234),
                upstream_interface_index: Some(7),
                downstream_epoch: 2,
                downstream_mtu_floor: 1280,
                ..base()
            },
            &mut published,
            &configs,
        )
        .await
        .expect("a floor change with its epoch is legal");
        assert_eq!(applied.downstream_mtu_floor, 1280);
        assert_eq!(applied.downstream_epoch, 2);
    }
}
