use std::collections::{BTreeMap, BTreeSet};

/// Coalesced free descriptor ranges, indexed both for reuse and for release.
pub(super) struct DynamicSlots {
    by_start: BTreeMap<u32, u32>,
    by_size: BTreeSet<(u32, u32)>,
    free: u32,
}

impl DynamicSlots {
    pub(super) fn new(count: u32) -> Self {
        let mut slots = Self {
            by_start: BTreeMap::new(),
            by_size: BTreeSet::new(),
            free: 0,
        };
        if count != 0 {
            slots.insert(0, count);
        }
        slots
    }

    pub(super) fn take(&mut self, count: u32) -> Option<u32> {
        // Fill the smallest suitable hole before splitting a larger range.
        let &(size, first) = self.by_size.range((count, 0)..).next()?;
        self.remove(first, size);
        if size > count {
            self.insert(first + count, size - count);
        }
        Some(first)
    }

    #[cfg(any(test, feature = "diagnostics"))]
    pub(super) fn free(&self) -> u32 {
        self.free
    }

    #[cfg(any(test, feature = "diagnostics"))]
    pub(super) fn is_free(&self, slot: u32) -> bool {
        self.by_start
            .range(..=slot)
            .next_back()
            .is_some_and(|(&start, &size)| slot - start < size)
    }

    pub(super) fn release(&mut self, mut first: u32, mut end: u32) {
        if first >= end {
            return;
        }
        if let Some((&start, &size)) = self.by_start.range(..=first).next_back()
            && start + size >= first
        {
            first = start;
            end = end.max(start + size);
            self.remove(start, size);
        }
        while let Some((&start, &size)) = self.by_start.range(first..=end).next() {
            end = end.max(start + size);
            self.remove(start, size);
        }
        self.insert(first, end - first);
    }

    fn remove(&mut self, first: u32, size: u32) {
        self.by_start.remove(&first);
        self.by_size.remove(&(size, first));
        self.free -= size;
    }

    fn insert(&mut self, first: u32, size: u32) {
        self.by_start.insert(first, size);
        self.by_size.insert((size, first));
        self.free += size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_small_holes_without_splitting_large_ranges() {
        let mut slots = DynamicSlots::new(20);
        for (count, first) in [(8, 0), (2, 8), (4, 10), (6, 14)] {
            assert_eq!(slots.take(count), Some(first));
        }
        slots.release(0, 8);
        slots.release(10, 14);
        assert_eq!(slots.take(3), Some(10));
        assert_eq!(slots.take(8), Some(0));
        assert_eq!(slots.free(), 1);
        assert!(slots.is_free(13));
    }

    #[test]
    fn coalesces_partial_releases_without_counting_overlap_twice() {
        let mut slots = DynamicSlots::new(20);
        assert_eq!(slots.take(20), Some(0));
        slots.release(0, 2);
        slots.release(18, 20);
        assert_eq!(slots.free(), 4);
        assert_eq!(slots.take(3), None);
        slots.release(4, 8);
        slots.release(1, 6);
        assert_eq!(slots.free(), 10);
        assert_eq!(slots.take(8), Some(0));
        slots.release(0, 18);
        assert_eq!(slots.take(20), Some(0));
        assert_eq!(slots.take(1), None);
    }
}
