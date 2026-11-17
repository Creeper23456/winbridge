// SPDX-License-Identifier: GPL-3.0-or-later

//! virglrenderer (Venus) loaded at run time, so the crate builds without it.
//!
//! The library is used the way vtest uses it: `NO_VIRGL | VENUS | RENDER_SERVER | THREAD_SYNC |
//! USE_EXTERNAL_BLOB`, one context per guest Vulkan instance, shared memory made by the render server (blob id 0) and mapped here. The library has
//! no locking, so every call comes from the one session thread, and the fence callback runs
//! inline inside `context_poll` on that thread.

use crate::session::Renderer;
use libloading::Library;
use std::ffi::{c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::sync::Mutex;

const CALLBACKS_VERSION: c_int = 4;
const FLAG_THREAD_SYNC: c_int = 2;
const FLAG_USE_EXTERNAL_BLOB: c_int = 1 << 5;
const FLAG_VENUS: c_int = 1 << 6;
const FLAG_NO_VIRGL: c_int = 1 << 7;
const FLAG_RENDER_SERVER: c_int = 1 << 9;
const BLOB_MEM_HOST3D: u32 = 2;
const FENCE_FLAG_MERGEABLE: u32 = 1;
const CAPSET_VENUS: u32 = 4;
const BLOB_FLAG_MAPPABLE: u32 = 1;

#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    create_gl_context: Option<unsafe extern "C" fn()>,
    destroy_gl_context: Option<unsafe extern "C" fn()>,
    make_current: Option<unsafe extern "C" fn()>,
    get_drm_fd: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    write_context_fence: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<unsafe extern "C" fn(*mut c_void, u32) -> c_int>,
    get_egl_display: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
}

#[repr(C)]
struct CreateBlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const c_void,
    num_iovs: u32,
}

/// Fences that retired since the last `poll`: (ctx, ring_idx, fence_id).
static RETIRED: Mutex<Vec<(u32, u32, u64)>> = Mutex::new(Vec::new());

unsafe extern "C" fn write_context_fence(_cookie: *mut c_void, ctx: u32, ring_idx: u32, fence_id: u64) {
    RETIRED.lock().unwrap().push((ctx, ring_idx, fence_id));
}

type InitFn = unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int;
type CleanupFn = unsafe extern "C" fn(*mut c_void);
type GetCapSetFn = unsafe extern "C" fn(u32, *mut u32, *mut u32);
type FillCapsFn = unsafe extern "C" fn(u32, u32, *mut c_void);
type CtxCreateFn = unsafe extern "C" fn(u32, u32, u32, *const c_char) -> c_int;
type CtxDestroyFn = unsafe extern "C" fn(u32);
type AttachFn = unsafe extern "C" fn(c_int, c_int);
type UnrefFn = unsafe extern "C" fn(u32);
type SubmitFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type FenceFn = unsafe extern "C" fn(u32, u32, u32, u64) -> c_int;
type PollFn = unsafe extern "C" fn(u32);
type PollFdFn = unsafe extern "C" fn(u32) -> c_int;
type CreateBlobFn = unsafe extern "C" fn(*const CreateBlobArgs) -> c_int;
type MapFn = unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int;
type UnmapFn = unsafe extern "C" fn(u32) -> c_int;
type ExportFn = unsafe extern "C" fn(u32, *mut u32, *mut c_int) -> c_int;

pub struct Virgl {
    // Keeps the library mapped for the function pointers below.
    _lib: Library,
    cleanup: CleanupFn,
    get_cap_set: GetCapSetFn,
    fill_caps: FillCapsFn,
    ctx_create: CtxCreateFn,
    ctx_destroy: CtxDestroyFn,
    attach: AttachFn,
    unref: UnrefFn,
    submit: SubmitFn,
    fence: FenceFn,
    poll: PollFn,
    poll_fd: PollFdFn,
    create_blob: CreateBlobFn,
    map: MapFn,
    unmap: UnmapFn,
    /// Only used to log what kind of descriptor a blob exports.
    export: Option<ExportFn>,
    // Must outlive the library: virglrenderer keeps the pointer.
    _callbacks: Box<Callbacks>,
}

impl Virgl {
    /// Loads `libvirglrenderer` from `lib` and starts the renderer. The render server is the
    /// `virgl_render_server` that `RENDER_SERVER_EXEC_PATH` names, or the library's built-in path.
    pub fn open(lib: &str) -> Result<Virgl, String> {
        // The render server inherits this. NVIDIA 550 exports dma-bufs it cannot map and crashes in
        // vkWaitForFences on sync-fd exportable fences; both flags come from venus/patches/virglrenderer.
        if std::env::var_os("VKR_DEBUG").is_none() {
            std::env::set_var("VKR_DEBUG", "opaque,nofenceexport");
        }
        // SAFETY: loading a shared library runs its initialisers; this is the system's renderer.
        let lib = unsafe { Library::new(lib) }.map_err(|e| format!("cannot load {lib}: {e}"))?;
        macro_rules! sym {
            ($name:literal, $ty:ty) => {
                // SAFETY: the symbol has the declared C signature in virglrenderer.h.
                *unsafe { lib.get::<$ty>(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
            };
        }
        let init: InitFn = sym!("virgl_renderer_init", InitFn);
        let mut callbacks = Box::new(Callbacks {
            version: CALLBACKS_VERSION,
            write_fence: None,
            create_gl_context: None,
            destroy_gl_context: None,
            make_current: None,
            get_drm_fd: None,
            write_context_fence: Some(write_context_fence),
            get_server_fd: None,
            get_egl_display: None,
        });
        let flags = FLAG_NO_VIRGL | FLAG_VENUS | FLAG_RENDER_SERVER | FLAG_THREAD_SYNC | FLAG_USE_EXTERNAL_BLOB;
        // SAFETY: `callbacks` stays alive as long as the renderer.
        let r = unsafe { init(std::ptr::null_mut(), flags, &mut *callbacks) };
        if r != 0 {
            return Err(format!("virgl_renderer_init failed: {r}"));
        }
        Ok(Virgl {
            cleanup: sym!("virgl_renderer_cleanup", CleanupFn),
            get_cap_set: sym!("virgl_renderer_get_cap_set", GetCapSetFn),
            fill_caps: sym!("virgl_renderer_fill_caps", FillCapsFn),
            ctx_create: sym!("virgl_renderer_context_create_with_flags", CtxCreateFn),
            ctx_destroy: sym!("virgl_renderer_context_destroy", CtxDestroyFn),
            attach: sym!("virgl_renderer_ctx_attach_resource", AttachFn),
            unref: sym!("virgl_renderer_resource_unref", UnrefFn),
            submit: sym!("virgl_renderer_submit_cmd", SubmitFn),
            fence: sym!("virgl_renderer_context_create_fence", FenceFn),
            poll: sym!("virgl_renderer_context_poll", PollFn),
            poll_fd: sym!("virgl_renderer_context_get_poll_fd", PollFdFn),
            create_blob: sym!("virgl_renderer_resource_create_blob", CreateBlobFn),
            map: sym!("virgl_renderer_resource_map", MapFn),
            unmap: sym!("virgl_renderer_resource_unmap", UnmapFn),
            // SAFETY: the symbol has this signature in virglrenderer.h; it is optional.
            export: unsafe { lib.get::<ExportFn>(b"virgl_renderer_resource_export_blob\0") }.ok().map(|s| *s),
            _lib: lib,
            _callbacks: callbacks,
        })
    }
}

impl Drop for Virgl {
    fn drop(&mut self) {
        // SAFETY: the renderer was initialised in `open` with a null cookie.
        unsafe { (self.cleanup)(std::ptr::null_mut()) };
    }
}

impl Renderer for Virgl {
    fn capset(&mut self) -> Result<Vec<u8>, String> {
        let (mut ver, mut size) = (0u32, 0u32);
        // SAFETY: both out pointers are valid.
        unsafe { (self.get_cap_set)(CAPSET_VENUS, &mut ver, &mut size) };
        if size == 0 {
            return Err("virglrenderer has no venus capset".into());
        }
        let mut caps = vec![0u8; size as usize];
        // SAFETY: `caps` holds the `size` bytes the library announced.
        unsafe { (self.fill_caps)(CAPSET_VENUS, 0, caps.as_mut_ptr().cast()) };
        Ok(caps)
    }

    fn create_context(&mut self, ctx: u32) -> Result<(), i32> {
        let name = b"winbridge";
        // SAFETY: `name` is valid for its length.
        let r = unsafe { (self.ctx_create)(ctx, CAPSET_VENUS, name.len() as u32, name.as_ptr().cast()) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    fn destroy_context(&mut self, ctx: u32) {
        // SAFETY: plain id.
        unsafe { (self.ctx_destroy)(ctx) };
        RETIRED.lock().unwrap().retain(|f| f.0 != ctx);
    }

    fn unref(&mut self, res_id: u32) {
        // SAFETY: plain id.
        unsafe { (self.unref)(res_id) };
    }

    fn submit(&mut self, ctx: u32, cs: &[u8]) -> Result<(), i32> {
        if cs.len() % 4 != 0 {
            return Err(-libc::EINVAL);
        }
        // SAFETY: the library reads `cs.len() / 4` dwords and does not keep the pointer.
        let r = unsafe { (self.submit)(cs.as_ptr() as *mut c_void, ctx as c_int, (cs.len() / 4) as c_int) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    fn create_fence(&mut self, ctx: u32, ring_idx: u32, fence_id: u64) -> Result<(), i32> {
        // SAFETY: plain values.
        let r = unsafe { (self.fence)(ctx, FENCE_FLAG_MERGEABLE, ring_idx, fence_id) };
        if r == 0 { Ok(()) } else { Err(r) }
    }

    fn poll(&mut self, ctx: u32) -> Vec<(u32, u64)> {
        // SAFETY: plain id; the retire callback runs inside this call on this thread.
        unsafe { (self.poll)(ctx) };
        let mut retired = RETIRED.lock().unwrap();
        let mine = retired.iter().filter(|f| f.0 == ctx).map(|f| (f.1, f.2)).collect();
        retired.retain(|f| f.0 != ctx);
        mine
    }

    fn poll_fd(&self, ctx: u32) -> RawFd {
        // SAFETY: plain id.
        unsafe { (self.poll_fd)(ctx) }
    }

    fn create_blob(&mut self, ctx: u32, res_id: u32, blob_id: u64, size: u64) -> Result<(), i32> {
        let args = CreateBlobArgs {
            res_handle: res_id,
            ctx_id: ctx,
            blob_mem: BLOB_MEM_HOST3D,
            blob_flags: BLOB_FLAG_MAPPABLE,
            blob_id,
            size,
            iovecs: std::ptr::null(),
            num_iovs: 0,
        };
        // SAFETY: `args` is valid for the call and names no iovecs.
        let r = unsafe { (self.create_blob)(&args) };
        if r != 0 {
            return Err(r);
        }
        // SAFETY: plain ids.
        unsafe { (self.attach)(ctx as c_int, res_id as c_int) };
        if let Some(export) = self.export {
            let (mut fd_type, mut fd) = (0u32, -1 as c_int);
            // SAFETY: both out pointers are valid; the descriptor, if any, is closed right away.
            let r = unsafe { export(res_id, &mut fd_type, &mut fd) };
            tracing::debug!("blob {res_id} (memory {blob_id}): export -> {r}, fd type {fd_type}");
            if fd >= 0 {
                // SAFETY: the library handed us this descriptor.
                unsafe { libc::close(fd) };
            }
        }
        Ok(())
    }

    fn map_blob(&mut self, res_id: u32) -> Result<(*mut u8, u64), i32> {
        let (mut ptr, mut size) = (std::ptr::null_mut::<c_void>(), 0u64);
        // SAFETY: both out pointers are valid.
        let r = unsafe { (self.map)(res_id, &mut ptr, &mut size) };
        if r != 0 || ptr.is_null() {
            return Err(if r != 0 { r } else { -libc::EFAULT });
        }
        Ok((ptr.cast(), size))
    }

    fn unmap_blob(&mut self, res_id: u32) {
        // SAFETY: plain id.
        unsafe { (self.unmap)(res_id) };
    }
}
