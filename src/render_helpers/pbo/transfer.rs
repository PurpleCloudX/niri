use std::ffi::CStr;
use std::ptr;

use anyhow::{bail, ensure};
use smithay::backend::renderer::gles::{ffi, GlesTexture};
use smithay::utils::{Physical, Rectangle};

use super::{pixel_order::PixelOrder, Slot};

/// Restores caller state on every exit, including GL allocation failures.
struct TransferState<'a> {
    gl: &'a ffi::Gles2,
    framebuffer: u32,
    saved: [i32; 6],
}

const PARAMETERS: [u32; 6] = [
    ffi::READ_FRAMEBUFFER_BINDING,
    ffi::PIXEL_PACK_BUFFER_BINDING,
    ffi::PACK_ALIGNMENT,
    ffi::PACK_ROW_LENGTH,
    ffi::PACK_SKIP_ROWS,
    ffi::PACK_SKIP_PIXELS,
];

impl<'a> TransferState<'a> {
    unsafe fn new(gl: &'a ffi::Gles2) -> anyhow::Result<Self> {
        let mut state = Self {
            gl,
            framebuffer: 0,
            saved: [0; 6],
        };
        for (parameter, value) in PARAMETERS.into_iter().zip(&mut state.saved) {
            gl.GetIntegerv(parameter, value);
        }
        check_error(gl, "querying readback state")?;
        gl.GenFramebuffers(1, &mut state.framebuffer);
        ensure!(
            state.framebuffer != 0,
            "cannot allocate readback framebuffer"
        );
        Ok(state)
    }
}

impl Drop for TransferState<'_> {
    fn drop(&mut self) {
        unsafe {
            self.gl
                .BindFramebuffer(ffi::READ_FRAMEBUFFER, self.saved[0] as u32);
            self.gl
                .BindBuffer(ffi::PIXEL_PACK_BUFFER, self.saved[1] as u32);
            for (parameter, value) in PARAMETERS[2..].iter().zip(&self.saved[2..]) {
                self.gl.PixelStorei(*parameter, *value);
            }
            if self.framebuffer != 0 {
                self.gl.DeleteFramebuffers(1, &self.framebuffer);
            }
        }
    }
}

unsafe fn check_error(gl: &ffi::Gles2, operation: &str) -> anyhow::Result<()> {
    let error = gl.GetError();
    ensure!(
        error == ffi::NO_ERROR,
        "GL error {error:#x} while {operation}"
    );
    Ok(())
}

unsafe fn supports_transfer(gl: &ffi::Gles2) -> bool {
    let version = gl.GetString(ffi::VERSION);
    if version.is_null() {
        return false;
    }
    let version = CStr::from_ptr(version.cast()).to_string_lossy();
    let es3 = version
        .strip_prefix("OpenGL ES ")
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|major| major >= 3);
    es3 && gl.ReadBuffer.is_loaded() && gl.MapBufferRange.is_loaded() && gl.UnmapBuffer.is_loaded()
}

pub(super) unsafe fn read(
    gl: &ffi::Gles2,
    allocation: &mut Slot,
    texture: &GlesTexture,
    region: Rectangle<i32, Physical>,
    capacity: usize,
) -> anyhow::Result<Option<PixelOrder>> {
    // Attribute only errors from this operation; never silently disable on an old error.
    for _ in 0..32 {
        let error = gl.GetError();
        if error == ffi::NO_ERROR {
            break;
        }
        tracing::warn!("pre-existing GL error before readback: {error:#x}");
        if error == ffi::CONTEXT_LOST {
            bail!("GL context lost before readback");
        }
    }
    if !supports_transfer(gl) {
        allocation.disabled = true;
        tracing::warn!("PBO readback unavailable: OpenGL ES 3 and mapping support required");
        return Ok(None);
    }
    let state = TransferState::new(gl)?;
    gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, state.framebuffer);
    gl.FramebufferTexture2D(
        ffi::READ_FRAMEBUFFER,
        ffi::COLOR_ATTACHMENT0,
        ffi::TEXTURE_2D,
        texture.tex_id(),
        0,
    );
    ensure!(
        gl.CheckFramebufferStatus(ffi::READ_FRAMEBUFFER) == ffi::FRAMEBUFFER_COMPLETE,
        "incomplete readback framebuffer"
    );
    let mut format = 0;
    let mut kind = 0;
    gl.GetIntegerv(ffi::IMPLEMENTATION_COLOR_READ_FORMAT, &mut format);
    gl.GetIntegerv(ffi::IMPLEMENTATION_COLOR_READ_TYPE, &mut kind);
    let extensions = gl.GetString(ffi::EXTENSIONS);
    let bgra = !extensions.is_null()
        && CStr::from_ptr(extensions.cast())
            .to_bytes()
            .split(|b| *b == b' ')
            .any(|ext| ext == b"GL_EXT_read_format_bgra");
    let order = if bgra || (format as u32 == ffi::BGRA_EXT && kind as u32 == ffi::UNSIGNED_BYTE) {
        PixelOrder::Bgra
    } else {
        PixelOrder::Rgba
    };
    check_error(gl, "querying readback format")?;
    if allocation.buffer == 0 {
        gl.GenBuffers(1, &mut allocation.buffer);
    }
    ensure!(allocation.buffer != 0, "cannot allocate readback PBO");
    gl.BindBuffer(ffi::PIXEL_PACK_BUFFER, allocation.buffer);
    if allocation.capacity != capacity {
        gl.BufferData(
            ffi::PIXEL_PACK_BUFFER,
            capacity as isize,
            ptr::null(),
            ffi::STREAM_READ,
        );
        check_error(gl, "allocating readback storage")?;
    }
    gl.PixelStorei(ffi::PACK_ALIGNMENT, 4);
    gl.PixelStorei(ffi::PACK_ROW_LENGTH, 0);
    gl.PixelStorei(ffi::PACK_SKIP_ROWS, 0);
    gl.PixelStorei(ffi::PACK_SKIP_PIXELS, 0);
    gl.ReadBuffer(ffi::COLOR_ATTACHMENT0);
    let format = match order {
        PixelOrder::Bgra => ffi::BGRA_EXT,
        PixelOrder::Rgba => ffi::RGBA,
    };
    gl.ReadPixels(
        region.loc.x,
        region.loc.y,
        region.size.w,
        region.size.h,
        format,
        ffi::UNSIGNED_BYTE,
        ptr::null_mut(),
    );
    check_error(gl, "reading framebuffer pixels")?;
    Ok(Some(order))
}
