//! Warn loudly when OpenH264 would be built without its x86 SIMD assembly.
//!
//! `openh264-sys2` silently falls back to plain C when `nasm` is missing,
//! which made the Windows encoder 3-7x slower in the first real test.

fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !matches!(arch.as_str(), "x86_64" | "x86") || std::env::var("OPENH264_NO_ASM").is_ok() {
        return;
    }
    let nasm_ok = std::process::Command::new("nasm")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !nasm_ok {
        println!(
            "cargo:warning=nasm not found: OpenH264 will be built WITHOUT SIMD assembly and \
             encode several times slower. Install it (macOS: `brew install nasm`, \
             Windows: `winget install nasm`) and run `cargo clean -p openh264-sys2`."
        );
    }
}
