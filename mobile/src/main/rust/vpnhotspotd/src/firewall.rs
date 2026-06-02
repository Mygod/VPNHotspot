use std::io;
use std::process::ExitStatus;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

pub(crate) const NDC: &str = "/system/bin/ndc";
const RESTORE_ERROR_OUTPUT_LIMIT: usize = 4096;

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
    mut line: impl FnMut(&str),
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
    let stderr_task = tokio::spawn(read_limited(stderr));
    let mut stdout = BufReader::new(stdout);
    let mut stdout_sample = Vec::new();
    let mut buffer = Vec::new();
    while stdout
        .read_until(b'\n', &mut buffer)
        .await
        .with_report_context_details("firewall.restore.stdout", restore_details(target, input))?
        != 0
    {
        append_limited(&mut stdout_sample, &buffer);
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        line(&String::from_utf8_lossy(&buffer));
        buffer.clear();
    }
    let status = child
        .wait()
        .await
        .with_report_context_details("firewall.restore.wait", restore_details(target, input))?;
    let stderr = stderr_task
        .await
        .map_err(|e| {
            io::Error::other(e.to_string()).with_report_context_details(
                "firewall.restore.stderr",
                restore_details(target, input),
            )
        })?
        .with_report_context_details("firewall.restore.stderr", restore_details(target, input))?;
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
    child
        .wait_with_output()
        .await
        .with_report_context_details("firewall.restore.wait", restore_details(target, input))
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
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    io::Error::other(format!(
        "{binary} exited with {} stdout={} stderr={}",
        status,
        stdout.trim_end(),
        stderr.trim_end(),
    ))
    .with_report_context_details("firewall.restore.status", restore_details(target, input))
}

async fn read_limited(mut input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            return Ok(result);
        }
        append_limited(&mut result, &buffer[..read]);
    }
}

fn append_limited(output: &mut Vec<u8>, input: &[u8]) {
    let remaining = RESTORE_ERROR_OUTPUT_LIMIT.saturating_sub(output.len());
    if remaining > 0 {
        output.extend_from_slice(&input[..input.len().min(remaining)]);
    }
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
