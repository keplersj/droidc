//! Asks the host for a device's usbfs fd: connect to its abstract socket, check it runs as our uid,
//! send [`proto::request`], and receive the reply line with the fd attached as `SCM_RIGHTS`.

use crate::proto::{self, Refusal};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};

/// Selkie's socket, when `DROIDC_USB_SOCKET` doesn't name another.
pub const DEFAULT_SOCKET: &str = "io.github.keplersj.selkie.usb";

/// The longest reply line read; a host that sends more is not following the protocol.
const MAX_LINE: usize = 64 * 1024;

/// Why no fd came back.
#[derive(Debug)]
pub enum Failure {
    /// Nothing listens on the socket (the host app isn't running), or it isn't our uid.
    Unreachable(io::Error),
    /// The host answered and said no.
    Refused(Refusal),
    /// The exchange broke off.
    Io(io::Error),
}

impl Failure {
    /// The errno the `open()` that asked sets.
    pub fn errno(&self) -> i32 {
        match self {
            Failure::Unreachable(_) => libc::ENODEV,
            Failure::Refused(r) => r.errno(),
            Failure::Io(_) => libc::EIO,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Unreachable(e) => write!(f, "host unreachable: {e}"),
            Failure::Refused(r) => write!(f, "{}: {}", r.kind, r.message),
            Failure::Io(e) => write!(f, "{e}"),
        }
    }
}

/// The socket name from the environment, or [`DEFAULT_SOCKET`].
pub fn socket_name() -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    match std::env::var_os("DROIDC_USB_SOCKET") {
        Some(v) if !v.is_empty() => v.as_bytes().to_vec(),
        _ => DEFAULT_SOCKET.as_bytes().to_vec(),
    }
}

/// The usbfs fd for device `bus`/`dev`, from the host listening on the abstract socket `name`.
pub fn open(name: &[u8], bus: u16, dev: u16) -> Result<OwnedFd, Failure> {
    let addr = SocketAddr::from_abstract_name(name).map_err(Failure::Unreachable)?;
    let sock = UnixStream::connect_addr(&addr).map_err(Failure::Unreachable)?;
    match same_uid(sock.as_fd()) {
        Ok(true) => {}
        Ok(false) => {
            return Err(Failure::Unreachable(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the socket's owner is another app",
            )))
        }
        Err(e) => return Err(Failure::Unreachable(e)),
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    exchange(sock, NEXT.fetch_add(1, Ordering::Relaxed), bus, dev)
}

/// Sends the request on a connected socket and reads the reply and its fd.
pub fn exchange(mut sock: UnixStream, id: u64, bus: u16, dev: u16) -> Result<OwnedFd, Failure> {
    sock.write_all(proto::request(id, bus, dev).as_bytes())
        .map_err(Failure::Io)?;
    let (line, mut fds) = read_line(sock.as_fd()).map_err(Failure::Io)?;
    proto::reply(&line, id).map_err(Failure::Refused)?;
    // Extra fds, if any, close as they drop.
    if fds.is_empty() {
        return Err(Failure::Refused(Refusal {
            kind: "protocol".into(),
            message: "the reply carried no fd".into(),
        }));
    }
    Ok(fds.remove(0))
}

/// Whether the peer of a connected socket has our real uid. `getuid()` may be faked (proot's `-0`),
/// so ours is read the same way as the peer's: `SO_PEERCRED` of a socketpair we make.
pub fn same_uid(sock: BorrowedFd) -> io::Result<bool> {
    let (a, _b) = UnixStream::pair()?;
    Ok(peer_uid(sock)? == peer_uid(a.as_fd())?)
}

fn peer_uid(sock: BorrowedFd) -> io::Result<libc::uid_t> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid socket and an out-buffer of the size given.
    let r = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Reads up to the first newline (or the end), collecting every fd that came with the bytes.
fn read_line(sock: BorrowedFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut line = Vec::new();
    let mut fds = Vec::new();
    loop {
        let mut buf = [0u8; 4096];
        let n = recv_with_fds(sock, &mut buf, &mut fds)?;
        if n == 0 {
            break;
        }
        line.extend_from_slice(&buf[..n]);
        if let Some(end) = line.iter().position(|b| *b == b'\n') {
            line.truncate(end);
            break;
        }
        if line.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reply too long"));
        }
    }
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the host closed the socket without a reply",
        ));
    }
    Ok((line, fds))
}

/// One `recvmsg`, with room for a few fds, received close-on-exec.
fn recv_with_fds(sock: BorrowedFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    const MAX_FDS: usize = 4;
    // u64 elements keep the control buffer aligned for cmsghdr.
    let mut control = [0u64; 16];
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<libc::c_int>()) as u32) };
    assert!(space as usize <= std::mem::size_of_val(&control));
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        // SAFETY: a valid socket and a msghdr whose buffers are live and sized as declared.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // SAFETY: walking the control messages the kernel just wrote into `msg`.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<libc::c_int>() {
                    // Each received fd is new in our table and owned by nothing else.
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the host sent more fds than asked",
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Seek};

    /// Writes `line` on `sock` with `fd` attached, as the host does.
    fn send_with_fd(sock: &UnixStream, line: &[u8], fd: Option<BorrowedFd>) {
        let mut control = [0u64; 8];
        let mut iov = libc::iovec {
            iov_base: line.as_ptr() as *mut _,
            iov_len: line.len(),
        };
        // SAFETY: test-only sendmsg with buffers that outlive the call.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if let Some(fd) = fd {
                let space = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32);
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = space as _;
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
            }
            assert_eq!(
                libc::sendmsg(sock.as_raw_fd(), &msg, 0),
                line.len() as isize
            );
        }
    }

    /// A host on the other end of a socketpair: reads one request, answers with `reply`, and
    /// attaches a file holding `contents` when `with_fd`.
    fn fake_host(
        reply: &'static str,
        with_fd: bool,
    ) -> (UnixStream, std::thread::JoinHandle<String>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || {
            let mut req = String::new();
            BufReader::new(&theirs).read_line(&mut req).unwrap();
            let mut f = tempfile();
            std::io::Write::write_all(&mut f, b"descriptors").unwrap();
            f.rewind().unwrap();
            send_with_fd(&theirs, reply.as_bytes(), with_fd.then(|| f.as_fd()));
            req
        });
        (ours, t)
    }

    fn tempfile() -> std::fs::File {
        let p = std::env::temp_dir().join(format!(
            "usbshim-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        std::fs::remove_file(&p).unwrap();
        f
    }

    #[test]
    fn receives_the_fd() {
        let (sock, host) = fake_host("{\"id\":4,\"result\":{}}\n", true);
        let fd = exchange(sock, 4, 1, 5).unwrap();
        let req = host.join().unwrap();
        assert!(req.contains("\"method\":\"usb.open\""), "{req}");
        assert!(req.contains("\"bus\":1,\"dev\":5"), "{req}");
        let mut got = String::new();
        std::fs::File::from(fd).read_to_string(&mut got).unwrap();
        assert_eq!(got, "descriptors");
    }

    #[test]
    fn refusal_sets_errno() {
        let (sock, _host) = fake_host(
            "{\"id\":1,\"error\":{\"kind\":\"not-allowed\",\"message\":\"no\"}}\n",
            false,
        );
        let e = exchange(sock, 1, 1, 5).unwrap_err();
        assert_eq!(e.errno(), libc::EACCES, "{e}");
    }

    #[test]
    fn result_without_fd_is_an_error() {
        let (sock, _host) = fake_host("{\"id\":1,\"result\":{}}\n", false);
        assert!(matches!(exchange(sock, 1, 1, 5), Err(Failure::Refused(_))));
    }

    #[test]
    fn closed_socket_is_an_error() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(matches!(exchange(ours, 1, 1, 5), Err(Failure::Io(_))));
    }

    #[test]
    fn same_uid_for_our_own_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(same_uid(a.as_fd()).unwrap());
    }

    #[test]
    fn nobody_listening_is_unreachable() {
        let name = format!("usbshim-test-none-{}", std::process::id());
        let e = open(name.as_bytes(), 1, 5).unwrap_err();
        assert!(matches!(e, Failure::Unreachable(_)), "{e}");
        assert_eq!(e.errno(), libc::ENODEV);
    }

    #[test]
    fn over_an_abstract_socket() {
        use std::os::unix::net::UnixListener;
        let name = format!("usbshim-test-{}", std::process::id());
        let l = UnixListener::bind_addr(&SocketAddr::from_abstract_name(&name).unwrap()).unwrap();
        let host = std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            let mut req = String::new();
            BufReader::new(&c).read_line(&mut req).unwrap();
            let id: serde_json::Value = serde_json::from_str(&req).unwrap();
            let line = format!("{{\"id\":{},\"result\":{{}}}}\n", id["id"]);
            let f = tempfile();
            send_with_fd(&c, line.as_bytes(), Some(f.as_fd()));
        });
        open(name.as_bytes(), 2, 9).unwrap();
        host.join().unwrap();
    }
}
