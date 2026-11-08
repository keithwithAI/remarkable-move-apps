//! Clean-room client for the qtfb compositor protocol (XOVI AppLoad's framebuffer server on
//! reMarkable Paper Pro / Move devices).
//!
//! [`proto`] (message encode/decode) is pure and tested on any host; [`client`] (socket +
//! shared memory) is Linux-only. External apps get their framebuffer key via the `QTFB_KEY`
//! env var, draw into the mapped buffer, send update messages, and receive touch/pen input
//! back over the same socket — already scaled to framebuffer pixels.

pub mod proto;

#[cfg(target_os = "linux")]
pub mod client;

pub use proto::{
    bytes_per_pixel, decode_server, dims_for_format, key_from_env, InputEvent, InputKind,
    RefreshMode, ServerMsg, DEFAULT_SCENE_KEY, FBFMT_RMPPM_RGB888,
};

#[cfg(target_os = "linux")]
pub use client::Connection;
