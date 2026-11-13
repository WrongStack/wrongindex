//! Byte transport for JS clients. Verification happens on the exact pipe
//! handle before any stdin byte (including authentication) is forwarded.
use std::time::Duration;
use wrongindex_ipc::{DataRoot, Discovery};

#[cfg(windows)]
pub async fn run(root: &DataRoot) -> std::io::Result<()> {
    let discovery = Discovery::load(root)?;
    let pipe =
        wrongindex_ipc::win::connect(&discovery.info.endpoint, Duration::from_secs(5)).await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut input, &mut writer) => { r?; }
        r = tokio::io::copy(&mut reader, &mut output) => { r?; }
    }
    Ok(())
}

#[cfg(not(windows))]
pub async fn run(_root: &DataRoot) -> std::io::Result<()> {
    let _ = Duration::ZERO;
    let _ = std::mem::size_of::<Discovery>();
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native IPC bridge requires Windows",
    ))
}
