use std::collections::VecDeque;
use std::rc::Rc;

use smithay::utils::{Physical, Rectangle, Size};

/// Identity of successfully copied content, without wrapping sequence counters.
#[derive(Debug, Clone)]
pub struct ContentStamp(Rc<()>);

#[derive(Debug, Default)]
pub(super) struct ReadbackDamage {
    frames: VecDeque<(ContentStamp, Option<Rectangle<i32, Physical>>)>,
}

impl ReadbackDamage {
    pub fn reset(&mut self) {
        self.frames.clear();
    }

    pub fn record(&mut self, damage: &[Rectangle<i32, Physical>]) {
        let bounds = damage.iter().copied().reduce(|a, b| a.merge(b));
        self.frames.push_back((ContentStamp(Rc::new(())), bounds));
        if self.frames.len() > 16 {
            self.frames.pop_front();
        }
    }

    pub fn since(
        &self,
        stamp: Option<&ContentStamp>,
        size: Size<i32, Physical>,
    ) -> (ContentStamp, Option<Rectangle<i32, Physical>>) {
        let current = self
            .frames
            .back()
            .expect("record damage before selecting readback")
            .0
            .clone();
        let full = Rectangle::from_size(size);
        let previous = stamp.and_then(|stamp| {
            self.frames
                .iter()
                .position(|(old, _)| Rc::ptr_eq(&stamp.0, &old.0))
        });
        let Some(previous) = previous else {
            return (current, Some(full));
        };
        let bounds = self
            .frames
            .iter()
            .skip(previous + 1)
            .filter_map(|(_, rect)| *rect)
            .reduce(|a, b| a.merge(b));
        let bounds = bounds.map(|rect| {
            // One bounding rectangle caps GL calls even for fragmented damage.
            // Large regions keep the contiguous full-frame copy path.
            if !full.contains_rect(rect)
                || i64::from(rect.size.w) * i64::from(rect.size.h) * 2
                    >= i64::from(size.w) * i64::from(size.h)
            {
                full
            } else {
                rect
            }
        });
        (current, bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_each_destination_and_bounds_history() {
        let mut history = ReadbackDamage::default();
        let size = Size::from((100, 100));
        let full = Rectangle::from_size(size);
        history.record(&[full]);
        let (first, rect) = history.since(None, size);
        assert_eq!(rect, Some(full));
        let a = Rectangle::new((2, 3).into(), (4, 5).into());
        history.record(&[a]);
        let (second, rect) = history.since(Some(&first), size);
        assert_eq!(rect, Some(a));
        history.record(&[]);
        assert_eq!(history.since(Some(&first), size).1, Some(a));
        assert_eq!(history.since(Some(&second), size).1, None);
        for _ in 0..16 {
            history.record(&[]);
        }
        assert_eq!(history.since(Some(&first), size).1, Some(full));
        history.reset();
        history.record(&[]);
        assert_eq!(history.since(Some(&second), size).1, Some(full));
    }

    #[test]
    fn merges_missed_frames_and_falls_back_for_large_bounds() {
        let size = Size::from((100, 100));
        let full = Rectangle::from_size(size);
        let mut history = ReadbackDamage::default();
        history.record(&[full]);
        let (stamp, _) = history.since(None, size);
        let a = Rectangle::new((10, 10).into(), (2, 3).into());
        let b = Rectangle::new((20, 20).into(), (2, 3).into());
        history.record(&[a]);
        history.record(&[b]);
        assert_eq!(history.since(Some(&stamp), size).1, Some(a.merge(b)));
        history.record(&[Rectangle::new((90, 90).into(), (2, 3).into())]);
        assert_eq!(history.since(Some(&stamp), size).1, Some(full));
    }
}
