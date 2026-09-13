//! The bindings generator, built from this crate so it cannot drift from it.
//!
//! UniFFI's scaffolding and its generated Kotlin agree on a checksum per
//! function. A separately installed `uniffi-bindgen` of a different version
//! computes different ones, and the mismatch surfaces at *runtime*, in the
//! host, as an unhelpful panic. Building the generator here makes the version
//! the same by construction.

fn main() {
    uniffi::uniffi_bindgen_main()
}
