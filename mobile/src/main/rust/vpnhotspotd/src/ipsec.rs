use std::io;

use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use vpnhotspotd::shared::ipsec::find_forward_policy_targets;
use vpnhotspotd::shared::protocol::{
    daemon_io_error_report_with_details, ipsec_forward_policy_frame, IoErrorReportExt,
    IoResultReportExt,
};

use crate::{platform, report};

const DUMPSYS: &str = "/system/bin/dumpsys";

pub(crate) async fn scan(call_id: u64, interfaces: Vec<String>, sender: &UnboundedSender<Vec<u8>>) {
    if interfaces.is_empty() || platform::android_api_level() < 31 {
        return;
    }
    let output = match Command::new(DUMPSYS)
        .arg("ipsec")
        .kill_on_drop(true)
        .output()
        .await
        .with_report_context("ipsec.dumpsys.spawn")
    {
        Ok(output) => output,
        Err(e) => {
            report::report_for(
                Some(call_id),
                daemon_io_error_report_with_details(
                    "ipsec.dumpsys",
                    e,
                    [("interfaces", interfaces.join(","))],
                ),
            );
            return;
        }
    };
    let dump = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        report::report_for(
            Some(call_id),
            daemon_io_error_report_with_details(
                "ipsec.dumpsys",
                io::Error::other(format!(
                    "{DUMPSYS} ipsec exited with {} stdout={} stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim_end(),
                    String::from_utf8_lossy(&output.stderr).trim_end(),
                ))
                .with_report_context("ipsec.dumpsys.status"),
                [("interfaces", interfaces.join(","))],
            ),
        );
        return;
    };
    let targets = match find_forward_policy_targets(interfaces.iter().map(String::as_str), &dump) {
        Ok(targets) => targets,
        Err(e) => {
            report::report_for(
                Some(call_id),
                daemon_io_error_report_with_details(
                    "ipsec.parse",
                    e,
                    [("interfaces", interfaces.join(","))],
                ),
            );
            return;
        }
    };
    for target in targets {
        if sender
            .send(ipsec_forward_policy_frame(call_id, &target))
            .is_err()
        {
            report::stderr!("controller send failed");
            return;
        }
    }
}
