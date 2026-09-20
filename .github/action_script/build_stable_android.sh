#!/bin/bash

unset RUSTFLAGS

export RUSTFLAGS="
    --cfg tokio_unstable
    -C default-linker-libraries
    -C link-arg=-fuse-ld=mold
    -C symbol-mangling-version=v0
    -C llvm-args=-enable-misched
    -C llvm-args=-enable-post-misched
    -C llvm-args=-enable-dfa-jump-thread
    -C link-arg=-Wl,--no-rosegment
    -C link-arg=-Wl,--sort-section=alignment
    -C link-args=-Wl,-O3,--gc-sections,--as-needed
    -C link-args=-Wl,-x,-z,noexecstack,--pack-dyn-relocs=relr,-s,--strip-all,--relax
"

echo $RUSTFLAGS

export CARGO_TERM_COLOR=always

export JEMALLOC_SYS_DISABLE_WARN_ERROR=1

export ANDROID_NDK_HOME=$(realpath ~/ndk_temp)

export ANDROID_NDK_ROOT=$ANDROID_NDK_HOME

cargo +stable ndk --platform 35 -t arm64-v8a build --target "$1" -r --bin "$2" --all-features
