# SPDX-License-Identifier: MIT OR Apache-2.0

# The environment every browser build runs under. Source it, don't run it:
#
#   source scripts/web-env.sh && trunk serve --release
#
# The browser build runs the decode workers as wasm threads over one shared
# memory, which needs std rebuilt with atomics, and that needs nightly. The
# nightly is pinned to a date so this machine and CI build with the same
# compiler: an April 2026 nightly failed to link naga's libm calls under
# build-std, and a later one may break something else. Move the date on
# purpose, then run the web benchmark (tools/web-bench) against the result.
# Native builds stay on the stable toolchain rust-toolchain.toml names.

WEB_TOOLCHAIN=nightly-2026-09-28

export RUSTUP_TOOLCHAIN="$WEB_TOOLCHAIN"
# A Homebrew `rust` cargo ahead of rustup's on PATH ignores RUSTUP_TOOLCHAIN
# and has no wasm32 target, so put the pinned toolchain's own bin first.
_web_toolchain_bin="$(find "${RUSTUP_HOME:-$HOME/.rustup}/toolchains" -maxdepth 2 \
  -path "*/$WEB_TOOLCHAIN-*/bin" -type d 2>/dev/null | head -1)"
if [ -n "$_web_toolchain_bin" ]; then
  export PATH="$_web_toolchain_bin:$PATH"
  # The rustup proxy would have set this for rust-lld, which can't find
  # libLLVM.dylib without it; calling the toolchain's cargo directly doesn't.
  if [ "$(uname)" = Darwin ]; then
    export DYLD_FALLBACK_LIBRARY_PATH="${_web_toolchain_bin%/bin}/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
  fi
fi
unset _web_toolchain_bin
export CARGO_UNSTABLE_BUILD_STD=std,panic_abort
# Its own target directory, so a web build does not evict the native build's
# artifacts, which a different compiler and a rebuilt std would otherwise do.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR_WEB:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/wasm}"
# The cfg brings in wgpu's WebGPU and the File System Access bindings, and it
# lives here because trunk 0.21.14 does not pass Trunk.toml's `rustflags`
# through to cargo. The link arguments give wasm-bindgen a shared, imported
# memory with thread-local storage. `__heap_base` is exported by hand because
# current lld stopped exporting it, and wasm-bindgen's thread transform fails
# without it. `simd128` lets mozjpeg's color conversion and the compiler's
# vectorized loops use WASM SIMD; every browser with WebGPU has it.
export RUSTFLAGS="--cfg=web_sys_unstable_apis \
  -C target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128 \
  -C link-arg=--shared-memory \
  -C link-arg=--max-memory=4294967296 \
  -C link-arg=--import-memory \
  -C link-arg=--export=__wasm_init_tls \
  -C link-arg=--export=__tls_size \
  -C link-arg=--export=__tls_align \
  -C link-arg=--export=__tls_base \
  -C link-arg=--export=__heap_base"
