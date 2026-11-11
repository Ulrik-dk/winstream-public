use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow, bail};
use cudarc::driver::{CudaContext, sys};
use smithay::backend::renderer::gles::GlesRenderer;

const SLOTS: usize = 3;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;

type RegisterGlImage =
    unsafe extern "C" fn(*mut sys::CUgraphicsResource, u32, u32, u32) -> sys::CUresult;

fn check(r: sys::CUresult, what: &str) -> Result<()> {
    if r != sys::CUresult::CUDA_SUCCESS {
        bail!("{what}: {r:?}");
    }
    Ok(())
}

pub struct DeviceBuffer {
    pub ptr: sys::CUdeviceptr,
    pub pitch: usize,
    cuda: Arc<CudaContext>,
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        let _ = self.cuda.bind_to_thread();
        unsafe { sys::cuMemFree_v2(self.ptr) };
    }
}

pub struct SlotGuard {
    pub buffer: Arc<DeviceBuffer>,
    busy: Arc<AtomicBool>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

struct Slot {
    buffer: Arc<DeviceBuffer>,
    busy: Arc<AtomicBool>,
}

pub struct WindowCopy {
    resource: sys::CUgraphicsResource,
    tex_id: u32,
    size: (i32, i32),
    slots: Vec<Slot>,
}

impl Drop for WindowCopy {
    fn drop(&mut self) {
        unsafe { sys::cuGraphicsUnregisterResource(self.resource) };
    }
}

pub struct CudaCopy {
    cuda: Arc<CudaContext>,
    register_gl: RegisterGlImage,
}

impl CudaCopy {
    pub fn new() -> Result<Self> {
        let cuda = CudaContext::new(0).map_err(|e| anyhow!("CUDA: {e:?}"))?;
        cuda.bind_to_thread()
            .map_err(|e| anyhow!("bind CUDA: {e:?}"))?;
        let register_gl = unsafe {
            let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                bail!("dlopen libcuda.so.1");
            }
            let f = libc::dlsym(lib, c"cuGraphicsGLRegisterImage".as_ptr());
            if f.is_null() {
                bail!("no cuGraphicsGLRegisterImage in libcuda");
            }
            std::mem::transmute::<*mut c_void, RegisterGlImage>(f)
        };
        Ok(CudaCopy { cuda, register_gl })
    }

    pub fn copy(
        &self,
        state: &mut Option<WindowCopy>,
        renderer: &mut GlesRenderer,
        tex_id: u32,
        size: (i32, i32),
    ) -> Result<Option<SlotGuard>> {
        self.cuda
            .bind_to_thread()
            .map_err(|e| anyhow!("bind CUDA: {e:?}"))?;
        if state
            .as_ref()
            .is_none_or(|s| s.tex_id != tex_id || s.size != size)
        {
            *state = None;
            *state = Some(self.register(renderer, tex_id, size)?);
        }
        let wc = state.as_mut().unwrap();
        let Some(slot) = wc.slots.iter().find(|s| !s.busy.load(Ordering::Acquire)) else {
            return Ok(None);
        };
        slot.busy.store(true, Ordering::Release);
        let guard = SlotGuard {
            buffer: slot.buffer.clone(),
            busy: slot.busy.clone(),
        };
        let (resource, buffer) = (&mut wc.resource, guard.buffer.clone());
        let row = size.0 as usize * 4;
        renderer.with_context(|_| -> Result<()> {
            unsafe {
                check(
                    sys::cuGraphicsMapResources(1, resource, std::ptr::null_mut()),
                    "map texture",
                )?;
                let mut array: sys::CUarray = std::ptr::null_mut();
                let copied = check(
                    sys::cuGraphicsSubResourceGetMappedArray(&mut array, *resource, 0, 0),
                    "mapped array",
                )
                .and_then(|_| {
                    let c = sys::CUDA_MEMCPY2D {
                        srcXInBytes: 0,
                        srcY: 0,
                        srcMemoryType: sys::CUmemorytype::CU_MEMORYTYPE_ARRAY,
                        srcHost: std::ptr::null(),
                        srcDevice: 0,
                        srcArray: array,
                        srcPitch: 0,
                        dstXInBytes: 0,
                        dstY: 0,
                        dstMemoryType: sys::CUmemorytype::CU_MEMORYTYPE_DEVICE,
                        dstHost: std::ptr::null_mut(),
                        dstDevice: buffer.ptr,
                        dstArray: std::ptr::null_mut(),
                        dstPitch: buffer.pitch,
                        WidthInBytes: row,
                        Height: size.1 as usize,
                    };
                    check(sys::cuMemcpy2D_v2(&c), "copy texture -> buffer")
                });
                check(
                    sys::cuGraphicsUnmapResources(1, resource, std::ptr::null_mut()),
                    "unmap texture",
                )?;
                copied
            }
        })??;
        Ok(Some(guard))
    }

    fn register(
        &self,
        renderer: &mut GlesRenderer,
        tex_id: u32,
        size: (i32, i32),
    ) -> Result<WindowCopy> {
        let mut resource: sys::CUgraphicsResource = std::ptr::null_mut();
        let register_gl = self.register_gl;
        let r = renderer.with_context(|_| unsafe {
            register_gl(
                &mut resource,
                tex_id,
                GL_TEXTURE_2D,
                CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
            )
        })?;
        check(r, "register GL texture with CUDA")?;
        let mut wc = WindowCopy {
            resource,
            tex_id,
            size,
            slots: Vec::new(),
        };
        for _ in 0..SLOTS {
            let mut ptr: sys::CUdeviceptr = 0;
            let mut pitch = 0usize;
            check(
                unsafe {
                    sys::cuMemAllocPitch_v2(
                        &mut ptr,
                        &mut pitch,
                        size.0 as usize * 4,
                        size.1 as usize,
                        16,
                    )
                },
                "allocate picture buffer",
            )?;
            wc.slots.push(Slot {
                buffer: Arc::new(DeviceBuffer {
                    ptr,
                    pitch,
                    cuda: self.cuda.clone(),
                }),
                busy: Arc::new(AtomicBool::new(false)),
            });
        }
        Ok(wc)
    }
}
