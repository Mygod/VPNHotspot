use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use vpnhotspotd::shared::ipsec::{ForwardPolicyTargetScanner, IpSecForwardPolicyTarget};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::platform;
use crate::process_io::{append_limited, read_limited};

const DUMPSYS: &str = "/system/bin/dumpsys";

pub(crate) async fn scan(interfaces: &[String]) -> io::Result<Vec<IpSecForwardPolicyTarget>> {
    if interfaces.is_empty() || platform::android_api_level() < 31 {
        return Ok(Vec::new());
    }
    let interfaces_detail = interfaces.join(",");
    let mut child = Command::new(DUMPSYS)
        .arg("ipsec")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_report_context_details(
            "ipsec.dumpsys.spawn",
            [("interfaces", interfaces_detail.as_str())],
        )?;
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::other("missing dumpsys stdout").with_report_context_details(
            "ipsec.dumpsys.stdout",
            [("interfaces", interfaces_detail.as_str())],
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::other("missing dumpsys stderr").with_report_context_details(
            "ipsec.dumpsys.stderr",
            [("interfaces", interfaces_detail.as_str())],
        )
    })?;
    let stderr_task = tokio::spawn(read_limited(stderr));
    let mut scanner = ForwardPolicyTargetScanner::new(interfaces.iter().map(String::as_str));
    let mut stdout = BufReader::new(stdout);
    let mut stdout_sample = Vec::new();
    let mut buffer = Vec::new();
    let mut parse_error = None;
    while stdout
        .read_until(b'\n', &mut buffer)
        .await
        .with_report_context_details(
            "ipsec.dumpsys.stdout",
            [("interfaces", interfaces_detail.as_str())],
        )?
        != 0
    {
        append_limited(&mut stdout_sample, &buffer);
        if parse_error.is_none() {
            if let Err(error) = scanner.push_str(&String::from_utf8_lossy(&buffer)) {
                parse_error = Some(error.with_report_context_details(
                    "ipsec.parse",
                    [("interfaces", interfaces_detail.as_str())],
                ));
            }
        }
        buffer.clear();
    }
    let status = child.wait().await.with_report_context_details(
        "ipsec.dumpsys.wait",
        [("interfaces", interfaces_detail.as_str())],
    )?;
    let stderr = stderr_task
        .await
        .map_err(|e| {
            io::Error::other(e.to_string()).with_report_context_details(
                "ipsec.dumpsys.stderr",
                [("interfaces", interfaces_detail.as_str())],
            )
        })?
        .with_report_context_details(
            "ipsec.dumpsys.stderr",
            [("interfaces", interfaces_detail.as_str())],
        )?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{DUMPSYS} ipsec exited with {} stdout={} stderr={}",
            status,
            String::from_utf8_lossy(&stdout_sample).trim_end(),
            String::from_utf8_lossy(&stderr).trim_end(),
        ))
        .with_report_context_details(
            "ipsec.dumpsys.status",
            [("interfaces", interfaces_detail.as_str())],
        ));
    }
    if let Some(error) = parse_error {
        return Err(error);
    }
    scanner
        .finish()
        .with_report_context_details("ipsec.parse", [("interfaces", interfaces_detail.as_str())])
}
