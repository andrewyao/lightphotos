// SPDX-License-Identifier: MIT OR Apache-2.0

//! One Loupe cache tier: decoded images by key, least recently used first,
//! with a running total of the bytes they hold, so eviction by bytes reads
//! one number rather than summing the tier.

use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

use lightwatch::Measured;

use crate::decode::image_decode::DecodedImage;

pub(crate) struct Tier<K> {
    images: HashMap<K, Arc<DecodedImage>>,
    /// Least recently used first.
    order: VecDeque<K>,
    /// `Measured::bytes` of every image in `images`.
    bytes: usize,
}

impl<K: Clone + Eq + Hash> Tier<K> {
    pub(crate) fn new() -> Self {
        Self {
            images: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&Arc<DecodedImage>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.images.get(key)
    }

    pub(crate) fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.images.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.images.len()
    }

    /// Files `img` under `key` as the most recently used entry, replacing
    /// any image already there.
    pub(crate) fn insert(&mut self, key: K, img: Arc<DecodedImage>) {
        let added = img.bytes();
        match self.images.insert(key.clone(), img) {
            Some(old) => {
                self.bytes = self.bytes.saturating_sub(old.bytes());
                self.touch(&key);
            }
            None => self.order.push_back(key),
        }
        self.bytes = self.bytes.saturating_add(added);
    }

    /// Marks `key` most recently used. A no-op for a key not held.
    pub(crate) fn touch<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.order.back().is_some_and(|k| k.borrow() == key) {
            return;
        }
        if let Some(i) = self.order.iter().position(|k| k.borrow() == key) {
            if let Some(k) = self.order.remove(i) {
                self.order.push_back(k);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.images.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// Evicts least recently used entries until at most `max_len` remain
    /// and they hold at most `max_bytes`, skipping every key `keep` accepts.
    /// Stops over the caps when only kept keys remain. Returns the evicted
    /// keys.
    pub(crate) fn evict(
        &mut self,
        max_len: usize,
        max_bytes: usize,
        keep: impl Fn(&K) -> bool,
    ) -> Vec<K> {
        let mut evicted = Vec::new();
        while self.images.len() > max_len || self.bytes > max_bytes {
            let Some(i) = self.order.iter().position(|k| !keep(k)) else {
                break;
            };
            let Some(key) = self.order.remove(i) else {
                break;
            };
            if let Some(img) = self.images.remove(&key) {
                self.bytes = self.bytes.saturating_sub(img.bytes());
            }
            evicted.push(key);
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::image_decode::{DecodedImageFields, PixelFormat};

    fn image(bytes: usize) -> Arc<DecodedImage> {
        Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: 1,
            height: 1,
            rgba: vec![0; bytes],
            pixel_format: PixelFormat::Srgb8,
        }))
    }

    /// A replaced image's bytes leave the total, so the cap counts what is
    /// held, not every insert.
    #[test]
    fn the_running_total_follows_every_insert_replace_and_clear() {
        let mut tier = Tier::new();
        let (small, big) = (image(100), image(300));
        let both = small.bytes() + big.bytes();
        tier.insert("a", Arc::clone(&small));
        tier.insert("b", Arc::clone(&big));
        assert!(tier.evict(usize::MAX, both, |_| false).is_empty());
        tier.insert("a", Arc::clone(&big));
        assert_eq!(tier.evict(usize::MAX, both, |_| false), ["b"]);
        tier.clear();
        tier.insert("c", big);
        assert!(tier.evict(usize::MAX, both, |_| false).is_empty());
    }

    #[test]
    fn eviction_takes_the_least_recently_used_and_skips_kept_keys() {
        let mut tier = Tier::new();
        for k in ["a", "b", "c", "d"] {
            tier.insert(k, image(100));
        }
        tier.touch("a");
        let evicted = tier.evict(2, usize::MAX, |k| *k == "b");
        assert_eq!(evicted, ["c", "d"]);
        assert!(tier.contains("a") && tier.contains("b"));
        assert!(tier.evict(0, 0, |_| true).is_empty(), "only kept keys left");
        assert!(tier.contains("a") && tier.contains("b"));
    }
}
