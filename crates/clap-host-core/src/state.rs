//! Plugin state save/load via the `clap.state` extension.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::plugin::clap_plugin;
use clap_sys::stream::{clap_istream, clap_ostream};

use crate::loader;

/// In-memory `clap_ostream`/`clap_istream` pair over a shared byte buffer.
/// The C stream structs live inside the wrapper so the returned pointers
/// stay valid for the lifetime of the `StreamBuf` (don't move it between
/// `as_*stream()` and use).
pub struct StreamBuf {
    inner: Box<Inner>,
    ostream: clap_ostream,
    istream: clap_istream,
}

struct Inner {
    data: Vec<u8>,
    pos: usize,
}

impl StreamBuf {
    pub fn new() -> Self {
        Self::from(Vec::new())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.inner.data
    }

    pub fn as_ostream(&mut self) -> *const clap_ostream {
        &raw const self.ostream
    }

    pub fn as_istream(&mut self) -> *const clap_istream {
        &raw const self.istream
    }
}

impl Default for StreamBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Vec<u8>> for StreamBuf {
    fn from(data: Vec<u8>) -> Self {
        let mut inner = Box::new(Inner { data, pos: 0 });
        let ctx = ptr::from_mut(inner.as_mut()).cast();
        Self {
            inner,
            ostream: clap_ostream {
                ctx,
                write: Some(stream_write),
            },
            istream: clap_istream {
                ctx,
                read: Some(stream_read),
            },
        }
    }
}

unsafe extern "C" fn stream_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let inner = unsafe { &mut *(*stream).ctx.cast::<Inner>() };
    let slice = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), size as usize) };
    inner.data.extend_from_slice(slice);
    i64::try_from(size).unwrap_or(i64::MAX)
}

unsafe extern "C" fn stream_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let inner = unsafe { &mut *(*stream).ctx.cast::<Inner>() };
    let avail = inner.data.len().saturating_sub(inner.pos);
    let n = avail.min(size as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(
            inner.data.as_ptr().add(inner.pos),
            buffer.cast::<u8>(),
            n,
        );
    }
    inner.pos += n;
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Ask the plugin to serialize its state and write the blob to `path`.
pub fn save(plugin: *const clap_plugin, path: &Path) -> Result<(), String> {
    let state = loader::plugin_ext(plugin, CLAP_EXT_STATE).ok_or("plugin has no state extension")?;
    let state = unsafe { &*state.cast::<clap_plugin_state>() };
    let save = state.save.ok_or("state.save is null")?;
    let mut buf = StreamBuf::new();
    if !unsafe { save(plugin, buf.as_ostream()) } {
        return Err("state.save returned false".into());
    }
    std::fs::write(path, buf.bytes()).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Read a state blob from `path` and hand it to the plugin's `state.load`.
pub fn load(plugin: *const clap_plugin, path: &Path) -> Result<(), String> {
    let state = loader::plugin_ext(plugin, CLAP_EXT_STATE).ok_or("plugin has no state extension")?;
    let state = unsafe { &*state.cast::<clap_plugin_state>() };
    let load = state.load.ok_or("state.load is null")?;
    let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut buf = StreamBuf::from(data);
    if unsafe { load(plugin, buf.as_istream()) } {
        Ok(())
    } else {
        Err("plugin rejected state blob".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_buffer_roundtrip() {
        let mut buf = StreamBuf::new();
        let out = buf.as_ostream();
        unsafe {
            ((*out).write.unwrap())(out, b"hello".as_ptr().cast(), 5);
        }
        assert_eq!(buf.bytes(), b"hello");

        let inp = buf.as_istream();
        let mut readback = [0u8; 5];
        let n = unsafe { ((*inp).read.unwrap())(inp, readback.as_mut_ptr().cast(), 5) };
        assert_eq!(n, 5);
        assert_eq!(&readback, b"hello");
    }
}
