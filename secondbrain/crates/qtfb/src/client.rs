//! Linux transport for the qtfb protocol: SEQPACKET unix socket + mmap'd shared memory.
//! (macOS has no `SOCK_SEQPACKET`, and this only ever runs on the device.)

use crate::proto::{self, RefreshMode, ServerMsg};
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

pub struct Connection {
    fd: RawFd,
    shm: *mut u8,
    shm_len: usize,
    pub width: u32,
    pub height: u32,
    pub format: u8,
}

// The raw shm pointer is only touched through &mut self.
unsafe impl Send for Connection {}

impl Connection {
    /// Connect and initialize. `custom_size` picks `MSG_CUSTOM_INITIALIZE`; otherwise the
    /// server allocates the device-native size for `format` (954×1696 for the Move formats).
    pub fn connect(key: u32, format: u8, custom_size: Option<(u16, u16)>) -> io::Result<Connection> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // From here on, close fd on any early return.
        let guard = FdGuard(fd);

        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path = proto::SOCKET_PATH.as_bytes();
        if path.len() >= addr.sun_path.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(path) {
            *dst = *src as libc::c_char;
        }
        let rc = unsafe {
            libc::connect(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let init = match custom_size {
            Some((w, h)) => proto::encode_custom_initialize(key, format, w, h),
            None => proto::encode_initialize(key, format),
        };
        send_packet(fd, &init)?;

        let mut buf = [0u8; proto::SERVER_MSG_LEN];
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 1 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no init ack from qtfb"));
        }
        let (shm_key, shm_size) = match proto::decode_server(&buf[..n as usize]) {
            Some(ServerMsg::InitAck { shm_key, shm_size }) => (shm_key, shm_size),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected init reply: {other:?}"),
                ))
            }
        };

        let (width, height) = match custom_size {
            Some((w, h)) => (w as u32, h as u32),
            None => proto::dims_for_format(format).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("format {format} has no default size"))
            })?,
        };
        let needed = width as usize * height as usize * proto::bytes_per_pixel(format);
        if shm_size < needed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("shm too small: {shm_size} < {needed}"),
            ));
        }

        let shm_path = format!("{}{}", proto::SHM_PATH_PREFIX, shm_key);
        let file = OpenOptions::new().read(true).write(true).open(&shm_path)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                shm_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        std::mem::forget(guard); // Connection owns fd now
        log::info!("qtfb connected: key={key} fmt={format} {width}x{height} shm={shm_path} ({shm_size} bytes)");
        Ok(Connection { fd, shm: ptr as *mut u8, shm_len: shm_size, width, height, format })
    }

    /// The pixel buffer (row-major, `bytes_per_pixel(format)` per pixel, no row padding).
    ///
    /// Sliced to exactly `width*height*bpp`, NOT the raw shm size: `connect` only guarantees
    /// `shm_size >= needed`, and callers `copy_from_slice` whole frames into this — a padded shm
    /// allocation would make that panic on the length mismatch.
    pub fn frame_mut(&mut self) -> &mut [u8] {
        let needed =
            self.width as usize * self.height as usize * proto::bytes_per_pixel(self.format);
        unsafe { std::slice::from_raw_parts_mut(self.shm, needed.min(self.shm_len)) }
    }

    pub fn update_all(&self) -> io::Result<()> {
        send_packet(self.fd, &proto::encode_update_all())
    }

    pub fn update_partial(&self, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
        send_packet(self.fd, &proto::encode_update_partial(x, y, w, h))
    }

    pub fn set_refresh_mode(&self, mode: RefreshMode) -> io::Result<()> {
        send_packet(self.fd, &proto::encode_set_refresh_mode(mode))
    }

    pub fn request_full_refresh(&self) -> io::Result<()> {
        send_packet(self.fd, &proto::encode_request_full_refresh())
    }

    /// Wait up to `timeout_ms` for a server message (negative = block indefinitely).
    /// `Ok(None)` on timeout; `Err(UnexpectedEof)` when the compositor closed the socket.
    pub fn poll_msg(&self, timeout_ms: i32) -> io::Result<Option<ServerMsg>> {
        let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(e);
        }
        if rc == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; proto::SERVER_MSG_LEN];
        let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 1 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "qtfb socket closed"));
        }
        Ok(proto::decode_server(&buf[..n as usize]))
    }

    /// The underlying socket fd — for callers that want to `poll()` it alongside other fds (e.g. an
    /// evdev input device). Reads/writes still go through the methods above.
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = send_packet(self.fd, &proto::encode_terminate());
        unsafe {
            libc::munmap(self.shm as *mut libc::c_void, self.shm_len);
            libc::close(self.fd);
        }
    }
}

struct FdGuard(RawFd);
impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn send_packet(fd: RawFd, msg: &[u8; proto::CLIENT_MSG_LEN]) -> io::Result<()> {
    let n = unsafe {
        libc::send(fd, msg.as_ptr() as *const libc::c_void, msg.len(), libc::MSG_NOSIGNAL)
    };
    if n != msg.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
