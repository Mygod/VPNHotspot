//! The app-UID session's config calls: what one config has to survive before the dataplane sees it, and what
//! answering one means.
//!
//! The contract is level-triggered: the newest [ShizukuSessionConfig] is the truth, and each config call is
//! answered with the sequence and generation the daemon has actually applied. Nothing here replays history,
//! because the app coalesces and a superseded config is never sent at all.
//!
//! Retirement happens strictly before the reply. Seeing a [ShizukuApplied] carrying a generation means
//! everything bound to the previous one is already gone rather than merely asked to go, which is what the
//! app's ordered stop is built on. The ingress task owns that state, so it is the ingress task that reports
//! the retirement finished - this loop only refuses to reply until it has.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use prost::Message;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{mpsc, oneshot};
use vpnhotspotd::shared::app_control::{self, Rejected, Request};
use vpnhotspotd::shared::egress;
use vpnhotspotd::shared::proto::daemon::{
    self, DaemonErrorReport, ShizukuApplied, ShizukuSessionConfig,
};
use vpnhotspotd::shared::protocol::{
    describe_io_error, shizuku_applied_reply_frame, IoErrorReportExt,
};
use vpnhotspotd::shared::session_config;

use crate::control_wire::recv_packet;
use crate::report::{self, ControllerSender, ControllerSenderExt};
use crate::shizuku::tun_reader::Applied;

/// How the config loop ended, when it did not end with a failure of the session's own.
pub(super) enum Ended {
    /// Nothing more is owed on the start call. Either the app closed the control socket, so there is nobody
    /// left to answer, or it cancelled the start call - which removes the call on the app's side before the
    /// cancel is even written, exactly as the root controller does.
    Silent,
    /// A config was refused, and its own `ErrorFrame` is the answer. The session ends with that failure, but
    /// the start call gets no second frame carrying the same thing: one failure must not reach the app twice.
    ///
    /// Owed rather than written, which is the whole of [refuse]: the app's reader returns on a terminal frame,
    /// so this one has to be the last thing the session writes - after the dataplane has been joined and the
    /// reporter flushed, both of which happen after this loop has returned. So what comes back here is the
    /// answer itself, and [vpnhotspotd::shared::app_terminal::Terminal::answer] is what writes it.
    Refused {
        call_id: u64,
        /// Built where the refusal happened, so its context, errno, details and Rust source location are that
        /// site's rather than the teardown's.
        report: DaemonErrorReport,
        /// The same failure as the session's own result, which is what the process ends with.
        error: io::Error,
    },
}

/// Reads config calls until something ends the session.
///
/// Every refusal below is answered on the call that caused it, which is what makes a rejected config reach
/// the app as itself rather than as the conversation going quiet. Only a failure that names no call at all -
/// a malformed frame, a call ID of zero, a control writer that can no longer carry a frame - comes back as
/// this function's own error, because there is nowhere else for it to go.
pub(super) async fn serve(
    stream: &mut OwnedReadHalf,
    session: u64,
    interface_name: &str,
    gateway: Ipv4Addr,
    configs: &mpsc::Sender<Applied>,
    control: &ControllerSender,
) -> io::Result<Ended> {
    let mut owner = Configs {
        previous: None,
        interface_name: interface_name.to_owned(),
        gateway,
    };
    loop {
        let packet = match recv_packet(stream).await {
            Ok(packet) => packet,
            // EOF on the control socket is the authoritative cancellation signal
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Ended::Silent),
            Err(e) => return Err(e.with_report_context("shizuku.control.recv_packet")),
        };
        let envelope = daemon::ClientEnvelope::decode(packet.as_slice()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, e)
                .with_report_context("shizuku.control.parse_frame")
        })?;
        let (call_id, config) = match app_control::read_request(envelope, session) {
            Ok(Request::Config { call_id, config }) => (call_id, config),
            Ok(Request::CancelSession) => return Ok(Ended::Silent),
            Ok(Request::CancelStale) => continue,
            Err(Rejected {
                call_id: Some(call_id),
                error,
            }) => return Ok(refuse(call_id, error)),
            Err(Rejected {
                call_id: None,
                error,
            }) => return Err(error),
        };
        // Read off the config before it is moved into the dataplane, because this is what the reply says and
        // the reply is written after the dataplane has taken it.
        let applied = ShizukuApplied {
            sequence: config.sequence,
            upstream_generation: config.upstream_generation,
            // Set only from the config and never inferred, because only the app knows the session is ACTIVE.
            admitting: config.admit,
        };
        let retired = match owner.publish(config, configs).await {
            Ok(retired) => retired,
            // Answered on the config call whether the config was malformed or the dataplane behind it is
            // gone: either way it is the call the app is waiting on, so that is where the failure belongs.
            Err(e) => return Ok(refuse(call_id, e)),
        };
        // The reply waits on this, which is the whole ordering guarantee: the app may only believe the
        // previous generation is gone once the task that owns its state says so.
        if retired.await.is_err() {
            return Err(io::Error::other("tun ingress dropped a config it accepted")
                .with_report_context("shizuku.control.config"));
        }
        if !control.send_frame(shizuku_applied_reply_frame(call_id, applied)) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the control writer stopped before the session",
            )
            .with_report_context("shizuku.control.config"));
        }
    }
}

/// Owes one config call the failure that refused it, so the app learns it as itself.
///
/// Describing it here and writing it later are two halves of one answer, and they are apart because only one
/// of them belongs to this loop. The report has to be built where the refusal happened, or it would name the
/// teardown instead of the check; the frame has to be written where the session ends, or it would overtake
/// the reports that teardown has not raised yet - see [Ended::Refused].
///
/// `#[track_caller]` so that a refusal not already carrying a report is located at the check that made it
/// rather than here.
#[track_caller]
fn refuse(call_id: u64, error: io::Error) -> Ended {
    Ended::Refused {
        call_id,
        report: describe_io_error("shizuku.control.config", &error, [("call_id", call_id)]),
        error,
    }
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
    /// holds the peer to that, along with every other shape rule the contract has - including that the
    /// generation never moves backwards, which is why nothing here has to remember it separately.
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
            )
            .with_report_context("shizuku.control.config"));
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
                )
                .with_report_context("shizuku.control.config"));
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
            .with_report_context("shizuku.control.config")
        })?;
        let virtual_addresses = Arc::new(addresses(&config.virtual_addresses)?);
        report::stdout!(
            "applying config {} on {}: generation {}, admitting {}, egress {egress:?}",
            config.sequence,
            self.interface_name,
            config.upstream_generation,
            config.admit,
        );
        let (retired, applied) = oneshot::channel();
        let published = Applied {
            // Set only from the config and never inferred, because only the app knows the session is ACTIVE.
            admitting: config.admit,
            upstream_generation: config.upstream_generation,
            egress,
            virtual_addresses,
            gateway_addresses,
            retired,
        };
        // Last, and only now: everything that could refuse this config has already run, so what the dataplane
        // receives is a config this owner has committed to.
        self.previous = Some(config);
        if configs.send(published).await.is_err() {
            return Err(io::Error::other("tun ingress stopped before the session")
                .with_report_context("shizuku.control.config"));
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
            )
            .with_report_context("shizuku.control.config")),
        })
        .collect()
}
