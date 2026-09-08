use std::os::fd::BorrowedFd;
use std::ptr;

use anyhow::{ensure, Context as _};
use smithay::reexports::rustix;
use smithay::utils::{Physical, Rectangle};

/// Owns a mapping of a sealed, fixed-size memfd. No references into it escape.
#[derive(Debug)]
pub(super) struct ShmMapping {
    address: *mut std::ffi::c_void,
    len: usize,
}

impl ShmMapping {
    pub(super) fn new(fd: BorrowedFd<'_>, len: usize) -> anyhow::Result<Self> {
        ensure!(
            len > 0 && len <= isize::MAX as usize,
            "invalid mapping length"
        );
        // The caller seals the file against shrinking before creating this mapping.
        let address = unsafe {
            rustix::mm::mmap(
                ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                fd,
                0,
            )
        }
        .context("error mapping SHM buffer")?;
        Ok(Self { address, len })
    }

    /// Only call while the producer owns the dequeued PipeWire buffer.
    pub(super) fn copy_frame(&self, bytes: &[u8]) -> anyhow::Result<()> {
        ensure!(bytes.len() == self.len, "invalid SHM frame length");
        // The source cannot alias this private mapping; no Rust references to it exist.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.address.cast(), self.len) };
        Ok(())
    }

    pub(super) fn clear(&self) {
        unsafe { ptr::write_bytes(self.address.cast::<u8>(), 0, self.len) };
    }

    /// Copy compact readback rows while preserving the destination's unchanged pixels.
    pub(super) fn copy_region(
        &self,
        bytes: &[u8],
        stride: usize,
        region: Rectangle<i32, Physical>,
    ) -> anyhow::Result<()> {
        ensure!(
            region.loc.x >= 0 && region.loc.y >= 0 && region.size.w > 0 && region.size.h > 0,
            "invalid SHM copy region"
        );
        let row_bytes = (region.size.w as usize)
            .checked_mul(4)
            .context("row size overflow")?;
        let x = (region.loc.x as usize)
            .checked_mul(4)
            .context("column overflow")?;
        let y = region.loc.y as usize;
        let rows = region.size.h as usize;
        ensure!(
            stride > 0 && x.checked_add(row_bytes).is_some_and(|end| end <= stride),
            "SHM copy exceeds stride"
        );
        ensure!(
            y.checked_add(rows)
                .and_then(|end| end.checked_mul(stride))
                .is_some_and(|end| end <= self.len),
            "SHM copy exceeds mapping"
        );
        ensure!(
            row_bytes.checked_mul(rows) == Some(bytes.len()),
            "invalid region byte length"
        );
        if x == 0 && y == 0 && row_bytes == stride && bytes.len() == self.len {
            return self.copy_frame(bytes);
        }
        for row in 0..rows {
            // Bounds checked above; bytes never alias this privately owned mapping.
            unsafe {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(row * row_bytes),
                    self.address.cast::<u8>().add((y + row) * stride + x),
                    row_bytes,
                );
            }
        }
        Ok(())
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        // Drop must not panic, including while unwinding a rendering error.
        if let Err(err) = unsafe { rustix::mm::munmap(self.address, self.len) } {
            tracing::warn!("error unmapping SHM buffer: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::{fd::AsFd, unix::fs::FileExt};

    #[test]
    fn region_copy_preserves_other_rows_and_rejects_out_of_bounds() {
        let fd =
            rustix::fs::memfd_create("copy-region-test", rustix::fs::MemfdFlags::ALLOW_SEALING)
                .unwrap();
        rustix::fs::ftruncate(&fd, 48).unwrap();
        rustix::fs::fcntl_add_seals(
            &fd,
            rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
        )
        .unwrap();
        let mapping = ShmMapping::new(fd.as_fd(), 48).unwrap();
        mapping.copy_frame(&[7; 48]).unwrap();
        mapping
            .copy_region(&[9; 8], 16, Rectangle::new((1, 1).into(), (2, 1).into()))
            .unwrap();
        let mut bytes = [0; 48];
        std::fs::File::from(fd)
            .read_exact_at(&mut bytes, 0)
            .unwrap();
        assert_eq!(&bytes[..20], &[7; 20]);
        assert_eq!(&bytes[20..28], &[9; 8]);
        assert_eq!(&bytes[28..], &[7; 20]);
        for rect in [
            Rectangle::new((-1, 0).into(), (2, 1).into()),
            Rectangle::new((3, 0).into(), (2, 1).into()),
            Rectangle::new((0, 3).into(), (2, 1).into()),
        ] {
            assert!(mapping.copy_region(&[9; 8], 16, rect).is_err());
        }
    }
}
