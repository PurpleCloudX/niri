use std::cell::RefCell;
use std::ptr;
use std::rc::Rc;

use anyhow::{ensure, Context as _};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, ExportMem, Renderer};
use smithay::utils::{Physical, Rectangle, Size};

/// One bounded, context-owned GL buffer; shared-context callers use the fallback.
/// Like the renderer's context resources, GL frees this allocation at context teardown.
#[derive(Debug)]
struct Slot {
    context: ContextId<GlesTexture>,
    egl_context: *const std::ffi::c_void,
    buffer: u32,
    capacity: usize,
    busy: bool,
    disabled: bool,
}

struct MappingGuard<'a> {
    gl: &'a ffi::Gles2,
    previous: u32,
    mapped: bool,
}

impl MappingGuard<'_> {
    fn unmap(&mut self) -> bool {
        self.mapped = false;
        unsafe { self.gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER) == ffi::TRUE }
    }
}

impl Drop for MappingGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if self.mapped {
                self.gl.UnmapBuffer(ffi::PIXEL_PACK_BUFFER);
            }
            self.gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, self.previous);
        }
    }
}

#[derive(Debug)]
pub enum Readback {
    Unchanged,
    Smithay(GlesMapping),
    Pooled(PooledReadback),
}

#[derive(Debug)]
pub struct PooledReadback {
    slot: Rc<RefCell<Slot>>,
    len: usize,
}

impl Drop for PooledReadback {
    fn drop(&mut self) {
        // Future reads use the same command stream, including after cancellation.
        self.slot.borrow_mut().busy = false;
    }
}

impl Readback {
    pub fn with_bytes<T>(
        &self,
        renderer: &mut GlesRenderer,
        copy: impl FnOnce(&[u8]) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        match self {
            Self::Unchanged => copy(&[]),
            Self::Smithay(mapping) => copy(renderer.map_texture(mapping)?),
            Self::Pooled(pending) => {
                let slot = pending.slot.borrow();
                ensure!(
                    slot.context == renderer.context_id(),
                    "PBO renderer changed"
                );
                ensure!(
                    slot.egl_context == renderer.egl_context().get_context_handle(),
                    "PBO EGL context changed"
                );
                renderer.with_context(|gl| unsafe {
                    let mut previous = 0;
                    gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut previous);
                    gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, slot.buffer);
                    let mut guard = MappingGuard {
                        gl,
                        previous: previous as u32,
                        mapped: false,
                    };
                    let pointer = gl.MapBufferRange(
                        ffi::PIXEL_PACK_BUFFER,
                        0,
                        pending.len as isize,
                        ffi::MAP_READ_BIT,
                    );
                    if pointer.is_null() {
                        anyhow::bail!("error mapping pooled PBO");
                    }
                    guard.mapped = true;
                    let result = copy(std::slice::from_raw_parts(pointer.cast(), pending.len));
                    ensure!(guard.unmap(), "pooled PBO contents became invalid");
                    result
                })?
            }
        }
    }
}

pub fn try_readback(
    renderer: &mut GlesRenderer,
    texture: &GlesTexture,
    size: Size<i32, Physical>,
    format: Fourcc,
) -> anyhow::Result<Option<Readback>> {
    try_readback_region(renderer, texture, size, format, Rectangle::from_size(size))
}

pub fn try_readback_region(
    renderer: &mut GlesRenderer,
    texture: &GlesTexture,
    size: Size<i32, Physical>,
    format: Fourcc,
    region: Rectangle<i32, Physical>,
) -> anyhow::Result<Option<Readback>> {
    ensure!(size.w > 0 && size.h > 0, "empty PBO readback");
    ensure!(
        region.size.w > 0 && region.size.h > 0 && Rectangle::from_size(size).contains_rect(region),
        "invalid readback region"
    );
    if !matches!(format, Fourcc::Argb8888 | Fourcc::Xrgb8888) {
        return Ok(None);
    }
    let len = (region.size.w as usize)
        .checked_mul(region.size.h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("PBO size overflow")?;
    isize::try_from(len).context("PBO exceeds addressable size")?;
    // Keep one frame of storage, so changing damage sizes never reallocates it.
    let capacity = (size.w as usize)
        .checked_mul(size.h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("PBO capacity overflow")?;
    isize::try_from(capacity).context("PBO capacity exceeds addressable size")?;
    let context = renderer.context_id();
    let egl_context = renderer.egl_context().get_context_handle();
    let data = renderer.egl_context().user_data();
    data.insert_if_missing(|| {
        Rc::new(RefCell::new(Slot {
            context: context.clone(),
            egl_context,
            buffer: 0,
            capacity: 0,
            busy: false,
            disabled: false,
        }))
    });
    let Some(slot) = data.get::<Rc<RefCell<Slot>>>().cloned() else {
        return Ok(None);
    };
    let mut allocation = slot.borrow_mut();
    if allocation.busy
        || allocation.disabled
        || allocation.context != context
        || allocation.egl_context != egl_context
    {
        return Ok(None);
    }
    let success = renderer.with_context(|gl| unsafe {
        if !gl.ReadBuffer.is_loaded() || !gl.MapBufferRange.is_loaded() {
            allocation.disabled = true;
            return false;
        }
        let mut framebuffer = 0;
        let mut old_read_fbo = 0;
        let mut old_pack = 0;
        let mut old_alignment = 0;
        let mut old_row_length = 0;
        let mut old_skip_rows = 0;
        let mut old_skip_pixels = 0;
        gl.GetIntegerv(ffi::READ_FRAMEBUFFER_BINDING, &mut old_read_fbo);
        gl.GetIntegerv(ffi::PIXEL_PACK_BUFFER_BINDING, &mut old_pack);
        gl.GetIntegerv(ffi::PACK_ALIGNMENT, &mut old_alignment);
        gl.GetIntegerv(ffi::PACK_ROW_LENGTH, &mut old_row_length);
        gl.GetIntegerv(ffi::PACK_SKIP_ROWS, &mut old_skip_rows);
        gl.GetIntegerv(ffi::PACK_SKIP_PIXELS, &mut old_skip_pixels);
        gl.GenFramebuffers(1, &mut framebuffer);
        if framebuffer == 0 {
            return false;
        }
        gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, framebuffer);
        gl.FramebufferTexture2D(
            ffi::READ_FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            texture.tex_id(),
            0,
        );
        let complete =
            gl.CheckFramebufferStatus(ffi::READ_FRAMEBUFFER) == ffi::FRAMEBUFFER_COMPLETE;
        if complete {
            if allocation.buffer == 0 {
                gl.GenBuffers(1, &mut allocation.buffer);
            }
            if allocation.buffer == 0 {
                gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, old_read_fbo as u32);
                gl.DeleteFramebuffers(1, &framebuffer);
                return false;
            }
            gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, allocation.buffer);
            if allocation.capacity != capacity {
                gl.BufferData(
                    ffi::PIXEL_PACK_BUFFER,
                    capacity as isize,
                    ptr::null(),
                    ffi::STREAM_READ,
                );
            }
            gl.PixelStorei(ffi::PACK_ALIGNMENT, 4);
            gl.PixelStorei(ffi::PACK_ROW_LENGTH, 0);
            gl.PixelStorei(ffi::PACK_SKIP_ROWS, 0);
            gl.PixelStorei(ffi::PACK_SKIP_PIXELS, 0);
            gl.ReadBuffer(ffi::COLOR_ATTACHMENT0);
            gl.ReadPixels(
                region.loc.x,
                region.loc.y,
                region.size.w,
                region.size.h,
                ffi::BGRA_EXT,
                ffi::UNSIGNED_BYTE,
                ptr::null_mut(),
            );
        }
        let error = gl.GetError();
        if matches!(error, ffi::INVALID_ENUM | ffi::INVALID_OPERATION) {
            allocation.disabled = true;
        }
        gl.PixelStorei(ffi::PACK_ALIGNMENT, old_alignment);
        gl.PixelStorei(ffi::PACK_ROW_LENGTH, old_row_length);
        gl.PixelStorei(ffi::PACK_SKIP_ROWS, old_skip_rows);
        gl.PixelStorei(ffi::PACK_SKIP_PIXELS, old_skip_pixels);
        gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, old_pack as u32);
        gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, old_read_fbo as u32);
        gl.DeleteFramebuffers(1, &framebuffer);
        complete && error == ffi::NO_ERROR
    })?;
    if !success {
        allocation.capacity = 0;
        return Ok(None);
    }
    allocation.capacity = capacity;
    allocation.busy = true;
    drop(allocation);
    Ok(Some(Readback::Pooled(PooledReadback { slot, len })))
}

#[cfg(test)]
mod tests;
