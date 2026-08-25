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

use crate::control_wire::{recv_packet, spawn_writer};
use crate::report::{self, ControllerSender, ControllerSenderExt};
use crate::shizuku::budget;
use crate::shizuku::tun_reader::{self, Applied};
use crate::shizuku::tun_writer;
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
/// The exception is what keeps this from doubling up. [crate::shizuku::tun_writer] converges every fatal way out of
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
