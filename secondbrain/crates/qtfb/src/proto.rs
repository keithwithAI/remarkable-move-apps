//! qtfb wire protocol: message encode/decode with explicit byte offsets.
//!
//! Clean-room reimplementation of the protocol used by AppLoad's qtfb compositor
//! (facts taken from `src/qtfb/common.h` in asivery/rm-appload; no GPL code copied):
//!
//! - Transport: unix `SOCK_SEQPACKET` socket at [`SOCKET_PATH`]; each message is one packet.
//! - Client → server, 24 bytes: `u8 type` at offset 0, C-union payload at offset 4
//!   (the union's largest member is the five-`int` update-region message).
//! - Server → client, 32 bytes: `u8 type` at offset 0, C-union payload at offset 8
//!   (aligned for the `size_t` inside the init response).
//! - After an initialize, the server replies with a shared-memory key + size; the client maps
//!   `/dev/shm/qtfb_<key>` and draws into it, then sends update messages.
//! - User input (touch/pen/buttons/virtual keyboard) arrives as server messages with
//!   coordinates already scaled to the framebuffer's pixel space.
//!
//! Everything here is pure byte manipulation (little-endian, matching the device's aarch64
//! C ABI) so it compiles and unit-tests on any host; the socket transport lives in
//! [`crate::client`] (Linux only).

pub const SOCKET_PATH: &str = "/tmp/qtfb.sock";
pub const SHM_PATH_PREFIX: &str = "/dev/shm/qtfb_";
/// AppLoad passes the allocated framebuffer key to external apps in this env var.
pub const KEY_ENV_VAR: &str = "QTFB_KEY";
/// The compositor's built-in test scene (connect without AppLoad, e.g. for the spike).
pub const DEFAULT_SCENE_KEY: u32 = 245209899;

pub const MSG_INITIALIZE: u8 = 0;
pub const MSG_UPDATE: u8 = 1;
pub const MSG_CUSTOM_INITIALIZE: u8 = 2;
pub const MSG_TERMINATE: u8 = 3;
pub const MSG_USERINPUT: u8 = 4;
pub const MSG_SET_REFRESH_MODE: u8 = 5;
pub const MSG_REQUEST_FULL_REFRESH: u8 = 6;

pub const UPDATE_ALL: i32 = 0;
pub const UPDATE_PARTIAL: i32 = 1;

pub const FBFMT_RM2FB: u8 = 0;
pub const FBFMT_RMPP_RGB888: u8 = 1;
pub const FBFMT_RMPP_RGBA8888: u8 = 2;
pub const FBFMT_RMPP_RGB565: u8 = 3;
pub const FBFMT_RMPPM_RGB888: u8 = 4;
pub const FBFMT_RMPPM_RGBA8888: u8 = 5;
pub const FBFMT_RMPPM_RGB565: u8 = 6;

pub const CLIENT_MSG_LEN: usize = 24;
pub const SERVER_MSG_LEN: usize = 32;

/// E-ink waveform selection (`MSG_SET_REFRESH_MODE` payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    UltraFast = 0,
    Fast = 1,
    Animate = 2,
    Content = 3,
    /// The compositor default.
    Ui = 4,
}

/// Native (width, height) for a device-default format, per the compositor's device table.
pub fn dims_for_format(format: u8) -> Option<(u32, u32)> {
    match format {
        FBFMT_RM2FB => Some((1404, 1872)),
        FBFMT_RMPP_RGB888 | FBFMT_RMPP_RGBA8888 | FBFMT_RMPP_RGB565 => Some((1620, 2160)),
        FBFMT_RMPPM_RGB888 | FBFMT_RMPPM_RGBA8888 | FBFMT_RMPPM_RGB565 => Some((954, 1696)),
        _ => None,
    }
}

pub fn bytes_per_pixel(format: u8) -> usize {
    match format {
        FBFMT_RMPP_RGBA8888 | FBFMT_RMPPM_RGBA8888 => 4,
        FBFMT_RMPP_RGB888 | FBFMT_RMPPM_RGB888 => 3,
        _ => 2, // rm2fb + the rgb565 modes
    }
}

fn put_i32(buf: &mut [u8], at: usize, v: i32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_i32(buf: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

pub fn encode_initialize(key: u32, format: u8) -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_INITIALIZE;
    put_i32(&mut b, 4, key as i32);
    b[8] = format;
    b
}

pub fn encode_custom_initialize(key: u32, format: u8, w: u16, h: u16) -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_CUSTOM_INITIALIZE;
    put_i32(&mut b, 4, key as i32);
    b[8] = format;
    // u16s align to offset 10/12 after the format byte in the C struct.
    b[10..12].copy_from_slice(&w.to_le_bytes());
    b[12..14].copy_from_slice(&h.to_le_bytes());
    b
}

pub fn encode_update_all() -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_UPDATE;
    put_i32(&mut b, 4, UPDATE_ALL);
    b
}

pub fn encode_update_partial(x: i32, y: i32, w: i32, h: i32) -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_UPDATE;
    put_i32(&mut b, 4, UPDATE_PARTIAL);
    put_i32(&mut b, 8, x);
    put_i32(&mut b, 12, y);
    put_i32(&mut b, 16, w);
    put_i32(&mut b, 20, h);
    b
}

pub fn encode_terminate() -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_TERMINATE;
    b
}

pub fn encode_set_refresh_mode(mode: RefreshMode) -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_SET_REFRESH_MODE;
    put_i32(&mut b, 4, mode as i32);
    b
}

pub fn encode_request_full_refresh() -> [u8; CLIENT_MSG_LEN] {
    let mut b = [0u8; CLIENT_MSG_LEN];
    b[0] = MSG_REQUEST_FULL_REFRESH;
    b
}

/// Input event kinds (`inputType` values in user-input messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    TouchPress,
    TouchRelease,
    TouchUpdate,
    PenPress,
    PenRelease,
    PenUpdate,
    ButtonPress,
    ButtonRelease,
    KeyPress,
    KeyRelease,
    Other(i32),
}

impl InputKind {
    fn from_raw(v: i32) -> InputKind {
        match v {
            0x10 => InputKind::TouchPress,
            0x11 => InputKind::TouchRelease,
            0x12 => InputKind::TouchUpdate,
            0x20 => InputKind::PenPress,
            0x21 => InputKind::PenRelease,
            0x22 => InputKind::PenUpdate,
            0x30 => InputKind::ButtonPress,
            0x31 => InputKind::ButtonRelease,
            0x40 => InputKind::KeyPress,
            0x41 => InputKind::KeyRelease,
            other => InputKind::Other(other),
        }
    }

    pub fn is_press(&self) -> bool {
        matches!(self, InputKind::TouchPress | InputKind::PenPress)
    }
    pub fn is_release(&self) -> bool {
        matches!(self, InputKind::TouchRelease | InputKind::PenRelease)
    }
    pub fn is_update(&self) -> bool {
        matches!(self, InputKind::TouchUpdate | InputKind::PenUpdate)
    }
}

/// One user-input event. `x`/`y` are already in framebuffer pixels; `dev_id` distinguishes
/// multitouch points; `d` carries pen pressure (0–100) or is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub dev_id: i32,
    pub x: i32,
    pub y: i32,
    pub d: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMsg {
    /// Reply to an initialize: map `/dev/shm/qtfb_<shm_key>` of `shm_size` bytes.
    InitAck { shm_key: i32, shm_size: usize },
    Input(InputEvent),
    Unknown(u8),
}

/// Decode one server packet (`recv` gives whole packets on SEQPACKET sockets).
pub fn decode_server(buf: &[u8]) -> Option<ServerMsg> {
    let kind = *buf.first()?;
    match kind {
        MSG_INITIALIZE if buf.len() >= 24 => Some(ServerMsg::InitAck {
            shm_key: get_i32(buf, 8),
            shm_size: u64::from_le_bytes(buf[16..24].try_into().unwrap()) as usize,
        }),
        MSG_USERINPUT if buf.len() >= 28 => Some(ServerMsg::Input(InputEvent {
            kind: InputKind::from_raw(get_i32(buf, 8)),
            dev_id: get_i32(buf, 12),
            x: get_i32(buf, 16),
            y: get_i32(buf, 20),
            d: get_i32(buf, 24),
        })),
        other => Some(ServerMsg::Unknown(other)),
    }
}

/// Read the framebuffer key AppLoad passes to external apps.
pub fn key_from_env() -> Option<u32> {
    std::env::var(KEY_ENV_VAR).ok()?.trim().parse::<i64>().ok().map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_are_24_bytes_with_documented_offsets() {
        let b = encode_initialize(245209899, FBFMT_RMPPM_RGB888);
        assert_eq!(b.len(), CLIENT_MSG_LEN);
        assert_eq!(b[0], MSG_INITIALIZE);
        assert_eq!(get_i32(&b, 4), 245209899);
        assert_eq!(b[8], FBFMT_RMPPM_RGB888);

        let b = encode_custom_initialize(7, FBFMT_RMPPM_RGB888, 954, 1696);
        assert_eq!(b[0], MSG_CUSTOM_INITIALIZE);
        assert_eq!(u16::from_le_bytes(b[10..12].try_into().unwrap()), 954);
        assert_eq!(u16::from_le_bytes(b[12..14].try_into().unwrap()), 1696);

        let b = encode_update_partial(10, 20, 300, 400);
        assert_eq!(b[0], MSG_UPDATE);
        assert_eq!(get_i32(&b, 4), UPDATE_PARTIAL);
        assert_eq!((get_i32(&b, 8), get_i32(&b, 12), get_i32(&b, 16), get_i32(&b, 20)), (10, 20, 300, 400));

        assert_eq!(encode_update_all()[0], MSG_UPDATE);
        assert_eq!(encode_terminate()[0], MSG_TERMINATE);
        assert_eq!(get_i32(&encode_set_refresh_mode(RefreshMode::Fast), 4), 1);
        assert_eq!(encode_request_full_refresh()[0], MSG_REQUEST_FULL_REFRESH);
    }

    #[test]
    fn decodes_init_ack_at_size_t_aligned_offsets() {
        let mut b = [0u8; SERVER_MSG_LEN];
        b[0] = MSG_INITIALIZE;
        b[8..12].copy_from_slice(&1234i32.to_le_bytes());
        b[16..24].copy_from_slice(&4_853_952u64.to_le_bytes()); // 954*1696*3
        assert_eq!(
            decode_server(&b),
            Some(ServerMsg::InitAck { shm_key: 1234, shm_size: 4_853_952 })
        );
    }

    #[test]
    fn decodes_touch_input() {
        let mut b = [0u8; SERVER_MSG_LEN];
        b[0] = MSG_USERINPUT;
        b[8..12].copy_from_slice(&0x10i32.to_le_bytes());
        b[12..16].copy_from_slice(&2i32.to_le_bytes());
        b[16..20].copy_from_slice(&477i32.to_le_bytes());
        b[20..24].copy_from_slice(&848i32.to_le_bytes());
        let Some(ServerMsg::Input(ev)) = decode_server(&b) else { panic!() };
        assert_eq!(ev.kind, InputKind::TouchPress);
        assert!(ev.kind.is_press());
        assert_eq!((ev.dev_id, ev.x, ev.y, ev.d), (2, 477, 848, 0));
    }

    #[test]
    fn move_format_geometry() {
        assert_eq!(dims_for_format(FBFMT_RMPPM_RGB888), Some((954, 1696)));
        assert_eq!(bytes_per_pixel(FBFMT_RMPPM_RGB888), 3);
        let (w, h) = dims_for_format(FBFMT_RMPPM_RGB888).unwrap();
        assert_eq!(w as usize * h as usize * bytes_per_pixel(FBFMT_RMPPM_RGB888), 4_853_952);
    }

    #[test]
    fn short_or_unknown_packets_do_not_panic() {
        assert_eq!(decode_server(&[]), None);
        assert_eq!(decode_server(&[MSG_TERMINATE]), Some(ServerMsg::Unknown(MSG_TERMINATE)));
        assert_eq!(decode_server(&[MSG_USERINPUT, 0, 0]), Some(ServerMsg::Unknown(MSG_USERINPUT)));
    }
}
