use std::io;
use std::process::Stdio;

use tokio::process::Command;
use vpnhotspotd::shared::ipsec::{ForwardPolicyTargetScanner, IpSecForwardPolicyTarget};
use vpnhotspotd::shared::process_io::{read_limited, read_limited_with};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::root::platform;

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
    let output = async move {
        let mut scanner = ForwardPolicyTargetScanner::new();
        let mut parse_error = None;
        let stdout_sample = read_limited_with(stdout, |chunk| {
            if parse_error.is_none() {
                if let Err(error) = scanner.push_str(&String::from_utf8_lossy(chunk)) {
                    parse_error = Some(error.with_report_context("ipsec.parse"));
                }
            }
        })
        .await
        .with_report_context("ipsec.dumpsys.stdout")?;
        let status = child
            .wait()
            .await
            .with_report_context("ipsec.dumpsys.wait")?;
        Ok::<_, io::Error>((status, stdout_sample, parse_error, scanner))
    };
    let stderr = async move {
        read_limited(stderr)
            .await
            .with_report_context("ipsec.dumpsys.stderr")
    };
    let ((status, stdout_sample, parse_error, scanner), stderr) = tokio::try_join!(output, stderr)?;
    if !status.success() {
        return Err(
            io::Error::other(format!("{DUMPSYS} ipsec exited with {status}"))
                .with_report_context_details(
                    "ipsec.dumpsys.status",
                    [
                        ("stdout", String::from_utf8_lossy(&stdout_sample).trim_end()),
                        ("stderr", String::from_utf8_lossy(&stderr).trim_end()),
                    ],
                ),
        );
    }
    if let Some(error) = parse_error {
        return Err(error);
    }
    scanner.finish().with_report_context("ipsec.parse")
}
