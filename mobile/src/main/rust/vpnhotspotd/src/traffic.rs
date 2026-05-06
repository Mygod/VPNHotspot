use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub(crate) async fn read_counter_lines() -> io::Result<Vec<String>> {
    let mut child = Command::new("iptables")
        .args(["-w", "-t", "filter", "-nvx", "-L", "vpnhotspot_stats"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing iptables stdout"))?;
    let mut stdout = BufReader::new(stdout);
    let mut lines = Vec::new();
    let mut line = String::new();
    while stdout.read_line(&mut line).await? != 0 {
        lines.push(line.trim_end_matches(['\r', '\n']).to_owned());
        line.clear();
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(lines.into_iter().skip(2).collect())
    } else {
        Err(io::Error::other(format!("iptables exited with {status}")))
    }
}
