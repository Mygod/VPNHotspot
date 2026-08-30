use std::io;
use std::process::ExitStatus;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use vpnhotspotd::shared::process_io::{read_limited, read_limited_lines};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

pub(crate) const NDC: &str = "/system/bin/ndc";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IptablesTarget {
    Ipv4,
    Ipv6,
}

impl IptablesTarget {
    pub(crate) fn restore_binary(self) -> &'static str {
        match self {
            Self::Ipv4 => "/system/bin/iptables-restore",
            Self::Ipv6 => "/system/bin/ip6tables-restore",
        }
    }
}

enum RestoreOutput {
    Status,
    Capture,
}

pub(crate) async fn restore(target: IptablesTarget, input: &str) -> io::Result<()> {
    let output = run_restore(target, input, RestoreOutput::Capture).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(restore_status_error(target, input, &output))
    }
}

pub(crate) async fn restore_status(target: IptablesTarget, input: &str) -> io::Result<bool> {
    Ok(run_restore(target, input, RestoreOutput::Status)
        .await?
        .status
        .success())
}

pub(crate) async fn restore_stdout_lines(
    target: IptablesTarget,
    input: &str,
    line: impl FnMut(&str),
) -> io::Result<()> {
    let binary = target.restore_binary();
    let mut child = Command::new(binary)
        .args(["-w", "--noflush"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_report_context_details("firewall.restore.spawn", restore_details(target, input))?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        io::Error::other("missing restore stdin")
            .with_report_context_details("firewall.restore.stdin", restore_details(target, input))
    })?;
    stdin
        .write_all(input.as_bytes())
        .await
        .with_report_context_details("firewall.restore.write", restore_details(target, input))?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::other("missing restore stdout")
            .with_report_context_details("firewall.restore.stdout", restore_details(target, input))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::other("missing restore stderr")
            .with_report_context_details("firewall.restore.stderr", restore_details(target, input))
    })?;
    let status = async move {
        child
            .wait()
            .await
            .with_report_context_details("firewall.restore.wait", restore_details(target, input))
    };
    let stdout = async move {
        read_limited_lines(stdout, line)
            .await
            .with_report_context_details("firewall.restore.stdout", restore_details(target, input))
    };
    let stderr = async move {
        read_limited(stderr)
            .await
            .with_report_context_details("firewall.restore.stderr", restore_details(target, input))
    };
    // Scoped together: an error in line parsing, waiting, or either drain drops the other futures and the
    // kill-on-drop child rather than detaching a pipe reader.
    let (status, stdout_sample, stderr) = tokio::try_join!(status, stdout, stderr)?;
    if status.success() {
        Ok(())
    } else {
        Err(restore_status_error_parts(
            target,
            input,
            status,
            &stdout_sample,
            &stderr,
        ))
    }
}

async fn run_restore(
    target: IptablesTarget,
    input: &str,
    output: RestoreOutput,
) -> io::Result<std::process::Output> {
    let binary = target.restore_binary();
    let (stdout, stderr) = match output {
        RestoreOutput::Status => (Stdio::null(), Stdio::null()),
        RestoreOutput::Capture => (Stdio::piped(), Stdio::piped()),
    };
    let mut child = Command::new(binary)
        .args(["-w", "--noflush"])
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .with_report_context_details("firewall.restore.spawn", restore_details(target, input))?;
    child
        .stdin
        .take()
        .ok_or_else(|| {
            io::Error::other("missing restore stdin").with_report_context_details(
                "firewall.restore.stdin",
                restore_details(target, input),
            )
        })?
        .write_all(input.as_bytes())
        .await
        .with_report_context_details("firewall.restore.write", restore_details(target, input))?;
    match output {
        RestoreOutput::Status => child
            .wait()
            .await
            .map(|status| std::process::Output {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
            .with_report_context_details("firewall.restore.wait", restore_details(target, input)),
        RestoreOutput::Capture => {
            let stdout = child.stdout.take().ok_or_else(|| {
                io::Error::other("missing restore stdout").with_report_context_details(
                    "firewall.restore.stdout",
                    restore_details(target, input),
                )
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                io::Error::other("missing restore stderr").with_report_context_details(
                    "firewall.restore.stderr",
                    restore_details(target, input),
                )
            })?;
            let status = async move {
                child.wait().await.with_report_context_details(
                    "firewall.restore.wait",
                    restore_details(target, input),
                )
            };
            let stdout = async move {
                read_limited(stdout).await.with_report_context_details(
                    "firewall.restore.stdout",
                    restore_details(target, input),
                )
            };
            let stderr = async move {
                read_limited(stderr).await.with_report_context_details(
                    "firewall.restore.stderr",
                    restore_details(target, input),
                )
            };
            let (status, stdout, stderr) = tokio::try_join!(status, stdout, stderr)?;
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
    }
}

fn restore_status_error(
    target: IptablesTarget,
    input: &str,
    output: &std::process::Output,
) -> io::Error {
    restore_status_error_parts(target, input, output.status, &output.stdout, &output.stderr)
}

fn restore_status_error_parts(
    target: IptablesTarget,
    input: &str,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> io::Error {
    let binary = target.restore_binary();
    let mut details = restore_details(target, input);
    details.push((
        "stdout".to_owned(),
        String::from_utf8_lossy(stdout).trim_end().to_owned(),
    ));
    details.push((
        "stderr".to_owned(),
        String::from_utf8_lossy(stderr).trim_end().to_owned(),
    ));
    io::Error::other(format!("{binary} exited with {status}"))
        .with_report_context_details("firewall.restore.status", details)
}

pub(crate) fn restore_input(table: &str, lines: &[String]) -> String {
    let mut input = String::new();
    input.push('*');
    input.push_str(table);
    input.push('\n');
    for line in lines {
        input.push_str(line);
        input.push('\n');
    }
    input.push_str("COMMIT\n");
    input
}

pub(crate) fn restore_line(operation: &str, chain: &str, args: &[String]) -> io::Result<String> {
    let mut line = String::new();
    append_token(&mut line, operation)?;
    append_token(&mut line, chain)?;
    for arg in args {
        append_token(&mut line, arg)?;
    }
    Ok(line)
}

pub(crate) fn restore_details(target: IptablesTarget, input: &str) -> Vec<(String, String)> {
    vec![
        ("binary".to_owned(), target.restore_binary().to_owned()),
        ("stdin".to_owned(), input.to_owned()),
    ]
}

fn append_token(line: &mut String, token: &str) -> io::Result<()> {
    if token.is_empty() || token.bytes().any(|byte| byte <= b' ' || byte == b'\x7f') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid iptables-restore token {token:?}"),
        ));
    }
    if !line.is_empty() {
        line.push(' ');
    }
    line.push_str(token);
    Ok(())
}
