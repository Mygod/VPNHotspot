use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct TrafficCounters {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl TrafficCounters {
    fn start() -> io::Result<Self> {
        let mut child = Command::new("iptables-restore")
            .args(["-w", "--noflush", "-v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing iptables-restore stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing iptables-restore stdout"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn read_counter_lines(&mut self) -> io::Result<Vec<String>> {
        let marker = "# vpnhotspotd traffic counters\n";
        self.stdin
            .write_all(format!("*filter\n-nvx -L vpnhotspot_stats\nCOMMIT\n{marker}").as_bytes())
            .await?;
        self.stdin.flush().await?;
        let marker = marker.trim_end();
        let mut lines = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line).await? == 0 {
                return Err(match self.child.try_wait()? {
                    Some(status) => {
                        io::Error::other(format!("iptables-restore exited with {status}"))
                    }
                    None => io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "iptables-restore stdout closed",
                    ),
                });
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line == marker {
                return Ok(lines.into_iter().skip(2).collect());
            }
            lines.push(line.to_owned());
        }
    }

    async fn stop(mut self) {
        drop(self.stdin);
        if self.child.wait().await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}

pub(crate) async fn read_counter_lines() -> io::Result<Vec<String>> {
    let mut counters = TrafficCounters::start()?;
    let result = counters.read_counter_lines().await;
    counters.stop().await;
    result
}
