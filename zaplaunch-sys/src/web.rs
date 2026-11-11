//! Small, bounded HTTP downloads using Windows' TLS and proxy support.
use std::{ffi::c_void, ptr::{null, null_mut}, time::Instant};

type Handle = *mut c_void;
const WINHTTP_OPTION_DISABLE_FEATURE: u32 = 63;
const WINHTTP_DISABLE_COOKIES: u32 = 1;
const WINHTTP_DISABLE_REDIRECTS: u32 = 2;
#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> Handle;
    fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
    fn WinHttpOpenRequest(connection: Handle, verb: *const u16, path: *const u16, version: *const u16, referer: *const u16, accepts: *const *const u16, flags: u32) -> Handle;
    fn WinHttpSetTimeouts(handle: Handle, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
    fn WinHttpSetOption(handle: Handle, option: u32, buffer: *const c_void, length: u32) -> i32;
    fn WinHttpSendRequest(request: Handle, headers: *const u16, length: u32, data: Handle, data_len: u32, total: u32, context: usize) -> i32;
    fn WinHttpReceiveResponse(request: Handle, reserved: Handle) -> i32;
    fn WinHttpQueryHeaders(request: Handle, level: u32, name: *const u16, buffer: Handle, length: *mut u32, index: *mut u32) -> i32;
    fn WinHttpReadData(request: Handle, buffer: Handle, length: u32, read: *mut u32) -> i32;
    fn WinHttpCloseHandle(handle: Handle) -> i32;
}
struct OwnedHandle(Handle);
impl OwnedHandle {
    fn new(handle: Handle) -> Option<Self> { if handle.is_null() { None } else { Some(Self(handle)) } }
}
impl Drop for OwnedHandle {
    fn drop(&mut self) { unsafe { WinHttpCloseHandle(self.0); } }
}

/// No credentials/cookies, at most 1 MiB, TLS validation stays enabled.
pub fn download(host: &str, port: u16, path: &str, secure: bool) -> Option<Vec<u8>> {
    if host.contains('\0') || path.contains('\0') { return None; }
    let started = Instant::now();
    unsafe {
        let agent = super::wide("ZapLaunch/1.0");
        let session = OwnedHandle::new(WinHttpOpen(agent.as_ptr(), 4, null(), null(), 0))?;
        if WinHttpSetTimeouts(session.0, 3000, 3000, 3000, 3000) == 0 { return None; }
        let host = super::wide(host);
        let connection = OwnedHandle::new(WinHttpConnect(session.0, host.as_ptr(), port, 0))?;
        let path = super::wide(path);
        let verb = super::wide("GET");
        let request = OwnedHandle::new(WinHttpOpenRequest(connection.0, verb.as_ptr(), path.as_ptr(), null(), null(), null(), if secure { 0x00800000 } else { 0 }))?;
        // Never automatically authenticate against a site discovered in a button.
        let policy = 2u32; // WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH
        if WinHttpSetOption(request.0, 77, &policy as *const _ as *const c_void, 4) == 0 { return None; }
        let disabled = WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_REDIRECTS;
        if WinHttpSetOption(request.0, WINHTTP_OPTION_DISABLE_FEATURE, &disabled as *const _ as *const c_void, 4) == 0 { return None; }
        if WinHttpSendRequest(request.0, null(), 0, null_mut(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0 { return None; }
        let mut status = 0u32;
        let mut size = 4;
        if WinHttpQueryHeaders(request.0, 19 | 0x20000000, null(), &mut status as *mut _ as Handle, &mut size, null_mut()) == 0 || status != 200 { return None; }
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            if started.elapsed().as_secs() >= 15 { return None; }
            let mut read = 0;
            if WinHttpReadData(request.0, chunk.as_mut_ptr() as Handle, chunk.len() as u32, &mut read) == 0 { return None; }
            if read == 0 { return Some(bytes); }
            if bytes.len() + read as usize > 1024 * 1024 { return None; }
            bytes.extend_from_slice(&chunk[..read as usize]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read, Write}, net::TcpListener};
    #[test]
    fn download_checks_status_and_size() {
        for (status, body, expected) in [("200 OK", vec![42; 32], true), ("404 Not Found", vec![42; 32], false), ("200 OK", vec![42; 1024 * 1024 + 1], false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = [0; 4096];
                let count = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /favicon.ico HTTP/1.1"));
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            });
            let result = download("127.0.0.1", port, "/favicon.ico", false);
            assert_eq!(result.is_some(), expected);
            if expected { assert_eq!(result.unwrap(), vec![42; 32]); }
            server.join().unwrap();
        }
    }
    #[test]
    fn download_does_not_follow_redirect_to_another_origin() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirected = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let destination = redirected.local_addr().unwrap();
        redirected.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{destination}/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert!(download("127.0.0.1", port, "/favicon.ico", false).is_none());
        assert_eq!(redirected.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        server.join().unwrap();
    }
    use std::time::Duration;
}
