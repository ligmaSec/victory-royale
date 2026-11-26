//! Our own compute shaders (the indirect argument clamp, the rectangle
//! expander), compiled at run time from HLSL with their root signature
//! inside: by the system's compiler (`D3DCompile`, `cs_5_1`) and, when it
//! refuses them, by DXC (`cs_6_0`). Wine's `d3dcompiler_47` (vkd3d-shader)
//! can reject raw-buffer operations or return bytecode without the HLSL root
//! signature. The bundled and runtime DXC paths keep those bindings.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::{PCSTR, s};

/// Compiles compute shader `source` (entry `main`) named `name` in
/// messages: the system's compiler first, then DXC.
pub(crate) fn compile_compute(name: &str, source: &str) -> windows::core::Result<Vec<u8>> {
    match system_compile(name, source) {
        Ok(code) if has_root_signature(&code) => return Ok(code),
        Ok(_) => tracing::info!("{name}: system compiler omitted the root signature"),
        Err((e, msg)) => {
            tracing::info!(error = %e, "{name}: system compiler refused it ({})", msg.trim())
        }
    }
    #[cfg(victory_bundled_hlsl)]
    if let Some(bytes) = bundled_shader(source) {
        tracing::info!("{name}: using build-time DXC shader");
        return Ok(bytes.to_vec());
    }
    dxc_compile(name, source).inspect_err(|e| tracing::warn!(error = %e, "{name}: DXC failed"))
}

fn has_root_signature(code: &[u8]) -> bool {
    use victory_shader::{Container, FourCC};
    Container::parse(code).is_ok_and(|c| c.part(FourCC::RTS0).is_some())
}

// Match the source so a changed shader can never pick up another helper's bytecode.
#[cfg(victory_bundled_hlsl)]
fn bundled_shader(source: &str) -> Option<&'static [u8]> {
    macro_rules! bytecode {
        ($name:literal) => {
            include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".dxil"))
        };
    }
    match source {
        include_str!("../shaders/argument_clamp.hlsl") => Some(bytecode!("argument_clamp")),
        include_str!("../shaders/rect_rewrite.hlsl") => Some(bytecode!("rect_rewrite")),
        include_str!("../shaders/rect_expand.hlsl") => Some(bytecode!("rect_expand")),
        include_str!("../shaders/packed_copy.hlsl") => Some(bytecode!("packed_copy")),
        #[cfg(test)]
        include_str!("../shaders/table_probe.hlsl") => Some(bytecode!("table_probe")),
        _ => None,
    }
}

/// Compiles `source` with the system's `D3DCompile` as `cs_5_1`; on failure
/// returns the error and the compiler's messages.
pub(crate) fn system_compile(
    name: &str,
    source: &str,
) -> Result<Vec<u8>, (windows::core::Error, String)> {
    let cname = std::ffi::CString::new(name).unwrap_or_default();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers are ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(cname.as_ptr().cast()),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        return Err((
            e,
            errors
                .as_ref()
                .map(blob_bytes)
                .map(String::from_utf8_lossy)
                .map(Into::into)
                .unwrap_or_default(),
        ));
    }
    code.as_ref()
        .map(|c| blob_bytes(c).to_vec())
        .ok_or_else(|| (windows::core::Error::empty(), String::new()))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, borrowed for the blob's lifetime.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// Where `dxcompiler.dll` is looked for: `VICTORY_DXC` (the DLL or its
/// folder), beside the `dxil.dll` shader validation uses (the SDK ships them
/// together), then the executable's folder and System32.
fn dxc_library() -> windows::core::Result<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows::core::{HSTRING, w};
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("VICTORY_DXC").map(std::path::PathBuf::from) {
        paths.push(if p.is_dir() {
            p.join("dxcompiler.dll")
        } else {
            p
        });
    }
    if let Some(dir) = victory_shader::emit::validator::Validator::default_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        paths.push(dir.join("dxcompiler.dll"));
    }
    for path in paths.iter().filter(|p| p.is_file()) {
        // SAFETY: a NUL-terminated path; its folder (dxil.dll) is searched first.
        if let Ok(m) = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        } {
            return Ok(m);
        }
    }
    // SAFETY: a bare name, searched only in the executable's folder and System32.
    unsafe {
        LoadLibraryExW(
            w!("dxcompiler.dll"),
            None,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
}

/// Compiles compute shader `source` with DXC as `cs_6_0` (see
/// [`dxc_library`]); `name` labels its messages.
pub(crate) fn dxc_compile(name: &str, source: &str) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::{Interface, w};
    type Create = unsafe extern "system" fn(
        *const windows::core::GUID,
        *const windows::core::GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    // SAFETY: loading the compiler and calling its documented entry point with
    // out pointers of ours; the buffer and arguments outlive the calls.
    unsafe {
        let library = dxc_library()?;
        let create: Create = std::mem::transmute(
            GetProcAddress(library, s!("DxcCreateInstance"))
                .ok_or_else(windows::core::Error::from_thread)?,
        );
        let mut raw = std::ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw).ok()?;
        let compiler = IDxcCompiler3::from_raw(raw);
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let arguments = [w!("-T"), w!("cs_6_0"), w!("-E"), w!("main"), w!("-O3")];
        let result: IDxcResult = compiler.Compile(&buffer, Some(&arguments), None)?;
        if let Err(e) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!("{name}: DXC: {}", String::from_utf8_lossy(text).trim());
            }
            return Err(e);
        }
        let blob = result.GetResult()?;
        Ok(
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = r#"
        RWByteAddressBuffer b : register(u0);
        [RootSignature("UAV(u0)")]
        [numthreads(1, 1, 1)]
        void main() { b.Store(0, b.Load(0)); }
    "#;

    #[cfg(victory_bundled_hlsl)]
    #[test]
    fn bundled_helpers_have_shader_code_and_root_signatures() {
        use victory_shader::{Container, FourCC};

        for source in [
            include_str!("../shaders/argument_clamp.hlsl"),
            include_str!("../shaders/rect_rewrite.hlsl"),
            include_str!("../shaders/rect_expand.hlsl"),
            include_str!("../shaders/packed_copy.hlsl"),
            include_str!("../shaders/table_probe.hlsl"),
        ] {
            let expected = bundled_shader(source).unwrap();
            let bytes = compile_compute("bundled helper", source).unwrap();
            assert!(has_root_signature(&bytes));
            if !system_compile("bundled helper", source).is_ok_and(|code| has_root_signature(&code))
            {
                assert_eq!(bytes, expected);
            }
            let container = Container::parse(expected).unwrap();
            assert!(container.part(FourCC::DXIL).is_some());
            assert!(container.part(FourCC::RTS0).is_some());
        }
        assert!(bundled_shader(PROBE).is_none());
    }

    /// A raw-buffer compute shader compiles with one of the two compilers
    /// whenever either is present, and both results are DXBC containers.
    #[test]
    fn raw_buffer_shader_compiles_with_a_fallback() {
        let system = system_compile("probe", PROBE);
        let dxc = dxc_compile("probe", PROBE);
        for code in [system.as_ref().ok(), dxc.as_ref().ok()]
            .into_iter()
            .flatten()
        {
            assert_eq!(&code[..4], b"DXBC");
        }
        if system.is_ok_and(|code| has_root_signature(&code)) || dxc.is_ok() {
            assert!(has_root_signature(
                &compile_compute("probe", PROBE).unwrap()
            ));
        } else {
            assert!(compile_compute("probe", PROBE).is_err());
        }
    }
}
