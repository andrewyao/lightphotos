// SPDX-License-Identifier: GPL-3.0-or-later

//! How many decoded images each `Loader` tier keeps in memory. One value per
//! target, so a platform with less memory to spare, or a slower disk behind
//! the thumbnail cache, is tuned here rather than in the loader.
//!
//! `LIGHTPHOTOS_CACHE_THUMBS`, `_PREVIEWS` and `_FULLS` override a target's
//! defaults for a tuning run. The browser build has no environment, so it
//! always gets its defaults.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheLimits {
    /// 512px thumbnails, about 700 KB each. A floor: the loader raises it to
    /// the grid's working set so a large grid never evicts itself.
    pub thumbs: usize,
    /// Loupe previews, about 5 MB each: the current photo plus a few
    /// neighbors either way.
    pub previews: usize,
    /// Full-resolution decodes, a whole RGBA8 frame each (about 180 MB at
    /// 45 MP). They only happen on zoom, so extra entries do not help
    /// navigation.
    pub fulls: usize,
    /// Most decode threads the loader starts, below its default of every
    /// core but two. On wasm32 every thread's decode scratch comes out of one
    /// shared heap of at most 4 GB, which never shrinks.
    pub decode_threads: usize,
}

impl CacheLimits {
    #[cfg(target_os = "macos")]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        fulls: 3,
        decode_threads: usize::MAX,
    };

    #[cfg(target_os = "windows")]
    pub const PLATFORM: CacheLimits = CacheLimits {
        thumbs: 256,
        previews: 8,
        fulls: 3,
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
        fulls: 3,
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
        fulls: 1,
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
        CacheLimits {
            thumbs: pick("LIGHTPHOTOS_CACHE_THUMBS", self.thumbs),
            previews: pick("LIGHTPHOTOS_CACHE_PREVIEWS", self.previews),
            fulls: pick("LIGHTPHOTOS_CACHE_FULLS", self.fulls),
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
    fn a_zero_or_garbled_override_keeps_the_default() {
        for bad in ["0", "lots", "-3", ""] {
            let limits = CacheLimits::PLATFORM.overridden_by(|_| Some(bad.to_string()));
            assert_eq!(limits, CacheLimits::PLATFORM, "override {bad:?}");
        }
    }
}
