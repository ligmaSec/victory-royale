// Build host-owned helpers once; the runtime never stages compiler or shader files.
use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

struct Helper {
    file: &'static str,
    id: &'static str,
    entry: &'static str,
    stage: &'static str,
    root: bool,
    variant: u8,
    defines: Option<(&'static str, &'static str)>,
}

const HELPERS: &[Helper] = &[
    Helper {
        file: "plane_corners",
        id: "plane_corners",
        entry: "corner",
        stage: "vs",
        root: false,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "plane_steps",
        id: "plane_steps",
        entry: "plane",
        stage: "ps",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "plane_floats",
        id: "plane_floats",
        entry: "plane",
        stage: "ps",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "card_compare",
        id: "card_compare",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "compose",
        id: "compose_vs",
        entry: "vs",
        stage: "vs",
        root: false,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "compose",
        id: "compose_ps",
        entry: "ps",
        stage: "ps",
        root: false,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "inplace_probe",
        id: "inplace_probe",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "dirty_page_count",
        id: "dirty_page_count",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "card_gather",
        id: "card_gather",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "card_scatter",
        id: "card_scatter",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "upload_check",
        id: "upload_check",
        entry: "main",
        stage: "cs",
        root: true,
        variant: 0,
        defines: None,
    },
    Helper {
        file: "indirect_clamp",
        id: "indirect_clamp_65535_0",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 1,
        defines: Some(("WIDEST=65535u", "REST=0")),
    },
    Helper {
        file: "indirect_clamp",
        id: "indirect_clamp_65535_1",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 2,
        defines: Some(("WIDEST=65535u", "REST=1")),
    },
    Helper {
        file: "indirect_clamp",
        id: "indirect_clamp_1048576_0",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 3,
        defines: Some(("WIDEST=1048576u", "REST=0")),
    },
    Helper {
        file: "indirect_clamp",
        id: "indirect_clamp_1048576_1",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 4,
        defines: Some(("WIDEST=1048576u", "REST=1")),
    },
    Helper {
        file: "indirect_batch",
        id: "indirect_batch_65535_0",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 1,
        defines: Some(("WIDEST=65535u", "REST=0")),
    },
    Helper {
        file: "indirect_batch",
        id: "indirect_batch_65535_1",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 2,
        defines: Some(("WIDEST=65535u", "REST=1")),
    },
    Helper {
        file: "indirect_batch",
        id: "indirect_batch_1048576_0",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 3,
        defines: Some(("WIDEST=1048576u", "REST=0")),
    },
    Helper {
        file: "indirect_batch",
        id: "indirect_batch_1048576_1",
        entry: "main",
        stage: "cs",
        root: false,
        variant: 4,
        defines: Some(("WIDEST=1048576u", "REST=1")),
    },
];

pub fn bundle() {
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
    let mut index = String::from(
        "fn bundled_shader(source: &str, entry: &[u8], profile: &[u8], variant: u8) -> Option<&'static [u8]> {\n    match (source, entry, profile, variant) {\n",
    );
    let mut tests =
        String::from("#[cfg(test)]\nconst BUNDLED_HELPERS: &[(&str, &[u8], bool)] = &[\n");
    let mut package = String::new();
    for helper in HELPERS {
        let Helper {
            file,
            id,
            entry,
            stage,
            root,
            variant,
            defines,
        } = *helper;
        let source = format!("shaders/{file}.hlsl");
        println!("cargo:rerun-if-changed={source}");
        let mut command = Command::new(&compiler);
        command.args(["-T", &format!("{stage}_6_0"), "-E", entry, "-O3"]);
        if let Some((width, rest)) = defines {
            command.args(["-D", width, "-D", rest]);
        }
        let status = command
            .arg("-Fo")
            .arg(out.join(format!("{id}.dxil")))
            .arg(&source)
            .status()
            .unwrap_or_else(|e| panic!("could not start {} for {source}: {e}", compiler.display()));
        assert!(status.success(), "DXC failed for {source} ({status})");
        writeln!(package, "{id}").unwrap();
        let code = format!("include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{id}.dxil\"))");
        writeln!(index, "        (include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{source}\")), b{entry:?}, b\"{stage}_5_1\", {variant}) => Some({code}),").unwrap();
        writeln!(tests, "    ({id:?}, {code}, {root}),").unwrap();
    }
    index.push_str("        _ => None,\n    }\n}\n");
    tests.push_str("];\n");
    index.push_str(&tests);
    std::fs::write(out.join("bundled_hlsl.rs"), index).unwrap();
    std::fs::write(out.join("package-hlsl.list"), package).unwrap();
    println!("cargo:rustc-cfg=victory_bundled_hlsl");
}
