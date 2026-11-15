// SPDX-License-Identifier: GPL-3.0-or-later

//! Vulkan call serialization for the WinBridge GPU channel.
//!
//! `generated` is produced by `winbridge-gfx-vkgen` from `vk.xml`. This file holds the
//! hand-written contracts the generated code is written against.

#[allow(clippy::all, non_snake_case)]
pub mod dxvk;
mod generated;

pub use generated::*;
pub use winbridge_gfx_proto::codec::CodecError;

/// Terminator of a serialized `pNext` chain.
pub const CHAIN_END: u32 = u32::MAX;
/// Largest element capacity of an enumeration call.
pub const MAX_ENUM: usize = 1 << 16;

/// Maps wire handle ids to driver handles.
pub trait Resolver {
    /// Id 0 is `VK_NULL_HANDLE` and maps to 0. An unknown id, or an id of another type, is an error.
    fn resolve(&self, id: u64, kind: &'static str) -> Result<u64, CodecError>;
}

#[derive(Debug)]
pub struct LinkError(pub String);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum GuestError {
    Codec(CodecError),
    Link(LinkError),
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codec(e) => write!(f, "codec: {e}"),
            Self::Link(e) => write!(f, "link: {e}"),
        }
    }
}

impl From<CodecError> for GuestError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

impl From<LinkError> for GuestError {
    fn from(e: LinkError) -> Self {
        Self::Link(e)
    }
}

/// Guest side of the channel, implemented by the ICD.
pub trait Link {
    /// Sends one serialized call and waits for its reply.
    fn call(&mut self, req: &[u8]) -> Result<Vec<u8>, LinkError>;
    /// Queues a call that needs no reply; it is sent with the next batch.
    fn send_async(&mut self, req: &[u8]) -> Result<(), LinkError>;
    /// Sends the queued batch now.
    fn flush(&mut self) -> Result<(), LinkError>;
    /// Allocates a handle: `(wire id, raw value)`. A non-dispatchable handle's raw value is its
    /// id; a dispatchable one is a pointer to an object that starts with `DispatchableHeader`.
    fn new_handle(&mut self, dispatchable: bool, kind: &'static str) -> (u64, u64);
    /// Releases a handle from `new_handle` that the host did not create.
    fn discard_handle(&mut self, raw: u64, dispatchable: bool);
    /// Called when a command fails in the transport or codec; the command then reports device loss.
    fn failed(&mut self, command: &str, err: &GuestError);
}

/// Wire id of a dispatchable handle created by the ICD (0 for null).
///
/// # Safety
/// `raw` must be 0 or a pointer to an object starting with `DispatchableHeader`.
pub unsafe fn hid_disp(raw: u64) -> u64 {
    if raw == 0 {
        0
    } else {
        (*(raw as *const winbridge_gfx_proto::DispatchableHeader)).id
    }
}

/// Host side of the channel, implemented by the renderer session.
#[cfg(feature = "host")]
pub trait Host: Resolver + Hooks {
    fn entry(&self) -> &ash::Entry;
    fn instance(&self, id: u64) -> Result<std::sync::Arc<ash::Instance>, CodecError>;
    fn instance_of_physical(&self, id: u64) -> Result<std::sync::Arc<ash::Instance>, CodecError>;
    /// `id` names a device, or an object owned by one (queue, command buffer).
    fn device(&self, id: u64) -> Result<std::sync::Arc<ash::Device>, CodecError>;
    /// The driver's address of a device-level command by name (for commands the pinned `ash`
    /// has no function table for). `id` names a device or an object owned by one.
    fn device_proc(&self, id: u64, name: &std::ffi::CStr) -> Result<ash::vk::PFN_vkVoidFunction, CodecError>;
    /// Records that guest id `id` is the driver handle `raw`, created below `parent`.
    fn register(&mut self, id: u64, raw: u64, kind: &'static str, parent: u64) -> Result<(), CodecError>;
    /// Forgets `id` after its destroy command ran.
    fn unregister(&mut self, id: u64);
}

