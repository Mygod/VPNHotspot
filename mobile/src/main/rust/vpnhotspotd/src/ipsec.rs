use std::io;

use tokio::process::Command;
use vpnhotspotd::shared::ipsec::{find_forward_policy_targets, IpSecForwardPolicyTarget};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::platform;

const DUMPSYS: &str = "/system/bin/dumpsys";

pub(crate) async fn scan(interfaces: &[String]) -> io::Result<Vec<IpSecForwardPolicyTarget>> {
    if interfaces.is_empty() || platform::android_api_level() < 31 {
        return Ok(Vec::new());
    }
    let interfaces_detail = interfaces.join(",");
    let output = Command::new(DUMPSYS)
        .arg("ipsec")
        .kill_on_drop(true)
        .output()
        .await
        .with_report_context_details(
            "ipsec.dumpsys.spawn",
            [("interfaces", interfaces_detail.as_str())],
        )?;
    let dump = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        return Err(io::Error::other(format!(
            "{DUMPSYS} ipsec exited with {} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end(),
        ))
        .with_report_context_details(
            "ipsec.dumpsys.status",
            [("interfaces", interfaces_detail.as_str())],
        ));
    };
    find_forward_policy_targets(interfaces.iter().map(String::as_str), &dump)
        .with_report_context_details("ipsec.parse", [("interfaces", interfaces_detail.as_str())])
}
