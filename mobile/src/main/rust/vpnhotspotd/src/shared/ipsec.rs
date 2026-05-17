use std::collections::{HashMap, HashSet};
use std::io;
use std::net::Ipv4Addr;

use regex::Regex;

use super::model::SessionConfig;

#[derive(Debug, Eq, PartialEq)]
pub struct IpSecForwardPolicyTarget {
    pub interface: String,
    pub uid: i32,
    pub source_address: String,
    pub destination_address: String,
    pub mark_value: i32,
    pub xfrm_interface_id: i32,
}

#[derive(Default)]
pub struct UpstreamTracker {
    sessions: HashMap<u64, HashSet<String>>,
    refcounts: HashMap<String, usize>,
}

impl UpstreamTracker {
    pub fn update_session(&mut self, session_id: u64, config: &SessionConfig) -> Vec<String> {
        let next = config
            .primary_upstream_interfaces
            .iter()
            .chain(config.fallback_upstream_interfaces.iter())
            .cloned()
            .collect::<HashSet<_>>();
        let previous = self.sessions.remove(&session_id).unwrap_or_default();
        for interface in previous.difference(&next) {
            if let Some(count) = self.refcounts.get_mut(interface) {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(interface);
                }
            }
        }
        let mut entered = Vec::new();
        for interface in next.difference(&previous) {
            let count = self.refcounts.entry(interface.clone()).or_insert(0);
            if *count == 0 {
                entered.push(interface.clone());
            }
            *count += 1;
        }
        if !next.is_empty() {
            self.sessions.insert(session_id, next);
        }
        entered
    }

    pub fn remove_session(&mut self, session_id: u64) {
        let Some(previous) = self.sessions.remove(&session_id) else {
            return;
        };
        for interface in previous {
            if let Some(count) = self.refcounts.get_mut(&interface) {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(&interface);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.sessions.clear();
        self.refcounts.clear();
    }

    pub fn session_for_interface(&self, interface: &str) -> Option<u64> {
        self.sessions.iter().find_map(|(session_id, interfaces)| {
            interfaces.contains(interface).then_some(*session_id)
        })
    }
}

pub fn find_forward_policy_targets<'a>(
    interfaces: impl IntoIterator<Item = &'a str>,
    dump: &str,
) -> io::Result<Vec<IpSecForwardPolicyTarget>> {
    let interfaces = interfaces.into_iter().collect::<HashSet<_>>();
    if interfaces.is_empty() {
        return Ok(Vec::new());
    }
    let tunnel_record = Regex::new(
        r"(?s)\{mResource=\{super=\{mResourceId=(\d+),\s*pid=\d+,\s*uid=(\d+)\},\s*mInterfaceName=([^,]+),.*?mLocalAddress=[^,]+,\s*mRemoteAddress=[^,]+,\s*mIkey=(\d+),\s*mOkey=\d+\}",
    )
    .expect("valid tunnel record regex");
    let transform_record = Regex::new(
        r"(?s)mConfig=\{mMode=TUNNEL,\s*mSourceAddress=([^,]+),\s*mDestinationAddress=([^,]+),\s*mNetwork=([^,]+),.*?mXfrmInterfaceId=(\d+)\}",
    )
    .expect("valid transform record regex");
    let transforms = transform_record.captures_iter(dump).collect::<Vec<_>>();
    let mut targets = Vec::new();
    for tunnel in tunnel_record.captures_iter(dump) {
        if !interfaces.contains(&tunnel[3]) {
            continue;
        }
        let xfrm_interface_id = parse_i32(&tunnel[1], "tunnel resource id")?;
        let Some(inbound) = transforms.iter().find(|transform| {
            &transform[3] == "null"
                && matches!(
                    parse_i32(&transform[4], "transform xfrm interface id"),
                    Ok(id) if id == xfrm_interface_id
                )
                && is_ipv4(&transform[1])
                && is_ipv4(&transform[2])
        }) else {
            continue;
        };
        targets.push(IpSecForwardPolicyTarget {
            interface: tunnel[3].to_owned(),
            uid: parse_i32(&tunnel[2], "tunnel uid")?,
            source_address: inbound[1].to_owned(),
            destination_address: inbound[2].to_owned(),
            mark_value: parse_i32(&tunnel[4], "tunnel input key")?,
            xfrm_interface_id,
        });
    }
    Ok(targets)
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

    #[test]
    fn ignores_non_ipsec_interfaces() {
        assert_eq!(
            find_forward_policy_targets(["wlan0"].into_iter(), DUMP).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn extracts_forward_policy_target() {
        assert_eq!(
            find_forward_policy_targets(["ipsec1"].into_iter(), DUMP).unwrap(),
            vec![IpSecForwardPolicyTarget {
                interface: "ipsec1".to_owned(),
                uid: 1000,
                source_address: "162.120.192.11".to_owned(),
                destination_address: "10.0.0.62".to_owned(),
                mark_value: 64512,
                xfrm_interface_id: 1,
            }]
        );
    }

    #[test]
    fn ignores_non_ipv4_inbound_transform() {
        let dump = DUMP.replace(
            "mSourceAddress=162.120.192.11",
            "mSourceAddress=2001:db8::1",
        );
        assert_eq!(
            find_forward_policy_targets(["ipsec1"].into_iter(), &dump).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn tracker_dedupes_and_rechecks_after_reentry() {
        let mut tracker = UpstreamTracker::default();
        assert_eq!(
            tracker.update_session(1, &session_config(["ipsec1"], ["ipsec1"])),
            vec!["ipsec1".to_owned()]
        );
        assert_eq!(
            tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])),
            vec!["wlan0".to_owned()]
        );
        tracker.remove_session(1);
        assert_eq!(
            tracker.update_session(2, &session_config(["wlan0"], [])),
            Vec::<String>::new()
        );
        assert_eq!(
            tracker.update_session(2, &session_config(["ipsec1"], ["wlan0"])),
            vec!["ipsec1".to_owned()]
        );
        assert_eq!(tracker.session_for_interface("ipsec1"), Some(2));
        tracker.remove_session(2);
        assert_eq!(tracker.session_for_interface("ipsec1"), None);
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
            clients: Vec::new(),
            ipv6_nat: None,
        }
    }
}
