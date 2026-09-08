use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{ensure, Context as _};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{ffi, GlesMapping, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, ExportMem, Renderer};
use smithay::utils::{Physical, Rectangle, Size};
mod pixel_order;
mod transfer;
use pixel_order::PixelOrder;

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
    SmithayRgba(GlesMapping),
    Pooled(PooledReadback),
}

#[derive(Debug)]
pub struct PooledReadback {
    slot: Rc<RefCell<Slot>>,
    len: usize,
    order: PixelOrder,
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
            Self::SmithayRgba(mapping) => {
                copy(&PixelOrder::Rgba.bgra(renderer.map_texture(mapping)?))
            }
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
                    let bytes = std::slice::from_raw_parts(pointer.cast(), pending.len);
                    let result = copy(&pending.order.bgra(bytes));
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
    let result = renderer.with_context(|gl| unsafe {
        transfer::read(gl, &mut allocation, texture, region, capacity)
    })?;
    let order = match result {
        Ok(Some(order)) => order,
        Ok(None) => return Ok(None),
        Err(err) => {
            allocation.capacity = 0;
            return Err(err);
        }
    };
    allocation.capacity = capacity;
    allocation.busy = true;
    drop(allocation);
    Ok(Some(Readback::Pooled(PooledReadback { slot, len, order })))
}

#[cfg(test)]
mod fault_tests;
#[cfg(test)]
mod tests;
