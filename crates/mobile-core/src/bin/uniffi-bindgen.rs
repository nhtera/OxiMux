//! Binding generator (library mode):
//!   cargo run --bin uniffi-bindgen -- generate --library \
//!     target/debug/liboximux_mobile_core.dylib --language swift --out-dir out/swift
fn main() {
    uniffi::uniffi_bindgen_main()
}
