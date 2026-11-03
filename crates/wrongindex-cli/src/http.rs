//! Minimal HTTP(S) GET over WinHTTP, the operating system's own client: TLS,
//! certificate validation, the system proxy and redirects (never HTTPS to
//! HTTP) come from Windows, and no network crate is linked. Used only by
//! `self-update`.

use std::io;

/// Largest body accepted (an installer is a few MiB).
pub const MAX_BODY: usize = 256 * 1024 * 1024;

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

#[cfg(windows)]
pub fn get(url: &str, accept: &str) -> io::Result<Response> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Networking::WinHttp::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle WinHTTP returned, closed once.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }
    fn check(h: *mut c_void, what: &str) -> io::Result<Handle> {
        if h.is_null() {
            let e = io::Error::last_os_error();
            Err(io::Error::new(e.kind(), format!("{what}: {e}")))
        } else {
            Ok(Handle(h))
        }
    }
    fn ok(r: i32, what: &str) -> io::Result<()> {
        if r == 0 {
            let e = io::Error::last_os_error();
            Err(io::Error::new(e.kind(), format!("{what}: {e}")))
        } else {
            Ok(())
        }
    }

    let wurl = wide(url);
    let mut host = vec![0u16; 256];
    let mut path = vec![0u16; 4096];
    let mut extra = vec![0u16; 4096];
    let mut parts = URL_COMPONENTS {
        dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
        lpszHostName: host.as_mut_ptr(),
        dwHostNameLength: host.len() as u32,
        lpszUrlPath: path.as_mut_ptr(),
        dwUrlPathLength: path.len() as u32,
        lpszExtraInfo: extra.as_mut_ptr(),
        dwExtraInfoLength: extra.len() as u32,
        ..Default::default()
    };
    // SAFETY: `wurl` is NUL-terminated; the component buffers outlive the call
    // and their lengths are passed with them.
    ok(
        unsafe { WinHttpCrackUrl(wurl.as_ptr(), 0, 0, &mut parts) },
        "parsing URL",
    )?;
    let secure = parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS;
    host.truncate(parts.dwHostNameLength as usize);
    host.push(0);
    // The object name is the path plus the query ("extra info").
    path.truncate(parts.dwUrlPathLength as usize);
    path.extend_from_slice(&extra[..parts.dwExtraInfoLength as usize]);
    path.push(0);

    let agent = wide(&format!("wrongindex/{}", env!("CARGO_PKG_VERSION")));
    // SAFETY: valid NUL-terminated strings; null proxy arguments are allowed.
    let session = check(
        unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            )
        },
        "opening WinHTTP",
    )?;
    // Resolve, connect, send: 30 s each; receive: 120 s between reads.
    // SAFETY: a live session handle.
    ok(
        unsafe { WinHttpSetTimeouts(session.0, 30_000, 30_000, 30_000, 120_000) },
        "setting timeouts",
    )?;
    // SAFETY: a live session handle and a NUL-terminated host name.
    let connect = check(
        unsafe { WinHttpConnect(session.0, host.as_ptr(), parts.nPort, 0) },
        "connecting",
    )?;
    let verb = wide("GET");
    // SAFETY: live connect handle; NUL-terminated verb and path; null version,
    // referrer and accept types are allowed.
    let request = check(
        unsafe {
            WinHttpOpenRequest(
                connect.0,
                verb.as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                null(),
                if secure { WINHTTP_FLAG_SECURE } else { 0 },
            )
        },
        "opening request",
    )?;
    let headers = wide(&format!("Accept: {accept}\r\n"));
    // SAFETY: live request handle; the header string is NUL-terminated
    // (length -1 = up to the NUL); no request body.
    ok(
        unsafe { WinHttpSendRequest(request.0, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) },
        "sending request",
    )?;
    // SAFETY: live request handle after a successful send.
    ok(
        unsafe { WinHttpReceiveResponse(request.0, null_mut()) },
        "receiving response",
    )?;
    let mut status: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: the buffer is a u32 and its size is passed.
    ok(
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&mut status as *mut u32).cast(),
                &mut len,
                null_mut(),
            )
        },
        "reading status",
    )?;
    let mut body = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let mut read: u32 = 0;
        // SAFETY: `chunk` is writable for its full length.
        ok(
            unsafe {
                WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr().cast(),
                    chunk.len() as u32,
                    &mut read,
                )
            },
            "reading body",
        )?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read as usize]);
        if body.len() > MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response larger than {MAX_BODY} bytes"),
            ));
        }
    }
    Ok(Response { status, body })
}

#[cfg(not(windows))]
pub fn get(_url: &str, _accept: &str) -> io::Result<Response> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "self-update is available on Windows only",
    ))
}
