//! Pictures handed to egui frame after frame (video in a call, a video
//! attachment playing), written over once egui is done with them.
//!
//! egui holds a picture only until it is uploaded (in the frame that draws
//! it), so by the next picture one is free again and the pixels go into
//! memory that is already there. A new picture each time is a new block of
//! several MB, which Windows hands out as fresh pages: measured, that alone
//! cost a screen share a few percent of a core in page faults.

use std::sync::Arc;

use egui::{Color32, ColorImage};

pub struct Pictures {
    kept: Vec<Arc<ColorImage>>,
    /// How many to keep: as many as can be out at once (queued, being
    /// uploaded, being written).
    most: usize,
}

impl Pictures {
    pub fn new(most: usize) -> Pictures {
        Pictures {
            kept: Vec::new(),
            most,
        }
    }

    /// A picture of `size` that nobody else holds, to write into (its
    /// pixels are whatever was there).
    pub fn take(&mut self, size: [usize; 2]) -> Arc<ColorImage> {
        self.kept.retain(|p| p.size == size);
        match self.kept.iter_mut().position(|p| Arc::get_mut(p).is_some()) {
            Some(i) => self.kept.swap_remove(i),
            None => Arc::new(ColorImage::new(
                size,
                vec![Color32::BLACK; size[0] * size[1]],
            )),
        }
    }

    /// Keeps a picture just handed out, to write over later.
    pub fn keep(&mut self, img: &Arc<ColorImage>) {
        if self.kept.len() < self.most {
            self.kept.push(img.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_written_over_once_egui_lets_go() {
        let mut pool = Pictures::new(3);
        let first = pool.take([2, 2]);
        pool.keep(&first);
        // egui still holds it (not uploaded yet): the next one is new.
        let second = pool.take([2, 2]);
        assert!(!Arc::ptr_eq(&first, &second));
        pool.keep(&second);
        let was = Arc::as_ptr(&first);
        drop(first);
        // Uploaded and let go: written over instead of a new one.
        let third = pool.take([2, 2]);
        assert_eq!(Arc::as_ptr(&third), was);
        // Another size is never reused.
        assert_eq!(pool.take([4, 4]).size, [4, 4]);
    }

    #[test]
    fn only_so_many_are_kept() {
        let mut pool = Pictures::new(2);
        let held: Vec<_> = (0..4)
            .map(|_| {
                let p = pool.take([1, 1]);
                pool.keep(&p);
                p
            })
            .collect();
        assert_eq!(pool.kept.len(), 2);
        drop(held);
    }
}
