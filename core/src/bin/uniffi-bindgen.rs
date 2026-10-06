//! Generates the foreign-language bindings for the core. See
//! `apps/macos/build.sh` for how it is invoked.

fn main() {
    uniffi::uniffi_bindgen_main()
}
