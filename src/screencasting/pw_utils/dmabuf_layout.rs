use std::os::fd::BorrowedFd;

use anyhow::{ensure, Context as _};
use smithay::reexports::rustix::fs::{seek, SeekFrom};

/// Query the backing object's size, including padding and auxiliary planes.
/// A tiled buffer's allocation size cannot be inferred from width and height.
pub(super) fn backing_size(fd: BorrowedFd<'_>, offset: u32) -> anyhow::Result<u32> {
    let size = seek(fd, SeekFrom::End(0)).context("error querying DMA-BUF size")?;
    seek(fd, SeekFrom::Start(0)).context("error resetting DMA-BUF position")?;
    let size = u32::try_from(size).context("DMA-BUF exceeds SPA size range")?;
    ensure!(
        offset < size,
        "DMA-BUF plane offset exceeds backing storage"
    );
    Ok(size)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use smithay::reexports::rustix::fs::{ftruncate, memfd_create, MemfdFlags};

    use super::*;

    #[test]
    fn backing_size_preserves_padding_and_checks_offset() {
        let fd = memfd_create("dma-layout-test", MemfdFlags::CLOEXEC).unwrap();
        ftruncate(&fd, 8192).unwrap();
        assert_eq!(backing_size(fd.as_fd(), 4096).unwrap(), 8192);
        assert!(backing_size(fd.as_fd(), 8192).is_err());
        ftruncate(&fd, u64::from(u32::MAX) + 1).unwrap();
        assert!(backing_size(fd.as_fd(), 0).is_err());
    }
}
