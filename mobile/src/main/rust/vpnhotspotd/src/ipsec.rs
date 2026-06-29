use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use vpnhotspotd::shared::ipsec::{ForwardPolicyTargetScanner, IpSecForwardPolicyTarget};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::platform;
use crate::process_io::{append_limited, read_limited};

const DUMPSYS: &str = "/system/bin/dumpsys";

pub(crate) async fn scan() -> io::Result<Vec<IpSecForwardPolicyTarget>> {
    if platform::android_api_level() < 31 {
        return Ok(Vec::new());
    }
    let mut child = Command::new(DUMPSYS)
        .arg("ipsec")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_report_context("ipsec.dumpsys.spawn")?;
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::other("missing dumpsys stdout").with_report_context("ipsec.dumpsys.stdout")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::other("missing dumpsys stderr").with_report_context("ipsec.dumpsys.stderr")
    })?;
    let stderr_task = tokio::spawn(read_limited(stderr));
    let mut scanner = ForwardPolicyTargetScanner::new();
    let mut stdout = BufReader::new(stdout);
    let mut stdout_sample = Vec::new();
    let mut buffer = Vec::new();
    let mut parse_error = None;
    while stdout
        .read_until(b'\n', &mut buffer)
        .await
        .with_report_context("ipsec.dumpsys.stdout")?
        != 0
    {
        append_limited(&mut stdout_sample, &buffer);
        if parse_error.is_none() {
            if let Err(error) = scanner.push_str(&String::from_utf8_lossy(&buffer)) {
                parse_error = Some(error.with_report_context("ipsec.parse"));
            }
        }
        buffer.clear();
    }
    let status = child
        .wait()
        .await
        .with_report_context("ipsec.dumpsys.wait")?;
    let stderr = stderr_task
        .await
        .map_err(|e| io::Error::other(e.to_string()).with_report_context("ipsec.dumpsys.stderr"))?
        .with_report_context("ipsec.dumpsys.stderr")?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "{DUMPSYS} ipsec exited with {} stdout={} stderr={}",
            status,
            String::from_utf8_lossy(&stdout_sample).trim_end(),
            String::from_utf8_lossy(&stderr).trim_end(),
        ))
        .with_report_context("ipsec.dumpsys.status"));
    }
    if let Some(error) = parse_error {
        return Err(error);
    }
    scanner.finish().with_report_context("ipsec.parse")
}
