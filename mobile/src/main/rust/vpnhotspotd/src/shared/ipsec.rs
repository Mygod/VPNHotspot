use std::collections::{HashMap, HashSet};
use std::io;
use std::net::Ipv4Addr;

use super::model::SessionConfig;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IpSecForwardPolicyTarget {
    pub interface: String,
    pub uid: i32,
    pub source_address: String,
    pub destination_address: String,
    pub mark_value: i32,
    pub xfrm_interface_id: i32,
}

const TUNNEL_RECORD_NEEDLE: &str = "{mResource={super={mResourceId=";
const TRANSFORM_RECORD_NEEDLE: &str = "mConfig={mMode=TUNNEL,";

struct TunnelRecord {
    uid: String,
    interface: String,
    input_key: String,
    xfrm_interface_id: i32,
}

#[derive(Clone)]
struct TransformRecord {
    source_address: String,
    destination_address: String,
}

#[derive(Clone, Copy)]
enum SectionKind {
    Tunnel,
    Transform,
}

#[derive(Default)]
pub struct UpstreamTracker {
    sessions: HashMap<u64, SessionUpstreams>,
    refcounts: HashMap<String, usize>,
    /// The probe that owns the sole flight, named by the generation it was minted at; `None` when nothing is
    /// in flight. One at a time, because a scan reads the whole system's policies and two of them would answer
    /// the same question twice.
    ///
    /// Only the probe named here may end that flight or be handed its rescan. Any other belongs to a flight a
    /// [UpstreamTracker::clear] ended, and letting one of those end or extend the current flight is what would
    /// leave two scans running beside each other - or none at all, with the flight marked busy forever.
    flight: Option<u64>,
    /// An update that arrived while a probe was in flight. That probe may have read the kernel before the
    /// interface it is about existed, so its answer cannot be taken for the new upstream - and dropping the
    /// update would leave a tunnel unpolicied until something else happened to change. Kept here and handed
    /// back by [UpstreamTracker::finish_probe] instead, which turns it into exactly one more scan: however many
    /// updates pile up, the rescan is minted at the newest generation and there is only ever one.
    pending_rescan: bool,
    /// Which set of sessions this tracker currently speaks for. Advanced by every replacement a scan has to
    /// answer for and by [UpstreamTracker::clear], so a [Probe] minted at an older one saw a system that
    /// predates them and is recognisable as stale by comparison.
    generation: u64,
    emitted_targets: HashSet<IpSecForwardPolicyTarget>,
}

/// One probe this tracker asked for, and the sessions it speaks for.
///
/// Handed back to [UpstreamTracker::finish_probe]. Not a token a caller can invent: the only way to have one
/// is to have been told to probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Probe(u64);

/// What one probe's completion leaves behind.
pub struct Finished {
    /// Whether what the probe saw still speaks for this tracker's sessions.
    ///
    /// `false` for a probe the sessions moved out from under - a [UpstreamTracker::clear], or a replacement
    /// this scan predates - and committing one of those would do real damage rather than merely be stale. The
    /// targets it did not see include the ones a newer probe has already sent to the app, and forgetting those
    /// is what makes them be sent again; the ones it did see it would attribute from a session set that has
    /// since changed, which is how a policy ends up published for the wrong session and then marked as
    /// already sent.
    pub current: bool,
    /// The rescan owed to an update that arrived while this probe was running. `Some` at most once per probe,
    /// because taking it here is what keeps the flight going rather than starting a second one beside it.
    ///
    /// Independent of [Finished::current]: a probe whose answer is discarded still owns the flight, and the
    /// rescan is how the update that discarded it gets answered. Whoever gets one *must* run it - the flight
    /// has been handed to it - or nothing will ever probe again short of a clear.
    pub rescan: Option<Probe>,
}

#[derive(Clone, Default, Eq, PartialEq)]
struct SessionUpstreams {
    interfaces: HashSet<String>,
    upstream_generation: u64,
}

impl SessionUpstreams {
    fn from_config(config: &SessionConfig) -> Self {
        Self {
            interfaces: config
                .primary_upstream_interfaces
                .iter()
                .chain(config.fallback_upstream_interfaces.iter())
                .cloned()
                .collect(),
            upstream_generation: config.upstream_generation,
        }
    }
}

impl UpstreamTracker {
    /// Takes one session's upstreams and answers with the probe they call for, if any.
    ///
    /// `None` covers two different situations and neither loses the update: nothing changed that a scan could
    /// answer, or a scan is already running and this update has been recorded as the rescan it owes.
    pub fn update_session(&mut self, session_id: u64, config: &SessionConfig) -> Option<Probe> {
        let next = SessionUpstreams::from_config(config);
        let previous = self.sessions.get(&session_id).cloned().unwrap_or_default();
        if previous == next {
            return None;
        }
        let upstream_changed = previous.interfaces != next.interfaces
            || previous.upstream_generation != next.upstream_generation;
        for interface in previous.interfaces.difference(&next.interfaces) {
            if let Some(count) = self.refcounts.get_mut(interface) {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(interface);
                }
            }
        }
        for interface in next.interfaces.difference(&previous.interfaces) {
            let count = self.refcounts.entry(interface.clone()).or_insert(0);
            *count += 1;
        }
        self.emitted_targets
            .retain(|target| self.refcounts.contains_key(&target.interface));
        let probe = if !upstream_changed || next.interfaces.is_empty() {
            // Nothing a scan could answer. The upstreams that went away took their refcounts and emitted
            // targets with them above, and a probe already in flight is still answering for the sessions that
            // are left - attribution and retention are both read from the tracker as it is when it commits.
            None
        } else {
            // A replacement a scan has to answer for, so what any scan already in flight saw predates it.
            self.generation = self.generation.wrapping_add(1);
            if self.flight.is_some() {
                self.pending_rescan = true;
                None
            } else {
                self.flight = Some(self.generation);
                Some(Probe(self.generation))
            }
        };
        if next.interfaces.is_empty() {
            self.sessions.remove(&session_id);
        } else {
            self.sessions.insert(session_id, next);
        }
        probe
    }

    pub fn remove_session(&mut self, session_id: u64) {
        let Some(previous) = self.sessions.remove(&session_id) else {
            return;
        };
        for interface in previous.interfaces {
            if let Some(count) = self.refcounts.get_mut(&interface) {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(&interface);
                }
            }
        }
        self.emitted_targets
            .retain(|target| self.refcounts.contains_key(&target.interface));
    }

    /// Drops every session this tracker speaks for: a clean, or the process stopping.
    ///
    /// The generation moves with it, as it does for any other replacement, and the *flight* ends with it -
    /// which no other replacement does. That is what makes this the one place two probes can overlap: the one
    /// that was in flight, and the one the next update is free to start. Nothing is owed to the old one, so
    /// the pending rescan goes too: it belonged to sessions that no longer exist, and replaying it would scan
    /// for nothing.
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.refcounts.clear();
        self.flight = None;
        self.pending_rescan = false;
        self.generation = self.generation.wrapping_add(1);
        self.emitted_targets.clear();
    }

    /// Ends `probe`'s flight and says what may be done with what it saw. See [Finished].
    pub fn finish_probe(&mut self, probe: Probe) -> Finished {
        if self.flight != Some(probe.0) {
            // Started for sessions that have since been dropped wholesale. The flight it belonged to ended
            // with them, so this does not end the current one either, and nothing is owed to it: whatever is
            // owed belongs to the flight a later update started.
            return Finished {
                current: false,
                rescan: None,
            };
        }
        // Stale if anything replaced the sessions it was minted for, which is exactly what set the rescan
        // below - so what it saw is discarded and the replay is what answers for the change.
        let current = probe.0 == self.generation;
        if self.pending_rescan {
            // The flight continues rather than ending and being started again, so an update arriving now is
            // still recorded as a rescan instead of racing a second probe into existence.
            self.pending_rescan = false;
            self.flight = Some(self.generation);
            return Finished {
                current,
                rescan: Some(Probe(self.generation)),
            };
        }
        self.flight = None;
        Finished {
            current,
            rescan: None,
        }
    }

    pub fn session_for_interface(&self, interface: &str) -> Option<u64> {
        self.sessions.iter().find_map(|(session_id, interfaces)| {
            interfaces
                .interfaces
                .contains(interface)
                .then_some(*session_id)
        })
    }

    pub fn retain_observed_targets(&mut self, targets: &[IpSecForwardPolicyTarget]) {
        let targets = targets.iter().cloned().collect::<HashSet<_>>();
        self.emitted_targets.retain(|target| {
            self.refcounts.contains_key(&target.interface) && targets.contains(target)
        });
    }

    pub fn session_for_new_target(&mut self, target: &IpSecForwardPolicyTarget) -> Option<u64> {
        let session_id = self.session_for_interface(&target.interface)?;
        self.emitted_targets
            .insert(target.clone())
            .then_some(session_id)
    }
}

pub fn find_forward_policy_targets(dump: &str) -> io::Result<Vec<IpSecForwardPolicyTarget>> {
    let mut scanner = ForwardPolicyTargetScanner::new();
    scanner.push_str(dump)?;
    scanner.finish()
}

#[derive(Default)]
pub struct ForwardPolicyTargetScanner {
    transforms: HashMap<i32, TransformRecord>,
    tunnels: Vec<TunnelRecord>,
    buffer: String,
}

impl ForwardPolicyTargetScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_str(&mut self, chunk: &str) -> io::Result<()> {
        self.buffer.push_str(chunk);
        self.drain_sections()
    }

    pub fn finish(mut self) -> io::Result<Vec<IpSecForwardPolicyTarget>> {
        self.drain_sections()?;
        let mut targets = Vec::new();
        for tunnel in self.tunnels {
            let Some(inbound) = self.transforms.get(&tunnel.xfrm_interface_id) else {
                continue;
            };
            targets.push(IpSecForwardPolicyTarget {
                interface: tunnel.interface,
                uid: parse_i32(&tunnel.uid, "tunnel uid")?,
                source_address: inbound.source_address.clone(),
                destination_address: inbound.destination_address.clone(),
                mark_value: parse_i32(&tunnel.input_key, "tunnel input key")?,
                xfrm_interface_id: tunnel.xfrm_interface_id,
            });
        }
        Ok(targets)
    }

    fn drain_sections(&mut self) -> io::Result<()> {
        loop {
            let Some((start, kind, needle)) = next_section(&self.buffer) else {
                retain_needle_suffix(&mut self.buffer);
                return Ok(());
            };
            let brace_offset = needle.find('{').expect("needle contains opening brace");
            let section_start = start + brace_offset;
            let Some(len) = braced_section_len(&self.buffer[section_start..]) else {
                if start > 0 {
                    self.buffer.drain(..start);
                }
                return Ok(());
            };
            let section_end = section_start + len;
            let section = self.buffer[section_start..section_end].to_owned();
            self.buffer.drain(..section_end);
            match kind {
                SectionKind::Tunnel => {
                    self.process_nested_transform_records(&section);
                    self.process_tunnel_record(&section)?;
                }
                SectionKind::Transform => self.process_transform_record(&section),
            }
        }
    }

    fn process_tunnel_record(&mut self, record: &str) -> io::Result<()> {
        let Some((resource_id, after_resource_id)) = digits_after(record, "mResourceId=") else {
            return Ok(());
        };
        let Some((_, after_pid)) = digits_after(after_resource_id, "pid=") else {
            return Ok(());
        };
        let Some((uid, after_uid)) = digits_after(after_pid, "uid=") else {
            return Ok(());
        };
        let Some((interface, after_interface)) = field_after(after_uid, "mInterfaceName=") else {
            return Ok(());
        };
        let Some((_, after_local)) = field_after(after_interface, "mLocalAddress=") else {
            return Ok(());
        };
        let Some((_, after_remote)) = field_after(after_local, "mRemoteAddress=") else {
            return Ok(());
        };
        let Some((input_key, after_input_key)) = digits_after(after_remote, "mIkey=") else {
            return Ok(());
        };
        if digits_after(after_input_key, "mOkey=").is_none() {
            return Ok(());
        }
        self.tunnels.push(TunnelRecord {
            uid: uid.to_owned(),
            interface: interface.to_owned(),
            input_key: input_key.to_owned(),
            xfrm_interface_id: parse_i32(resource_id, "tunnel resource id")?,
        });
        Ok(())
    }

    fn process_nested_transform_records(&mut self, record: &str) {
        let brace_offset = TRANSFORM_RECORD_NEEDLE
            .find('{')
            .expect("needle contains opening brace");
        let mut offset = 0;
        while let Some(start) = record[offset..]
            .find(TRANSFORM_RECORD_NEEDLE)
            .map(|start| offset + start)
        {
            let section = &record[start + brace_offset..];
            let Some(len) = braced_section_len(section) else {
                return;
            };
            self.process_transform_record(&section[..len]);
            offset = start + brace_offset + len;
        }
    }

    fn process_transform_record(&mut self, record: &str) {
        let Some((source_address, after_source)) = field_after(record, "mSourceAddress=") else {
            return;
        };
        let Some((destination_address, after_destination)) =
            field_after(after_source, "mDestinationAddress=")
        else {
            return;
        };
        let Some((network, after_network)) = field_after(after_destination, "mNetwork=") else {
            return;
        };
        if network != "null" || !is_ipv4(source_address) || !is_ipv4(destination_address) {
            return;
        }
        let Some((xfrm_interface_id, _)) = digits_after(after_network, "mXfrmInterfaceId=") else {
            return;
        };
        let Ok(xfrm_interface_id) = parse_i32(xfrm_interface_id, "transform xfrm interface id")
        else {
            return;
        };
        self.transforms
            .entry(xfrm_interface_id)
            .or_insert_with(|| TransformRecord {
                source_address: source_address.to_owned(),
                destination_address: destination_address.to_owned(),
            });
    }
}

fn next_section(buffer: &str) -> Option<(usize, SectionKind, &'static str)> {
    match (
        buffer
            .find(TUNNEL_RECORD_NEEDLE)
            .map(|start| (start, SectionKind::Tunnel, TUNNEL_RECORD_NEEDLE)),
        buffer
            .find(TRANSFORM_RECORD_NEEDLE)
            .map(|start| (start, SectionKind::Transform, TRANSFORM_RECORD_NEEDLE)),
    ) {
        (Some(tunnel), Some(transform))
            if tunnel.0 <= transform.0
                && buffer[tunnel.0..transform.0].contains("mInterfaceName=") =>
        {
            Some(tunnel)
        }
        (Some(_), Some(transform)) => Some(transform),
        (Some(tunnel), None) => Some(tunnel),
        (None, Some(transform)) => Some(transform),
        (None, None) => None,
    }
}

fn retain_needle_suffix(buffer: &mut String) {
    let keep = [TUNNEL_RECORD_NEEDLE, TRANSFORM_RECORD_NEEDLE]
        .into_iter()
        .flat_map(|needle| {
            (1..needle.len())
                .rev()
                .find(|len| buffer.ends_with(&needle[..*len]))
        })
        .max()
        .unwrap_or_default();
    if keep < buffer.len() {
        buffer.drain(..buffer.len() - keep);
    }
}

fn braced_section_len(section: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in section.bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn field_after<'a>(record: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let start = record.find(name)? + name.len();
    let value = &record[start..];
    let end = value.find(',')?;
    Some((&value[..end], &value[end..]))
}

fn digits_after<'a>(record: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let start = record.find(name)? + name.len();
    let value = &record[start..];
    let end = value.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &value[end..];
    (end > 0 && matches!(rest.as_bytes().first(), Some(b',' | b'}')))
        .then_some((&value[..end], rest))
}

fn parse_i32(value: &str, name: &str) -> io::Result<i32> {
    value.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {name} {value}: {e}"),
        )
    })
}

fn is_ipv4(address: &str) -> bool {
    address.parse::<Ipv4Addr>().is_ok()
}

#[cfg(test)]
mod tests {
    use cidr::Ipv6Cidr;

    use super::*;
    use crate::shared::model::SessionConfig;
    use crate::shared::proto::daemon::MasqueradeMode;

    const DUMP: &str = r#"
IpSecService dump:

mUserResourceTracker:
{1000={mSpiQuotaTracker={mCurrent=2, mMax=64}, mTransformQuotaTracker={mCurrent=2, mMax=64}, mSocketQuotaTracker={mCurrent=1, mMax=16}, mTunnelQuotaTracker={mCurrent=1, mMax=8}, mSpiRecords={}, mTransformRecords={5={mResource={super={mResourceId=5, pid=1763, uid=1000}, mSocket={super={mResourceId=2, pid=1763, uid=1000}, mSocket=java.io.FileDescriptor@8d12f25, mPort=39573}, mSpi.mResourceId=4, mConfig={mMode=TUNNEL, mSourceAddress=10.0.0.62, mDestinationAddress=162.120.192.11, mNetwork=100, mEncapType=2, mEncapSocketResourceId=2, mEncapRemotePort=4500, mNattKeepaliveInterval=0{mSpiResourceId=4, mEncryption=null, mAuthentication=null, mAuthenticatedEncryption={mName=rfc4106(gcm(aes)), mTruncLenBits=128}, mMarkValue=0, mMarkMask=0, mXfrmInterfaceId=1}}, mRefCount=1, mChildren=[]}, 6={mResource={super={mResourceId=6, pid=1763, uid=1000}, mSocket={super={mResourceId=2, pid=1763, uid=1000}, mSocket=java.io.FileDescriptor@8d12f25, mPort=39573}, mSpi.mResourceId=3, mConfig={mMode=TUNNEL, mSourceAddress=162.120.192.11, mDestinationAddress=10.0.0.62, mNetwork=null, mEncapType=2, mEncapSocketResourceId=2, mEncapRemotePort=4500, mNattKeepaliveInterval=0{mSpiResourceId=3, mEncryption=null, mAuthentication=null, mAuthenticatedEncryption={mName=rfc4106(gcm(aes)), mTruncLenBits=128}, mMarkValue=0, mMarkMask=0, mXfrmInterfaceId=1}}, mRefCount=1, mChildren=[]}}}, mEncapSocketRecords={2={mResource={super={mResourceId=2, pid=1763, uid=1000}, mSocket=java.io.FileDescriptor@8d12f25, mPort=39573}, mRefCount=3, mChildren=[]}}, mTunnelInterfaceRecords={1={mResource={super={mResourceId=1, pid=1763, uid=1000}, mInterfaceName=ipsec1, mUnderlyingNetwork=100, mLocalAddress=127.0.0.1, mRemoteAddress=127.0.0.1, mIkey=64512, mOkey=64513}, mRefCount=1, mChildren=[]}}}}
"#;

    const DUMP_WITH_TRANSFORM_CHILDREN: &str = r#"
IpSecService dump:

mUserResourceTracker:
{1000={mSpiQuotaTracker={mCurrent=2, mMax=64}, mTransformQuotaTracker={mCurrent=2, mMax=64}, mSocketQuotaTracker={mCurrent=1, mMax=16}, mTunnelQuotaTracker={mCurrent=1, mMax=8}, mSpiRecords={}, mTransformRecords={47={mResource={super={mResourceId=47, pid=1788, uid=1000}, mSocket={super={mResourceId=44, pid=1788, uid=1000}, mSocket=java.io.FileDescriptor@9515a7a, mPort=36084}, mSpi.mResourceId=46, mConfig={mMode=TUNNEL, mSourceAddress=10.0.0.62, mDestinationAddress=162.120.192.11, mNetwork=128, mEncapType=2, mEncapSocketResourceId=44, mEncapRemotePort=4500, mNattKeepaliveInterval=0{mSpiResourceId=46, mEncryption=null, mAuthentication=null, mAuthenticatedEncryption={mName=rfc4106(gcm(aes)), mTruncLenBits=128}, mMarkValue=0, mMarkMask=0, mXfrmInterfaceId=43}}, mRefCount=1, mChildren=[{mResource={super={mResourceId=44, pid=1788, uid=1000}, mSocket=java.io.FileDescriptor@9515a7a, mPort=36084}, mRefCount=3, mChildren=[]}, {mResource={super={mResourceId=46, pid=1788, uid=1000}, mSpi=189336555, mSourceAddress=, mDestinationAddress=162.120.192.11, mOwnedByTransform=true}, mRefCount=1, mChildren=[]}]}, 48={mResource={super={mResourceId=48, pid=1788, uid=1000}, mSocket={super={mResourceId=44, pid=1788, uid=1000}, mSocket=java.io.FileDescriptor@9515a7a, mPort=36084}, mSpi.mResourceId=45, mConfig={mMode=TUNNEL, mSourceAddress=162.120.192.11, mDestinationAddress=10.0.0.62, mNetwork=null, mEncapType=2, mEncapSocketResourceId=44, mEncapRemotePort=4500, mNattKeepaliveInterval=0{mSpiResourceId=45, mEncryption=null, mAuthentication=null, mAuthenticatedEncryption={mName=rfc4106(gcm(aes)), mTruncLenBits=128}, mMarkValue=0, mMarkMask=0, mXfrmInterfaceId=43}}, mRefCount=1, mChildren=[{mResource={super={mResourceId=44, pid=1788, uid=1000}, mSocket=java.io.FileDescriptor@9515a7a, mPort=36084}, mRefCount=3, mChildren=[]}, {mResource={super={mResourceId=45, pid=1788, uid=1000}, mSpi=1900022394, mSourceAddress=, mDestinationAddress=10.0.0.62, mOwnedByTransform=true}, mRefCount=1, mChildren=[]}]}}, mEncapSocketRecords={44={mResource={super={mResourceId=44, pid=1788, uid=1000}, mSocket=java.io.FileDescriptor@9515a7a, mPort=36084}, mRefCount=3, mChildren=[]}}, mTunnelInterfaceRecords={43={mResource={super={mResourceId=43, pid=1788, uid=1000}, mInterfaceName=ipsec43, mUnderlyingNetwork=128, mLocalAddress=127.0.0.1, mRemoteAddress=127.0.0.1, mIkey=64522, mOkey=64523}, mRefCount=1, mChildren=[]}}}}
"#;

    #[test]
    fn tracker_ignores_targets_outside_current_upstreams() {
        let mut tracker = UpstreamTracker::default();
        tracker.update_session(1, &session_config(["wlan0"], []));
        assert_eq!(tracker.session_for_new_target(&expected_target()), None);
    }

    #[test]
    fn extracts_forward_policy_target() {
        assert_eq!(
            find_forward_policy_targets(DUMP).unwrap(),
            vec![expected_target()]
        );
    }

    #[test]
    fn extracts_forward_policy_target_with_transform_children() {
        assert_eq!(
            find_forward_policy_targets(DUMP_WITH_TRANSFORM_CHILDREN).unwrap(),
            vec![IpSecForwardPolicyTarget {
                interface: "ipsec43".to_owned(),
                uid: 1000,
                source_address: "162.120.192.11".to_owned(),
                destination_address: "10.0.0.62".to_owned(),
                mark_value: 64522,
                xfrm_interface_id: 43,
            }]
        );
    }

    #[test]
    fn extracts_forward_policy_target_from_streamed_chunks() {
        let mut scanner = ForwardPolicyTargetScanner::new();
        for chunk in DUMP.as_bytes().chunks(17) {
            scanner
                .push_str(std::str::from_utf8(chunk).unwrap())
                .unwrap();
        }
        assert_eq!(scanner.finish().unwrap(), vec![expected_target()]);
    }

    #[test]
    fn ignores_non_ipv4_inbound_transform() {
        let dump = DUMP.replace(
            "mSourceAddress=162.120.192.11",
            "mSourceAddress=2001:db8::1",
        );
        assert_eq!(find_forward_policy_targets(&dump).unwrap(), Vec::new());
    }

    #[test]
    fn ignores_incomplete_tunnel_records() {
        let dump = DUMP.replace("mIkey=64512, ", "");
        assert_eq!(find_forward_policy_targets(&dump).unwrap(), Vec::new());
    }

    #[test]
    fn reports_invalid_target_tunnel_resource_id() {
        let dump = DUMP.replace(
            "mTunnelInterfaceRecords={1={mResource={super={mResourceId=1, pid=1763, uid=1000}",
            "mTunnelInterfaceRecords={1={mResource={super={mResourceId=999999999999, pid=1763, uid=1000}",
        );
        let error = find_forward_policy_targets(&dump).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("invalid tunnel resource id 999999999999"));
    }

    /// The probe an update asks for, for a test that is about which updates ask for one.
    fn probed(probe: Option<Probe>) -> Probe {
        probe.expect("this update has to ask for a probe")
    }

    /// Ends the flight of the probe an update asked for, which is what the owner does when a scan comes back.
    fn finish(tracker: &mut UpstreamTracker, probe: Probe) {
        let finished = tracker.finish_probe(probe);
        assert!(finished.current, "this probe still speaks for its sessions");
        assert!(finished.rescan.is_none(), "nothing asked for a rescan");
    }

    #[test]
    fn tracker_rechecks_when_upstream_interfaces_change() {
        let mut tracker = UpstreamTracker::default();
        let probe = probed(tracker.update_session(1, &session_config(["ipsec1"], ["ipsec1"])));
        finish(&mut tracker, probe);
        let probe = probed(tracker.update_session(2, &session_config(["wlan0"], ["ipsec1"])));
        // Primary and fallback swapped, which is the same set of interfaces at the same generation: nothing a
        // scan could answer differently, so nothing is asked for and nothing is owed.
        assert!(tracker
            .update_session(2, &session_config(["ipsec1"], ["wlan0"]))
            .is_none());
        finish(&mut tracker, probe);
        let probe = probed(tracker.update_session(2, &session_config(["ipsec1"], ["wlan1"])));
        finish(&mut tracker, probe);
        let probe = probed(tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])));
        finish(&mut tracker, probe);
        let probe = probed(tracker.update_session(2, &session_config(["wlan0"], [])));
        finish(&mut tracker, probe);
        tracker.remove_session(1);
        let probe = probed(tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])));
        finish(&mut tracker, probe);
        assert_eq!(tracker.session_for_interface("ipsec1"), Some(2));
        tracker.remove_session(2);
        assert_eq!(tracker.session_for_interface("ipsec1"), None);
    }

    #[test]
    fn tracker_rechecks_when_upstream_generation_changes() {
        let mut tracker = UpstreamTracker::default();
        let mut config = session_config(["ipsec1"], ["wlan0"]);
        config.upstream_generation = 1;
        let probe = probed(tracker.update_session(1, &config));
        assert!(tracker.update_session(1, &config).is_none());

        config.upstream_generation = 2;
        assert!(tracker.update_session(1, &config).is_none());
        config.upstream_generation = 3;
        assert!(tracker.update_session(1, &config).is_none());
        let rescan = tracker
            .finish_probe(probe)
            .rescan
            .expect("the generations that moved during the probe are owed a rescan");
        finish(&mut tracker, rescan);
        config.upstream_generation = 4;
        let probe = probed(tracker.update_session(1, &config));
        finish(&mut tracker, probe);

        let mut fallback = session_config([], ["ipsec1"]);
        fallback.upstream_generation = 1;
        let probe = probed(tracker.update_session(2, &fallback));
        fallback.upstream_generation = 2;
        assert!(tracker.update_session(2, &fallback).is_none());
        let rescan = tracker
            .finish_probe(probe)
            .rescan
            .expect("the generation that moved during the probe is owed a rescan");
        finish(&mut tracker, rescan);
        fallback.upstream_generation = 3;
        assert!(tracker.update_session(2, &fallback).is_some());
    }

    #[test]
    fn tracker_coalesces_to_one_global_probe() {
        let mut tracker = UpstreamTracker::default();
        let probe = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        assert!(tracker
            .update_session(2, &session_config(["wlan0"], []))
            .is_none());
        let rescan = tracker
            .finish_probe(probe)
            .rescan
            .expect("the second session's update is owed a rescan");
        finish(&mut tracker, rescan);
        assert!(tracker
            .update_session(2, &session_config(["wlan1"], []))
            .is_some());
    }

    /// An update that arrives while a probe is running cannot be answered by that probe: it may have read the
    /// kernel's policies before the interface existed. So that probe's answer is *discarded* and the update is
    /// replayed, exactly once, however many updates piled up - and the flight stays open in the meantime, so
    /// nothing starts a second scan beside it.
    #[test]
    fn an_update_during_a_probe_discards_it_and_is_replayed_exactly_once() {
        let mut tracker = UpstreamTracker::default();
        let probe = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        for interface in ["ipsec2", "ipsec3", "ipsec4"] {
            assert!(
                tracker
                    .update_session(2, &session_config([interface], []))
                    .is_none(),
                "{interface} must not start a second scan beside the one running"
            );
        }
        let finished = tracker.finish_probe(probe);
        assert!(
            !finished.current,
            "the sessions moved under this probe, so nothing it saw may be committed"
        );
        let rescan = finished
            .rescan
            .expect("the updates are owed exactly one rescan");
        // Exactly one, and it speaks for the newest of those updates rather than for the one that opened the
        // window: the rescan is current, so what *it* sees is what commits.
        // The flight is still open, so an update arriving during the rescan is recorded rather than racing.
        assert!(tracker
            .update_session(3, &session_config(["ipsec5"], []))
            .is_none());
        let finished = tracker.finish_probe(rescan);
        assert!(!finished.current, "and the same is true of the rescan");
        let rescan = finished
            .rescan
            .expect("the update during the rescan is owed one of its own");
        finish(&mut tracker, rescan);
        // And now that nothing is in flight, the next update starts a scan of its own again.
        assert!(tracker
            .update_session(3, &session_config(["ipsec6"], []))
            .is_some());
    }

    /// The completion a replacement makes dangerous, which is the same damage a clean's does: a probe that
    /// predates the replacement, coming back afterwards. Committing it would forget an emitted target - and
    /// forgetting one is what makes it be sent twice - and publish what it did see from a session set that has
    /// since changed.
    #[test]
    fn a_probe_from_before_a_replacement_commits_nothing() {
        let mut tracker = UpstreamTracker::default();
        let stale = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        let target = expected_target();
        // Sent to the app by an earlier scan of this same flight, and therefore not to be sent again.
        assert_eq!(tracker.session_for_new_target(&target), Some(1));

        // The replacement this stale probe predates: session 1 keeps ipsec1 and gains one, so ipsec1 is still
        // refcounted and the stale answer would still look committable.
        assert!(tracker
            .update_session(1, &session_config(["ipsec1", "ipsec2"], []))
            .is_none());
        let finished = tracker.finish_probe(stale);
        assert!(
            !finished.current,
            "a probe from before a replacement speaks for the session set it predates"
        );
        // So the caller retains and publishes nothing from it, and the target the app already has stays
        // emitted rather than being forgotten and sent a second time.
        assert_eq!(tracker.session_for_new_target(&target), None);

        // One rescan, and it is the one that speaks for the replacement.
        let rescan = finished.rescan.expect("the replacement is owed a rescan");
        let finished = tracker.finish_probe(rescan);
        assert!(
            finished.current,
            "the rescan speaks for the new session set"
        );
        assert!(
            finished.rescan.is_none(),
            "and nothing more is owed, so the flight ends here"
        );
        tracker.retain_observed_targets(std::slice::from_ref(&target));
        assert_eq!(tracker.session_for_new_target(&target), None);
    }

    /// A change a scan cannot answer - a session's upstreams going away - neither discards the scan in flight
    /// nor asks for another. The one in flight still speaks for the sessions that are left, and a rescan that
    /// nothing owes would be a scan for nothing.
    #[test]
    fn an_upstream_that_goes_away_neither_discards_nor_replays() {
        // Session 1 opens the tracker and its probe is finished first, so the flight this test is about is
        // the one session 2's arrival mints - and session 2 is then deliberately removed, and separately
        // emptied, *before* that probe finishes. Finishing session 1's probe up front is what keeps the
        // rescan its arrival would otherwise owe out of the way, so the only thing happening during the
        // flight under test is the departure.
        let mut tracker = UpstreamTracker::default();
        let opening = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        finish(&mut tracker, opening);
        let probe = probed(tracker.update_session(2, &session_config(["ipsec2"], [])));

        // The departure, in flight. A session losing its last upstream takes its refcounts and emitted
        // targets with it, and that is all: nothing a scan could answer has changed, so the answer already
        // being fetched still speaks for the sessions that remain.
        tracker.remove_session(2);
        let finished = tracker.finish_probe(probe);
        assert!(
            finished.current,
            "a departure does not make an in-flight probe stale"
        );
        assert!(
            finished.rescan.is_none(),
            "and it does not ask for a scan of its own"
        );
        assert_eq!(tracker.session_for_interface("ipsec1"), Some(1));
        assert_eq!(tracker.session_for_interface("ipsec2"), None);

        // The same departure spelled as an empty config rather than a removal, which is the other way a
        // session's upstreams go away.
        let probe = probed(tracker.update_session(2, &session_config(["ipsec2"], [])));
        assert!(
            tracker.update_session(2, &session_config([], [])).is_none(),
            "losing every upstream asks for no probe"
        );
        let finished = tracker.finish_probe(probe);
        assert!(finished.current, "and does not discard the one in flight");
        assert!(finished.rescan.is_none(), "and does not replay it");
        assert_eq!(tracker.session_for_interface("ipsec1"), Some(1));
        assert_eq!(tracker.session_for_interface("ipsec2"), None);
    }

    /// The completion that would do real damage: a probe started before a clean, coming back after the next
    /// one has already told the app about a target. Committing it would forget that target, and forgetting it
    /// is what makes it be sent twice.
    #[test]
    fn a_probe_from_before_a_clean_commits_nothing() {
        let mut tracker = UpstreamTracker::default();
        let stale = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        // The clean drops every session this tracker spoke for, so the probe in flight speaks for nothing.
        tracker.clear();
        let current = probed(tracker.update_session(2, &session_config(["ipsec1"], [])));
        let target = expected_target();
        assert_eq!(tracker.session_for_new_target(&target), Some(2));

        // The stale probe saw the system before that tunnel existed. Its answer ends here.
        let finished = tracker.finish_probe(stale);
        assert!(!finished.current, "a probe from before a clean is stale");
        assert!(finished.rescan.is_none());
        // Nothing it saw was committed, so the target the app already has is not sent again.
        assert_eq!(tracker.session_for_new_target(&target), None);
        // And the current probe is still in flight: the stale one did not end its flight either.
        assert!(tracker
            .update_session(2, &session_config(["ipsec1", "ipsec2"], []))
            .is_none());
        let rescan = tracker
            .finish_probe(current)
            .rescan
            .expect("the update during the current probe is owed a rescan");
        finish(&mut tracker, rescan);
        assert_eq!(tracker.session_for_new_target(&target), None);
    }

    /// A rescan owed when a clean arrives is owed for sessions the clean drops, so it goes with them. The
    /// flight the next update is then free to start must not inherit a replay for a session set that no longer
    /// exists, and the stale completion must neither commit nor schedule anything.
    #[test]
    fn a_clean_drops_the_rescan_it_found_owed() {
        let mut tracker = UpstreamTracker::default();
        let stale = probed(tracker.update_session(1, &session_config(["ipsec1"], [])));
        assert!(tracker
            .update_session(2, &session_config(["ipsec3"], []))
            .is_none());
        tracker.clear();
        let current = probed(tracker.update_session(3, &session_config(["ipsec1"], [])));
        let finished = tracker.finish_probe(stale);
        assert!(!finished.current, "a probe from before a clean is stale");
        assert!(
            finished.rescan.is_none(),
            "and it schedules nothing: its flight ended with the sessions it spoke for"
        );
        // Current, and owed nothing: the pre-clean update's replay went with the clean rather than being
        // handed to the flight that replaced it.
        finish(&mut tracker, current);
    }

    #[test]
    fn tracker_emits_each_observed_target_once() {
        let mut tracker = UpstreamTracker::default();
        assert!(tracker
            .update_session(1, &session_config(["ipsec1"], []))
            .is_some());
        let target = expected_target();
        tracker.retain_observed_targets(std::slice::from_ref(&target));
        assert_eq!(tracker.session_for_new_target(&target), Some(1));
        assert_eq!(tracker.session_for_new_target(&target), None);

        let mut changed = target.clone();
        changed.mark_value += 1;
        tracker.retain_observed_targets(std::slice::from_ref(&changed));
        assert_eq!(tracker.session_for_new_target(&changed), Some(1));
    }

    #[test]
    fn tracker_clears_emitted_target_after_interface_disappears() {
        let mut tracker = UpstreamTracker::default();
        tracker.update_session(1, &session_config(["ipsec1"], []));
        let target = expected_target();
        assert_eq!(tracker.session_for_new_target(&target), Some(1));
        tracker.update_session(1, &session_config(["wlan0"], []));
        tracker.update_session(1, &session_config(["ipsec1"], []));
        assert_eq!(tracker.session_for_new_target(&target), Some(1));
    }

    fn session_config(
        primary_upstream_interfaces: impl IntoIterator<Item = &'static str>,
        fallback_upstream_interfaces: impl IntoIterator<Item = &'static str>,
    ) -> SessionConfig {
        SessionConfig {
            downstream: "downstream0".to_owned(),
            reply_mark: 0,
            ip_forward: true,
            masquerade: MasqueradeMode::None,
            ipv6_block: false,
            primary_network: Some(1),
            primary_routes: Vec::<Ipv6Cidr>::new(),
            fallback_network: None,
            primary_upstream_interfaces: primary_upstream_interfaces
                .into_iter()
                .map(str::to_owned)
                .collect(),
            fallback_upstream_interfaces: fallback_upstream_interfaces
                .into_iter()
                .map(str::to_owned)
                .collect(),
            upstream_generation: 0,
            clients: Vec::new(),
            ipv6_nat: None,
        }
    }

    fn expected_target() -> IpSecForwardPolicyTarget {
        IpSecForwardPolicyTarget {
            interface: "ipsec1".to_owned(),
            uid: 1000,
            source_address: "162.120.192.11".to_owned(),
            destination_address: "10.0.0.62".to_owned(),
            mark_value: 64512,
            xfrm_interface_id: 1,
        }
    }
}
