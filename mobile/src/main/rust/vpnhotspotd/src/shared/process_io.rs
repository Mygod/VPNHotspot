use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Bytes retained from each subprocess stream or line preview for one structured error detail. Crashlytics
/// accepts at most 1,024 characters per value; retaining at most 1,024 source bytes is conservative because
/// conversion to a Rust/Java string cannot produce more Unicode characters than input bytes. Fixed-size reads
/// keep peak drain storage bounded too. Excess bytes are consumed but omitted from diagnostics/line callbacks,
/// so a verbose child cannot block and command completion/status handling is unchanged.
/// https://firebase.google.com/docs/crashlytics/android/customize-crash-reports#add-custom-keys
const ERROR_OUTPUT_SAMPLE_LIMIT: usize = 1024;

pub async fn read_limited(input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    read_limited_with(input, |_| {}).await
}

/// Drains the whole stream through a fixed-size scratch buffer while retaining only the diagnostic prefix.
/// `inspect` sees each bounded chunk and may parse it without an intermediate unbounded line allocation.
pub async fn read_limited_with(
    mut input: impl AsyncRead + Unpin,
    mut inspect: impl FnMut(&[u8]),
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut buffer = [0; ERROR_OUTPUT_SAMPLE_LIMIT];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            return Ok(result);
        }
        let chunk = &buffer[..read];
        append_limited(&mut result, chunk);
        inspect(chunk);
    }
}

/// Drains a stream while presenting at most the first diagnostic-prefix worth of each line. Bytes after that
/// prefix are consumed through the fixed scratch buffer and omitted from the callback; the current consumers
/// need only iptables' leading counter columns or ignore lines entirely.
pub async fn read_limited_lines(
    input: impl AsyncRead + Unpin,
    mut line: impl FnMut(&str),
) -> io::Result<Vec<u8>> {
    let mut current = Vec::new();
    let result = read_limited_with(input, |mut chunk| {
        while let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            append_limited(&mut current, &chunk[..newline]);
            if current.last() == Some(&b'\r') {
                current.pop();
            }
            line(&String::from_utf8_lossy(&current));
            current.clear();
            chunk = &chunk[newline + 1..];
        }
        append_limited(&mut current, chunk);
    })
    .await?;
    if !current.is_empty() {
        line(&String::from_utf8_lossy(&current));
    }
    Ok(result)
}

pub fn append_limited(output: &mut Vec<u8>, input: &[u8]) {
    let remaining = ERROR_OUTPUT_SAMPLE_LIMIT.saturating_sub(output.len());
    if remaining > 0 {
        output.extend_from_slice(&input[..input.len().min(remaining)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn limited_reader_drains_the_stream_after_its_sample_is_full() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let input = (0..ERROR_OUTPUT_SAMPLE_LIMIT * 2 + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = input[..ERROR_OUTPUT_SAMPLE_LIMIT].to_vec();
        let input_len = input.len();
        let write = async move {
            writer.write_all(&input).await.unwrap();
            writer.shutdown().await.unwrap();
            input.len()
        };

        let (written, sample) = tokio::join!(write, read_limited(reader));

        assert_eq!(written, input_len);
        assert_eq!(sample.unwrap(), expected);
    }

    #[tokio::test]
    async fn line_preview_is_bounded_while_the_complete_stream_is_drained() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut input = vec![b'x'; ERROR_OUTPUT_SAMPLE_LIMIT * 2 + 17];
        input.extend_from_slice(b"\ntail\n");
        let expected = input[..ERROR_OUTPUT_SAMPLE_LIMIT].to_vec();
        let write = async move {
            writer.write_all(&input).await.unwrap();
            writer.shutdown().await.unwrap();
        };
        let mut lines = Vec::new();
        let read = read_limited_lines(reader, |line| lines.push(line.to_owned()));

        let ((), sample) = tokio::join!(write, read);

        assert_eq!(sample.unwrap(), expected);
        assert_eq!(
            lines,
            ["x".repeat(ERROR_OUTPUT_SAMPLE_LIMIT), "tail".to_owned()]
        );
    }
}
