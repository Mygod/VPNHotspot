//! Platform-neutral daemon logic shared by the Android daemon binary and Rust unit tests.
//!
//! Android runtime code is split between `src/shizuku/`, `src/root/`, and the cross-mode modules at `src/`;
//! shared modules live under `src/shared/`.

pub mod shared;
