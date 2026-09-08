use std::collections::VecDeque;
use std::rc::Rc;

use smithay::utils::{Physical, Rectangle, Size};

/// Identity of successfully copied content, without wrapping sequence counters.
#[derive(Debug, Clone)]
pub struct ContentStamp {
    epoch: Rc<()>,
    frame: u64,
}

#[derive(Debug, Default)]
pub(super) struct ReadbackDamage {
    frames: VecDeque<(u64, Option<Rectangle<i32, Physical>>)>,
    epoch: Rc<()>,
    sequence: u64,
}

impl ReadbackDamage {
    pub fn reset(&mut self) {
        self.frames.clear();
        self.epoch = Rc::new(());
        self.sequence = 0;
    }

    pub fn record(&mut self, damage: &[Rectangle<i32, Physical>]) {
        let bounds = damage.iter().copied().reduce(|a, b| a.merge(b));
        if self.sequence == u64::MAX {
            self.reset();
        }
        self.sequence += 1;
        if self.frames.len() == 16 {
            self.frames.pop_front();
        }
        self.frames.push_back((self.sequence, bounds));
    }

    pub fn since(
        &self,
        stamp: Option<&ContentStamp>,
        size: Size<i32, Physical>,
    ) -> (ContentStamp, Option<Rectangle<i32, Physical>>) {
        let current = ContentStamp {
            epoch: self.epoch.clone(),
            frame: self.sequence,
        };
        let full = Rectangle::from_size(size);
        let previous = stamp
            .filter(|stamp| Rc::ptr_eq(&stamp.epoch, &self.epoch))
            .and_then(|stamp| self.frames.iter().position(|(old, _)| stamp.frame == *old));
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
    fn counter_wrap_and_reset_never_reuse_old_content_identity() {
        let mut history = ReadbackDamage::default();
        let size = Size::from((16, 16));
        history.record(&[]);
        let (old, _) = history.since(None, size);
        history.record(&[]);
        let (next, _) = history.since(Some(&old), size);
        assert!(
            Rc::ptr_eq(&old.epoch, &next.epoch),
            "ordinary frames must not allocate identities"
        );
        history.sequence = u64::MAX;
        history.record(&[]);
        let (new, damage) = history.since(Some(&old), size);
        assert_eq!(new.frame, old.frame);
        assert!(!Rc::ptr_eq(&new.epoch, &old.epoch));
        assert_eq!(damage, Some(Rectangle::from_size(size)));
    }

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
