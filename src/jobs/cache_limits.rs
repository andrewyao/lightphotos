// SPDX-License-Identifier: MIT OR Apache-2.0

//! How many decoded images, and how many bytes of them, each `Loader` tier
//! keeps in memory. One value per target, so a platform with less memory to
//! spare, or a slower disk behind the thumbnail cache, is tuned here rather
//! than in the loader.
//!
//! `LIGHTPHOTOS_CACHE_THUMBS`, `_PREVIEWS`, `_FULLS`, `_PREVIEW_MB` and
//! `_FULL_MB` override a target's defaults for a tuning run. The browser
//! build has no environment, so it always gets its defaults.

const MIB: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheLimits {
    /// 512px thumbnails, about 700 KB each. A floor: the loader raises it to
    /// the grid's working set so a large grid never evicts itself.
    pub thumbs: usize,
    /// Loupe previews: the current photo plus a few neighbors either way.
    /// One is sized to the window's longest side, up to 4096 px: 45 MB at
    /// 4096 x 2731 in RGBA8, twice that for a macOS RAW, which decodes to
    /// half floats.
    pub previews: usize,
    /// Most bytes of pixels the preview tier holds. Eviction stops at the
    /// newest preview, so a single large one still caches.
    pub preview_bytes: usize,
    /// Full-resolution decodes, a whole RGBA8 frame each (about 180 MB at
    /// 45 MP), or 8 bytes a pixel for a macOS RAW (360 MB). They only happen
    /// on zoom or on a screen larger than the preview, and the loader drops
    /// them when the Loupe closes.
    pub fulls: usize,
    /// Most bytes of pixels the full tier holds. Eviction stops at the
    /// newest decode and the Loupe's photo, so a single large one still
    /// caches.
    pub full_bytes: usize,
    /// Most decode threads the loader starts, below its default of every
    /// core but two. On wasm32 every thread's decode scratch comes out of one
    /// shared heap of at most 4 GB, which never shrinks.
    pub decode_threads: usize,
}

impl CacheLimits {
    /// Eight RGBA8 previews at 4096 px, or four half-float RAW previews.
    #[cfg(not(target_arch = "wasm32"))]
    const NATIVE_PREVIEW_BYTES: usize = 384 * MIB;

    /// Three half-float decodes of a 48 MP RAW: the Loupe's photo and both
    /// neighbors, which `App::request_neighbors` prefetches on a large
    /// screen only when all three fit. A larger photo keeps fewer.
    #[cfg(not(target_arch = "wasm32"))]
    const NATIVE_FULL_BYTES: usize = 1152 * MIB;

    #[cfg(target_os = "macos")]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        preview_bytes: Self::NATIVE_PREVIEW_BYTES,
        fulls: 3,
        full_bytes: Self::NATIVE_FULL_BYTES,
        decode_threads: usize::MAX,
    };

    #[cfg(target_os = "windows")]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        preview_bytes: Self::NATIVE_PREVIEW_BYTES,
        fulls: 3,
        full_bytes: Self::NATIVE_FULL_BYTES,
        decode_threads: usize::MAX,
    };

    #[cfg(all(
        not(target_os = "macos"),
        not(target_os = "windows"),
        not(target_arch = "wasm32")
    ))]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        preview_bytes: Self::NATIVE_PREVIEW_BYTES,
        fulls: 3,
        full_bytes: Self::NATIVE_FULL_BYTES,
        decode_threads: usize::MAX,
    };

    /// A 32-bit address space holds about 180 MB of thumbnails at 256, and a
    /// tab that runs out of memory is killed rather than slowed down. A full
    /// decode here is `LinearF16`, 8 bytes a pixel (360 MB at 45 MP), so only
    /// the photo being zoomed is kept.
    #[cfg(target_arch = "wasm32")]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        preview_bytes: 256 * MIB,
        fulls: 1,
        full_bytes: 512 * MIB,
        decode_threads: 6,
    };

    /// This target's defaults, with any `LIGHTPHOTOS_CACHE_*` override applied.
    pub fn from_env() -> CacheLimits {
        Self::PLATFORM.overridden_by(|key| std::env::var(key).ok())
    }

    /// A zero override, or one that does not parse, is ignored, since an
    /// empty tier would re-decode on every frame.
    fn overridden_by(self, var: impl Fn(&str) -> Option<String>) -> CacheLimits {
        let pick = |key: &str, fallback: usize| {
            var(key)
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(fallback)
        };
        let pick_mb = |key: &str, fallback: usize| {
            var(key)
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .map_or(fallback, |mb| mb.saturating_mul(MIB))
        };
        CacheLimits {
            thumbs: pick("LIGHTPHOTOS_CACHE_THUMBS", self.thumbs),
            previews: pick("LIGHTPHOTOS_CACHE_PREVIEWS", self.previews),
            preview_bytes: pick_mb("LIGHTPHOTOS_CACHE_PREVIEW_MB", self.preview_bytes),
            fulls: pick("LIGHTPHOTOS_CACHE_FULLS", self.fulls),
            full_bytes: pick_mb("LIGHTPHOTOS_CACHE_FULL_MB", self.full_bytes),
            decode_threads: pick("LIGHTPHOTOS_DECODE_THREADS", self.decode_threads),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_replaces_only_the_tier_it_names() {
        let limits = CacheLimits::PLATFORM
            .overridden_by(|key| (key == "LIGHTPHOTOS_CACHE_THUMBS").then(|| "1024".to_string()));
        assert_eq!(
            limits,
            CacheLimits {
                thumbs: 1024,
                ..CacheLimits::PLATFORM
            }
        );
    }

    #[test]
    fn a_byte_override_is_read_in_megabytes() {
        let limits = CacheLimits::PLATFORM
            .overridden_by(|key| (key == "LIGHTPHOTOS_CACHE_FULL_MB").then(|| "64".to_string()));
        assert_eq!(limits.full_bytes, 64 * MIB);
        assert_eq!(limits.preview_bytes, CacheLimits::PLATFORM.preview_bytes);
    }

    #[test]
    fn a_zero_or_garbled_override_keeps_the_default() {
        for bad in ["0", "lots", "-3", ""] {
            let limits = CacheLimits::PLATFORM.overridden_by(|_| Some(bad.to_string()));
            assert_eq!(limits, CacheLimits::PLATFORM, "override {bad:?}");
        }
    }
}
