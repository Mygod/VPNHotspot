use std::io;

use tokio::process::Command;

use crate::{firewall, report};
use vpnhotspotd::shared::protocol::IoResultReportExt;

pub(super) async fn add_ip_forward(downstream: &str) -> io::Result<()> {
    if let Err(e) = run_ndc(
        "ipfwd",
        &["ipfwd", "enable", &format!("vpnhotspot_{downstream}")],
    )
    .await
    {
        report::stderr!("ndc ipfwd enable failed: {e}");
        tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").await?;
    }
    Ok(())
}

pub(super) async fn remove_ip_forward(downstream: &str) -> bool {
    if let Err(e) = run_ndc(
        "ipfwd",
        &["ipfwd", "disable", &format!("vpnhotspot_{downstream}")],
    )
    .await
    {
        report::io_with_details(
            "routing.remove_ip_forward",
            e,
            [("downstream", downstream.to_owned())],
        );
        false
    } else {
        true
    }
}

pub(super) async fn run_ndc(name: &str, args: &[&str]) -> io::Result<()> {
    let details = vec![
        ("binary".to_owned(), firewall::NDC.to_owned()),
        ("args".to_owned(), args.join(" ")),
    ];
    let output = Command::new(firewall::NDC)
        .args(args)
        .output()
        .await
        .with_report_context_details("routing.ndc.spawn", details.clone())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let suffix = format!("200 0 {name} operation succeeded\n");
    if output.status.success() && stdout.ends_with(&suffix) {
        if stdout.len() > suffix.len() {
            report::stderr!("ndc {}: {}", args.join(" "), stdout.trim_end());
        }
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ndc {} exited with {} stdout={} stderr={}",
            args.join(" "),
            output.status,
            stdout.trim_end(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )))
        .with_report_context_details("routing.ndc.status", details)
    }
}
