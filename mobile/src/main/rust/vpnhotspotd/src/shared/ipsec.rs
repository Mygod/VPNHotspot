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
    active_probe: bool,
    emitted_targets: HashSet<IpSecForwardPolicyTarget>,
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
    pub fn update_session(&mut self, session_id: u64, config: &SessionConfig) -> bool {
        let next = SessionUpstreams::from_config(config);
        let previous = self.sessions.get(&session_id).cloned().unwrap_or_default();
        if previous == next {
            return false;
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
        let probe = upstream_changed && !next.interfaces.is_empty() && !self.active_probe;
        if probe {
            self.active_probe = true;
        }
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

    pub fn clear(&mut self) {
        self.sessions.clear();
        self.refcounts.clear();
        self.active_probe = false;
        self.emitted_targets.clear();
    }

    pub fn finish_probe(&mut self) {
        self.active_probe = false;
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

    #[test]
    fn tracker_rechecks_when_upstream_interfaces_change() {
        let mut tracker = UpstreamTracker::default();
        assert!(tracker.update_session(1, &session_config(["ipsec1"], ["ipsec1"])));
        tracker.finish_probe();
        assert!(tracker.update_session(2, &session_config(["wlan0"], ["ipsec1"])));
        assert!(!tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])));
        tracker.finish_probe();
        assert!(tracker.update_session(2, &session_config(["ipsec1"], ["wlan1"])));
        tracker.finish_probe();
        assert!(tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])));
        tracker.finish_probe();
        assert!(tracker.update_session(2, &session_config(["wlan0"], [])));
        tracker.finish_probe();
        tracker.remove_session(1);
        assert!(tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])));
        assert_eq!(tracker.session_for_interface("ipsec1"), Some(2));
        tracker.remove_session(2);
        assert_eq!(tracker.session_for_interface("ipsec1"), None);
    }

    #[test]
    fn tracker_rechecks_when_upstream_generation_changes() {
        let mut tracker = UpstreamTracker::default();
        let mut config = session_config(["ipsec1"], ["wlan0"]);
        config.upstream_generation = 1;
        assert!(tracker.update_session(1, &config));
        assert!(!tracker.update_session(1, &config));

        config.upstream_generation = 2;
        assert!(!tracker.update_session(1, &config));
        config.upstream_generation = 3;
        assert!(!tracker.update_session(1, &config));
        tracker.finish_probe();
        config.upstream_generation = 4;
        assert!(tracker.update_session(1, &config));
        tracker.finish_probe();

        let mut fallback = session_config([], ["ipsec1"]);
        fallback.upstream_generation = 1;
        assert!(tracker.update_session(2, &fallback));
        fallback.upstream_generation = 2;
        assert!(!tracker.update_session(2, &fallback));
        tracker.finish_probe();
        fallback.upstream_generation = 3;
        assert!(tracker.update_session(2, &fallback));
    }

    #[test]
    fn tracker_coalesces_to_one_global_probe() {
        let mut tracker = UpstreamTracker::default();
        assert!(tracker.update_session(1, &session_config(["ipsec1"], [])));
        assert!(!tracker.update_session(2, &session_config(["wlan0"], [])));
        tracker.finish_probe();
        assert!(tracker.update_session(2, &session_config(["wlan1"], [])));
    }

    #[test]
    fn tracker_emits_each_observed_target_once() {
        let mut tracker = UpstreamTracker::default();
        assert!(tracker.update_session(1, &session_config(["ipsec1"], [])));
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
