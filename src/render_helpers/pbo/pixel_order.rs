use std::borrow::Cow;

#[derive(Debug, Clone, Copy)]
pub(super) enum PixelOrder {
    Bgra,
    Rgba,
}

impl PixelOrder {
    pub(super) fn bgra(self, bytes: &[u8]) -> Cow<'_, [u8]> {
        match self {
            Self::Bgra => Cow::Borrowed(bytes),
            Self::Rgba => {
                let mut converted = bytes.to_vec();
                for pixel in converted.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Cow::Owned(converted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rgba_allocates_and_preserves_green_and_alpha() {
        let bytes = [1, 2, 3, 4, 5, 6, 7, 8];
        assert!(matches!(PixelOrder::Bgra.bgra(&bytes), Cow::Borrowed(_)));
        assert_eq!(&*PixelOrder::Rgba.bgra(&bytes), &[3, 2, 1, 4, 7, 6, 5, 8]);
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
