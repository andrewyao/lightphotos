//! A minimal, dependency-free FNV-1a 64-bit hasher.
//!
//! Stable and deterministic across process runs (unlike
//! `std::collections::hash_map::DefaultHasher`), so it's safe to key persistent
//! artifacts on. Shared by the on-disk thumbnail cache key (`thumbnail`) and the
//! edit signature (`develop`).

pub(crate) struct Fnv1a {
    state: u64,
}

impl Fnv1a {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    pub(crate) fn new() -> Self {
        Fnv1a { state: Self::OFFSET_BASIS }
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state ^= b as u64;
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    pub(crate) fn finish(&self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_key_is_stable_and_deterministic() {
        let mut a = Fnv1a::new();
        a.write(b"/abs/IMG_001.jpg");
        a.write(&123u64.to_le_bytes());
        a.write(&456u64.to_le_bytes());
        a.write(&256u32.to_le_bytes());

        let mut b = Fnv1a::new();
        b.write(b"/abs/IMG_001.jpg");
        b.write(&123u64.to_le_bytes());
        b.write(&456u64.to_le_bytes());
        b.write(&256u32.to_le_bytes());

        assert_eq!(a.finish(), b.finish(), "identical inputs -> identical key");

        // Known FNV-1a-64 anchor: empty input hashes to the offset basis.
        assert_eq!(Fnv1a::new().finish(), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a_key_differs_when_max_px_differs() {
        let mk = |max_px: u32| {
            let mut h = Fnv1a::new();
            h.write(b"/abs/IMG_001.jpg");
            h.write(&123u64.to_le_bytes());
            h.write(&456u64.to_le_bytes());
            h.write(&max_px.to_le_bytes());
            h.finish()
        };
        assert_ne!(mk(256), mk(512), "different max_px -> different key");
    }
}
