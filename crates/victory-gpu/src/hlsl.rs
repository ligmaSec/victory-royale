//! Compile host-owned shaders with the system compiler where possible,
//! then use the package's matching embedded helper or runtime DXC.
//! Some Wine compilers omit the root signature even on success; where the
//! helper needs it embedded, that output is not usable.

use std::{borrow::Cow, ffi::CStr};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::core::{PCSTR, s};

#[cfg(victory_bundled_hlsl)]
include!(concat!(env!("OUT_DIR"), "/bundled_hlsl.rs"));

pub(crate) fn compile_compute(
    name: &str,
    source: &str,
) -> windows::core::Result<Cow<'static, [u8]>> {
    compile_stage(name, source, c"main", c"cs_5_1", true)
}

pub(crate) fn compile_stage(
    name: &str,
    source: &str,
    entry: &CStr,
    profile: &CStr,
    root: bool,
) -> windows::core::Result<Cow<'static, [u8]>> {
    #[cfg(victory_bundled_hlsl)]
    let bundled = bundled_shader(source, entry.to_bytes(), profile.to_bytes(), 0);
    #[cfg(not(victory_bundled_hlsl))]
    let bundled = None;
    compile_runtime(name, source, entry, profile, root, bundled)
}

pub(crate) fn compile_ambient(
    define: &str,
    entry: &CStr,
) -> windows::core::Result<Cow<'static, [u8]>> {
    const SOURCE: &str = include_str!("../shaders/ambient.hlsl");
    #[cfg(victory_bundled_hlsl)]
    let bundled = bundled_shader(SOURCE, entry.to_bytes(), b"cs_5_1", 0);
    #[cfg(not(victory_bundled_hlsl))]
    let bundled = None;
    compile_runtime(
        "indirect light stand-in",
        &format!("#define {define}\n{SOURCE}"),
        entry,
        c"cs_5_1",
        true,
        bundled,
    )
}

pub(crate) fn compile_clamp(
    source: &str,
    width: u32,
    rest: u32,
) -> windows::core::Result<Cow<'static, [u8]>> {
    #[cfg(victory_bundled_hlsl)]
    let bundled = match (width, rest) {
        (65_535, 0) => Some(1),
        (65_535, 1) => Some(2),
        (1_048_576, 0) => Some(3),
        (1_048_576, 1) => Some(4),
        _ => None,
    }
    .and_then(|variant| bundled_shader(source, b"main", b"cs_5_1", variant));
    #[cfg(not(victory_bundled_hlsl))]
    let bundled = None;
    let source = format!("#define WIDEST {width}u\n#define REST {rest}\n{source}");
    // The list supplies the clamp's root signature.
    compile_runtime(
        "argument clamp",
        &source,
        c"main",
        c"cs_5_1",
        false,
        bundled,
    )
}

fn has_root_signature(code: &[u8]) -> bool {
    use victory_shader::{Container, FourCC};
    Container::parse(code).is_ok_and(|c| c.part(FourCC::RTS0).is_some())
}

fn compile_runtime(
    name: &str,
    source: &str,
    entry: &CStr,
    profile: &CStr,
    root: bool,
    bundled: Option<&'static [u8]>,
) -> windows::core::Result<Cow<'static, [u8]>> {
    // Preserve the donor's native compiler output where it is usable.
    match system_compile(name, source, entry, profile) {
        Ok(code) if !root || has_root_signature(&code) => return Ok(Cow::Owned(code)),
        Ok(_) => tracing::debug!(helper = name, "system compiler omitted the root signature"),
        Err((e, text)) => {
            tracing::debug!(error = %e, helper = name, "system compiler refused it ({})", text.trim());
        }
    }
    if let Some(code) = bundled {
        return Ok(Cow::Borrowed(code));
    }
    let code = dxc_compile(name, source, entry, profile)
        .inspect_err(|e| tracing::warn!(error = %e, "{name}: DXC failed"))?;
    if root && !has_root_signature(&code) {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ));
    }
    Ok(Cow::Owned(code))
}

fn system_compile(
    name: &str,
    source: &str,
    entry: &CStr,
    profile: &CStr,
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
            PCSTR(entry.as_ptr().cast()),
            PCSTR(profile.as_ptr().cast()),
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

fn dxc_compile(
    name: &str,
    source: &str,
    entry: &CStr,
    profile: &CStr,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::{HSTRING, Interface, PCWSTR, w};
    let target = match profile.to_bytes() {
        b"cs_5_1" => w!("cs_6_0"),
        b"vs_5_1" => w!("vs_6_0"),
        b"ps_5_1" => w!("ps_6_0"),
        _ => {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_INVALIDARG,
            ));
        }
    };
    let entry = HSTRING::from(entry.to_string_lossy().as_ref());
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
        let arguments = [
            w!("-T"),
            target,
            w!("-E"),
            PCWSTR(entry.as_ptr()),
            w!("-O3"),
        ];
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
    fn bundled_helpers_have_shader_code_and_required_root_signatures() {
        use victory_shader::{Container, FourCC};
        for &(id, bytes, needs_root) in BUNDLED_HELPERS {
            let container = Container::parse(bytes).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(container.part(FourCC::DXIL).is_some(), "{id}");
            assert!(!needs_root || has_root_signature(bytes), "{id}");
        }
        assert!(bundled_shader(PROBE, b"main", b"cs_5_1", 0).is_none());
    }

    /// A raw-buffer compute shader compiles with one of the two compilers
    /// whenever either is present, and both results are DXBC containers.
    #[test]
    fn raw_buffer_shader_compiles_with_a_fallback() {
        let system = system_compile("probe", PROBE, c"main", c"cs_5_1");
        let dxc = dxc_compile("probe", PROBE, c"main", c"cs_5_1");
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
