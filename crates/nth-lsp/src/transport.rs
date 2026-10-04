//! The LSP base protocol: every message is a `Content-Length` header, a
//! blank line and that many bytes of JSON.

use std::io;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads one message body. `None` means the stream ended cleanly between
/// messages; ending inside one is an error.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut line = String::new();
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            if first {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        first = false;
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        // Content-Type is the only other header, and it is always utf-8 JSON.
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let parsed = value.trim().parse::<usize>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad Content-Length: {e}"),
                )
            })?;
            length = Some(parsed);
        }
    }
    let length = length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "message without Content-Length")
    })?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::BufReader;

    use super::*;

    #[tokio::test]
    async fn round_trips_messages_back_to_back() {
        let (client, server) = tokio::io::duplex(1024);
        let (_, mut write) = tokio::io::split(client);
        let (read, _) = tokio::io::split(server);
        let mut read = BufReader::new(read);

        write_message(&mut write, &json!({"a": "é"})).await.unwrap();
        write_message(&mut write, &json!([1, 2])).await.unwrap();
        drop(write);

        let one: Value =
            serde_json::from_slice(&read_message(&mut read).await.unwrap().unwrap()).unwrap();
        let two: Value =
            serde_json::from_slice(&read_message(&mut read).await.unwrap().unwrap()).unwrap();
        assert_eq!(one, json!({"a": "é"}));
        assert_eq!(two, json!([1, 2]));
    }

    #[tokio::test]
    async fn reads_extra_headers_and_any_case() {
        let raw = b"content-length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}";
        let mut read = BufReader::new(&raw[..]);
        assert_eq!(read_message(&mut read).await.unwrap().unwrap(), b"{}");
        assert!(read_message(&mut read).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cut_off_message_is_an_error() {
        let mut read = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_message(&mut read).await.is_err());
        let mut read = BufReader::new(&b"Content-Length: 10\r\n"[..]);
        assert!(read_message(&mut read).await.is_err());
    }
}
