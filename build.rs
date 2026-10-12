use std::path::Path;
use std::process::Command;

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("ui/app.slint");
    println!("cargo:rerun-if-changed=lang");

    build_relay();
}

/// Cross-compiles the Windows pipe relay (`bridge/rvrelay.c`) with zig so the
/// app can embed it and install it at runtime. The relay runs under the
/// service's Wine prefix to reach the named pipe.
fn build_relay() {
    println!("cargo:rerun-if-changed=bridge/rvrelay.c");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let exe = Path::new(&out).join("rvrelay.exe");
    let status = Command::new("zig")
        .args(["cc", "-target", "x86-windows-gnu", "-O2", "-o"])
        .arg(&exe)
        .arg("bridge/rvrelay.c")
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("zig failed to build rvrelay.exe ({s})"),
        Err(e) => panic!(
            "could not run zig to build rvrelay.exe: {e}. Install zig (the relay \
             is cross-compiled with `zig cc -target x86-windows-gnu`)."
        ),
    }
}
