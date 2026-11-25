use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(victory_bundled_hlsl)");
    println!("cargo:rerun-if-env-changed=VICTORY_BUILD_DXC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Some(compiler) = env::var_os("VICTORY_BUILD_DXC").map(PathBuf::from) else {
        return;
    };
    if compiler.as_os_str().is_empty() {
        println!("cargo:warning=VICTORY_BUILD_DXC is empty. Skipping GPU helper shader embedding.");
        return;
    }
    println!("cargo:rerun-if-changed={}", compiler.display());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    for name in [
        "argument_clamp",
        "rect_rewrite",
        "rect_expand",
        "packed_copy",
    ] {
        let source = format!("shaders/{name}.hlsl");
        println!("cargo:rerun-if-changed={source}");
        let status = Command::new(&compiler)
            .args(["-T", "cs_6_0", "-E", "main", "-O3", "-Fo"])
            .arg(out.join(format!("{name}.dxil")))
            .arg(&source)
            .status()
            .unwrap_or_else(|e| panic!("could not start {} for {source}: {e}", compiler.display()));
        assert!(status.success(), "DXC failed for {source} ({status})");
    }
    println!("cargo:rustc-cfg=victory_bundled_hlsl");
}
