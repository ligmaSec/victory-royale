//! Synthetic pixel programs with direct or interpolated barycentric inputs.

use super::container::SigElement;
use super::*;
use crate::native::{PsInput, StageIo, Usage};

/// `SPI_PS_INPUT_ADDR` bits: PERSP_CENTER, PERSP_CENTROID,
/// LINEAR_CENTER.
const PERSP_CENTER: u32 = 1 << 1;
const PERSP_CENTROID: u32 = 1 << 2;
const LINEAR_CENTER: u32 = 1 << 5;

/// `exp mrt0, v<r>, v<r>, v<r>, v<r> done vm`.
fn exp_mrt0(r: u32) -> [u32; 2] {
    [0xF800_180F, r * 0x0101_0101]
}

/// VOP2 `v_add_f32 v<dst>, v<a>, v<b>`.
fn v_add_f32(dst: u32, a: u32, b: u32) -> u32 {
    3 << 25 | dst << 17 | b << 9 | (256 + a)
}

fn element(name: &str, kind: u8, row: u32) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    }
}

/// Translates a pixel program with the input VGPRs `input_addr`
/// enables and one float4 TEXCOORD0 attribute (attr 0, row 0).
fn pixel(words: &[u32], input_addr: u32) -> Result<HostShader, ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 0,
                mask: 0b1111,
                attr: 0,
                flat: false,
                fp16: false,
                pack2: false,
                row2: 0,
            }],
            input_addr,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let mut target = element("SV_Target", 16, 0);
    target.usage = 15;
    let isg1 = container::signature(&[element("TEXCOORD", 0, 0)], true);
    let osg1 = container::signature(&[target], false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    emit(&c, &native, &p, &st, &io, &res)
}

/// The translation's `SFI0` features and `ISG1` rows.
fn parts(shader: &HostShader) -> (u64, Vec<SigElement>) {
    let c = Container::parse(&shader.dxil).unwrap();
    let sfi0 = c.part(FourCC::SFI0).unwrap().data.try_into().unwrap();
    (
        u64::from_le_bytes(sfi0),
        signature(c.part(FourCC::ISG1).unwrap().data).unwrap(),
    )
}

fn validate(shader: &HostShader) {
    #[cfg(windows)]
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone()).unwrap();
    }
    let _ = shader;
}

/// Custom interpolation reads I and J (v0, v1 with PERSP_CENTER) as
/// plain floats: they are channels y and z of `SV_Barycentrics`, the
/// weights of vertices 1 and 2.
#[test]
fn barycentrics_read_as_values_are_sv_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [v_add_f32(2, 0, 1), e0, e1, 0xBF81_0000];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0, "no Barycentrics feature");
    let bary: Vec<_> = inputs.iter().filter(|e| e.kind == 28).collect();
    assert_eq!(bary.len(), 1, "{inputs:?}");
    assert_eq!((bary[0].index, bary[0].col, bary[0].cols), (0, 0, 3));
    // Not packed: the validator wants packing location -1.
    assert_eq!(bary[0].row, None);
    // Only y and z (I and J) are read.
    let isg1 = Container::parse(&shader.dxil).unwrap();
    let isg1 = isg1.part(FourCC::ISG1).unwrap().data;
    let bary_row = inputs.iter().position(|e| e.kind == 28).unwrap();
    assert_eq!(isg1[8 + 32 * bary_row + 25], 0b110);
    validate(&shader);
}

/// Two kinds of barycentrics read directly: one element per mode.
#[test]
fn two_barycentric_modes_get_two_elements() {
    // v0-v1 perspective i, j; v2-v3 linear i, j.
    let [e0, e1] = exp_mrt0(4);
    let words = [v_add_f32(4, 0, 3), e0, e1, 0xBF81_0000];
    let shader = pixel(&words, PERSP_CENTER | LINEAR_CENTER).unwrap();
    let (_, inputs) = parts(&shader);
    let mut indices: Vec<u32> = inputs
        .iter()
        .filter(|e| e.kind == 28)
        .map(|e| e.index)
        .collect();
    indices.sort();
    assert_eq!(indices, [0, 1], "{inputs:?}");
    validate(&shader);
}

/// Center and centroid perspective I/J both read as values: DXIL
/// allows one perspective `SV_Barycentrics` element (the validator:
/// "There can only be up to two input attributes of SV_Barycentrics
/// with different perspective interpolation mode"), so the program is
/// refused rather than given two, or center weights for centroid ones.
#[test]
fn center_and_centroid_barycentrics_are_refused() {
    // v0-v1 perspective center i, j; v2-v3 perspective centroid i, j.
    let [e0, e1] = exp_mrt0(4);
    let words = [v_add_f32(4, 0, 3), e0, e1, 0xBF81_0000];
    match pixel(&words, PERSP_CENTER | PERSP_CENTROID) {
        Err(ShaderError::Unsupported { what, .. }) => {
            assert!(what.contains("two perspective modes"), "{what}")
        }
        Err(e) => panic!("{e:?}"),
        Ok(shader) => panic!("translated: {:?}", parts(&shader).1),
    }
    // Centroid alone (v2, v3) still translates, with one element.
    let words = [v_add_f32(4, 2, 3), e0, e1, 0xBF81_0000];
    let shader = pixel(&words, PERSP_CENTER | PERSP_CENTROID).unwrap();
    let (_, inputs) = parts(&shader);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    validate(&shader);
}

#[test]
fn dead_barycentric_arithmetic_keeps_the_old_shader_requirements() {
    let [exp, sources] = exp_mrt0(2);
    let tail = [0x7E04_02F2, exp, sources, 0xBF81_0000]; // v2 = 1.0, export v2
    let mut words = vec![v_add_f32(3, 0, 1)]; // dead v3 = I + J
    words.extend(tail);
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_eq!(features & 0x2_0000, 0);
    assert!(inputs.iter().all(|e| e.kind != 28));
}

/// I and J only fed to `v_interp_*` stay host interpolation: no
/// barycentrics are declared.
#[test]
fn interpolation_alone_declares_no_barycentrics() {
    let [e0, e1] = exp_mrt0(2);
    let words = [
        0xC808_0000, // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001, // v_interp_p2_f32 v2, v1, attr0.x
        e0,
        e1,
        0xBF81_0000,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_eq!(features & 0x2_0000, 0);
    assert!(inputs.iter().all(|e| e.kind != 28), "{inputs:?}");
    validate(&shader);
}

/// Both: the attribute is interpolated and J is used as a value.
#[test]
fn interpolation_and_a_barycentric_value_together() {
    let [e0, e1] = exp_mrt0(3);
    let words = [
        0xC808_0000,        // v_interp_p1_f32 v2, v0, attr0.x
        0xC809_0001,        // v_interp_p2_f32 v2, v1, attr0.x
        v_add_f32(3, 2, 1), // v3 = attribute + j
        e0,
        e1,
        0xBF81_0000,
    ];
    let shader = pixel(&words, PERSP_CENTER).unwrap();
    let (features, inputs) = parts(&shader);
    assert_ne!(features & 0x2_0000, 0);
    assert_eq!(inputs.iter().filter(|e| e.kind == 28).count(), 1);
    validate(&shader);
}

/// The weights of vertices 0, 1 and 2 to target 0: 1 - i - j, i, j (the
/// pair is in v0 and v1), and 1.
#[cfg(windows)]
const WEIGHTS: [u32; 6] = [
    0x0804_00F2, // v_sub_f32 v2, 1.0, v0
    0x0804_0302, // v_sub_f32 v2, v2, v1
    0x7E06_02F2, // v_mov_b32 v3, 1.0
    0xF800_180F,
    0x0301_0002, // exp mrt0 v2, v0, v1, v3 done vm
    0xBF81_0000, // s_endpgm
];

#[cfg(windows)]
fn barycentrics_supported(host: &super::host::Host) -> bool {
    use windows::Win32::Graphics::Direct3D12::*;
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
    // SAFETY: a feature query with its own out structure.
    let ok = unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS3,
            (&raw mut o).cast(),
            size_of_val(&o) as u32,
        )
    };
    ok.is_ok() && o.BarycentricsSupported.as_bool()
}

/// A triangle whose vertices have w 1, 2 and 4, drawn with the weights
/// program: every pixel well inside holds the weights D3D gives vertices
/// 0, 1 and 2 there, perspective-correct for the perspective pair and
/// screen-linear for the linear one (they differ by up to about 0.3).
#[test]
#[cfg(windows)]
fn drawn_weights_are_the_hosts_vertex_weights() {
    use super::host::{Draw, Host, Root};
    let ndc = [[-0.8f32, -0.7], [0.9, -0.6], [-0.5, 0.85]];
    let w = [1.0f32, 2.0, 4.0];
    let vertices: Vec<f32> = (0..3)
        .flat_map(|k| [ndc[k][0] * w[k], ndc[k][1] * w[k], 0.5 * w[k], w[k]])
        .collect();
    let (width, height) = (32u32, 32u32);
    // Screen-linear and perspective-correct weights at a pixel's centre.
    let weights = |px: u32, py: u32| {
        let p = [
            (px as f32 + 0.5) / width as f32 * 2.0 - 1.0,
            1.0 - (py as f32 + 0.5) / height as f32 * 2.0,
        ];
        let cross = |a: [f32; 2], b: [f32; 2]| a[0] * b[1] - a[1] * b[0];
        let sub = |a: [f32; 2], b: [f32; 2]| [a[0] - b[0], a[1] - b[1]];
        let (e1, e2, d) = (sub(ndc[1], ndc[0]), sub(ndc[2], ndc[0]), sub(p, ndc[0]));
        let (l1, l2) = (cross(d, e2) / cross(e1, e2), cross(e1, d) / cross(e1, e2));
        let linear = [1.0 - l1 - l2, l1, l2];
        let q = [linear[0] / w[0], linear[1] / w[1], linear[2] / w[2]];
        let s = q[0] + q[1] + q[2];
        (linear, [q[0] / s, q[1] / s, q[2] / s])
    };
    let vs = super::host::position_vs();
    validate(&vs);
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        if !barycentrics_supported(&host) {
            eprintln!("{label}: no barycentrics support; skipped");
            continue;
        }
        for (addr, linear) in [(PERSP_CENTER, false), (LINEAR_CENTER, true)] {
            let ps = pixel(&WEIGHTS, addr).unwrap();
            let (root, pso) = host
                .pipeline(&vs, &ps)
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let draw = Draw {
                roots: vec![(0, Root::Constants(vec![0])), (1, Root::Constants(vec![0]))],
                textures: &[],
                samplers: &[],
                vertices: &vertices,
                stride: 16,
                size: (width, height),
                mesh_groups: 0,
            };
            let texels = host
                .draw(&root, &pso, &draw)
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let (mut inside, mut spread) = (0, 0.0f32);
            for py in 0..height {
                for px in 0..width {
                    let (lin, persp) = weights(px, py);
                    // Near or past an edge: coverage is the rasteriser's.
                    if lin.iter().any(|&l| l < 0.02) {
                        continue;
                    }
                    inside += 1;
                    spread = (0..3).fold(spread, |m, c| m.max((lin[c] - persp[c]).abs()));
                    let want = if linear { lin } else { persp };
                    let t = texels[(py * width + px) as usize];
                    let ok = (0..3).all(|c| (t[c] - want[c]).abs() < 2e-3) && t[3] == 1.0;
                    assert!(ok, "{label} {addr:#x} ({px}, {py}): {t:?}, want {want:?}");
                }
            }
            assert!(inside > 100 && spread > 0.2, "{inside} pixels, {spread}");
            eprintln!("{label} {addr:#x}: {inside} pixels match");
        }
        ran += 1;
    }
    if ran == 0 {
        eprintln!("no device with barycentrics; nothing drawn");
    }
}
