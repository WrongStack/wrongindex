//! Length-prefixed JSON framing: `u32` little-endian byte length + UTF-8 JSON.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {size} bytes exceeds the {max}-byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("connection closed mid-frame")]
    Truncated,
    #[error("invalid frame payload: {0}")]
    Invalid(#[from] serde_json::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Write one frame and flush.
pub async fn write_frame<W, T>(w: &mut W, value: &T, max: usize) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let body = serde_json::to_vec(value)?;
    if body.len() > max {
        return Err(FrameError::TooLarge {
            size: body.len(),
            max,
        });
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        size: body.len(),
        max,
    })?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read the 4-byte length header. `Ok(None)` is a clean EOF on a frame boundary.
/// The size is validated before any body byte is buffered.
pub async fn read_header<R>(r: &mut R, max: usize) -> Result<Option<usize>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        let n = r.read(&mut header[filled..]).await?;
        if n == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        filled += n;
    }
    let size = u32::from_le_bytes(header) as usize;
    if size > max {
        return Err(FrameError::TooLarge { size, max });
    }
    Ok(Some(size))
}

pub async fn read_body<R, T>(r: &mut R, size: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut body = vec![0u8; size];
    r.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(serde_json::from_slice(&body)?)
}

/// Read one complete frame. `Ok(None)` is a clean EOF.
pub async fn read_frame<R, T>(r: &mut R, max: usize) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match read_header(r, max).await? {
        None => Ok(None),
        Some(size) => Ok(Some(read_body(r, size).await?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn round_trip_and_clean_eof() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &json!({"x": "çağrı 🚀"}), 1024)
            .await
            .unwrap();
        drop(a);
        let v: Value = read_frame(&mut b, 1024).await.unwrap().unwrap();
        assert_eq!(v["x"], "çağrı 🚀");
        assert!(
            read_frame::<_, Value>(&mut b, 1024)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn oversized_header_rejected_before_body() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(10_000u32).to_le_bytes()).await.unwrap();
        let err = read_frame::<_, Value>(&mut b, 100).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::TooLarge {
                size: 10_000,
                max: 100
            }
        ));
    }

    #[tokio::test]
    async fn truncated_frames_are_errors() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[5, 0]).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame::<_, Value>(&mut b, 100).await.unwrap_err(),
            FrameError::Truncated
        ));

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(10u32).to_le_bytes()).await.unwrap();
        a.write_all(b"{\"a\"").await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame::<_, Value>(&mut b, 100).await.unwrap_err(),
            FrameError::Truncated
        ));
    }

    #[tokio::test]
    async fn writer_refuses_oversized_payload() {
        let (mut a, _b) = tokio::io::duplex(64);
        let big = "x".repeat(200);
        assert!(matches!(
            write_frame(&mut a, &big, 100).await.unwrap_err(),
            FrameError::TooLarge { .. }
        ));
    }
}
