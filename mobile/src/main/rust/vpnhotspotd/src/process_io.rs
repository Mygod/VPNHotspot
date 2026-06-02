use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

const ERROR_OUTPUT_SAMPLE_LIMIT: usize = 4096;

pub(crate) async fn read_limited(mut input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
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

pub(crate) fn append_limited(output: &mut Vec<u8>, input: &[u8]) {
    let remaining = ERROR_OUTPUT_SAMPLE_LIMIT.saturating_sub(output.len());
    if remaining > 0 {
        output.extend_from_slice(&input[..input.len().min(remaining)]);
    }
}
