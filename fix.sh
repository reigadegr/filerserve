#!/bin/sh
export RUSTFLAGS="
    --cfg tokio_unstable
    -C link-arg=-fuse-ld=mold
"
cargo clippy --workspace --fix --allow-dirty --allow-staged --all --all-targets --all-features --no-deps
