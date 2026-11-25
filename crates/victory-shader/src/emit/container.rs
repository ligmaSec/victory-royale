//! The PC container around emitted DXIL: feature flags (`SFI0`), the
//! signatures (`ISG1`, `OSG1`), pipeline state validation (`PSV0`) and the
//! program (`DXIL`), laid out as validator versions 1.6 and 1.8 lay them out,
//! plus the container hash that D3D12 checks.

/// One signature element: one row (D3D12 links stages by semantic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SigElement {
    pub name: String,
    pub index: u32,
    /// DXIL semantic kind (0 arbitrary, 1 vertex id, 3 position, 16 target...).
    pub kind: u8,
    /// DXIL component type (4 i32, 5 u32, 9 f32, 1 i1).
    pub comp: u8,
    /// DXIL interpolation mode (0 for none).
    pub interp: u8,
    /// Start row, if the element has a register.
    pub row: Option<u32>,
    pub col: u8,
    pub cols: u8,
    /// Components read (inputs) or written (outputs), relative to `col`.
    pub usage: u8,
    /// 16-bit on the console (declared 32-bit here).
    pub half: bool,
}

impl SigElement {
    fn system_value(&self) -> Option<(u32, &'static str)> {
        match self.kind {
            1 => Some((6, "SV_VertexID")),
            2 => Some((8, "SV_InstanceID")),
            3 => Some((1, "SV_Position")),
            4 => Some((4, "SV_RenderTargetArrayIndex")),
            5 => Some((5, "SV_ViewportArrayIndex")),
            6 => Some((2, "SV_ClipDistance")),
            7 => Some((3, "SV_CullDistance")),
            10 => Some((7, "SV_PrimitiveID")),
            12 => Some((10, "SV_SampleIndex")),
            13 => Some((9, "SV_IsFrontFace")),
            14 => Some((66, "SV_Coverage")),
            15 => Some((70, "SV_InnerCoverage")),
            16 => Some((64, "SV_Target")),
            17 => Some((65, "SV_Depth")),
            18 => Some((68, "SV_DepthLessEqual")),
            19 => Some((67, "SV_DepthGreaterEqual")),
            20 => Some((69, "SV_StencilRef")),
            28 => Some((23, "SV_Barycentrics")),
            30 => Some((25, "SV_CullPrimitive")),
            _ => None,
        }
    }

    pub(super) fn semantic_name(&self) -> &str {
        self.system_value().map_or(&self.name, |(_, name)| name)
    }

    fn sig_comp(&self) -> u32 {
        match self.comp {
            1 | 5 => 1, // bool and u32
            4 => 2,
            9 => 3,
            _ => 0,
        }
    }

    fn mask(&self) -> u8 {
        ((1u8 << self.cols) - 1) << self.row.map_or(0, |_| self.col)
    }
}

/// The console packs elements of different interpolation modes into one
/// row (a constant front face beside a linear coordinate, say); DXIL needs
/// one mode per row. Each later mode of a row moves to a free row of its
/// own, shared by that row's elements of the mode. Code addresses elements
/// by id and stages link by semantic, so only the signatures change.
/// Elements without a mode (0) are left where they are.
pub(crate) fn separate_interpolation(elements: &mut [SigElement]) {
    let mut next = elements
        .iter()
        .filter_map(|e| e.row)
        .max()
        .map_or(0, |r| r + 1);
    // (row, the row's first mode)
    let mut first: Vec<(u32, u8)> = Vec::new();
    // ((old row, mode), new row)
    let mut moved: Vec<((u32, u8), u32)> = Vec::new();
    for e in elements.iter_mut() {
        let (Some(row), mode) = (e.row, e.interp) else {
            continue;
        };
        if mode == 0 {
            continue;
        }
        match first.iter().find(|f| f.0 == row) {
            None => first.push((row, mode)),
            Some(&(_, m)) if m == mode => {}
            Some(_) => {
                let to = match moved.iter().find(|m| m.0 == (row, mode)) {
                    Some(&(_, to)) => to,
                    None => {
                        moved.push(((row, mode), next));
                        next += 1;
                        next - 1
                    }
                };
                e.row = Some(to);
            }
        }
    }
}

/// Input, output and primitive signatures follow the resource and string tables
/// in PSV version 1 and later. Select the producer's per-vertex output records.
fn producer_records(mut data: &[u8], stage: u8) -> Option<std::slice::ChunksExact<'_, u8>> {
    fn take<'a>(data: &mut &'a [u8], bytes: usize) -> Option<&'a [u8]> {
        let (head, rest) = data.split_at_checked(bytes)?;
        *data = rest;
        Some(head)
    }
    fn word(data: &mut &[u8]) -> Option<usize> {
        Some(u32::from_le_bytes(take(data, 4)?.try_into().ok()?) as usize)
    }
    let info_size = word(&mut data)?;
    let info = take(&mut data, info_size)?;
    if *info.get(24)? != stage {
        return None;
    }
    let inputs = usize::from(*info.get(28)?);
    let outputs = usize::from(*info.get(29)?);
    let resources = word(&mut data)?;
    if resources > 0 {
        let stride = word(&mut data)?;
        take(&mut data, resources.checked_mul(stride)?)?;
    }
    let names = word(&mut data)?;
    take(&mut data, names)?;
    let indices = word(&mut data)?;
    take(&mut data, indices.checked_mul(4)?)?;
    let stride = word(&mut data)?;
    if stride < 16 {
        return None;
    }
    take(&mut data, inputs.checked_mul(stride)?)?;
    Some(take(&mut data, outputs.checked_mul(stride)?)?.chunks_exact(stride))
}

/// Preserve declared interpolation for user semantics, including flat floats.
/// Signature chunks sort by register, while PSV records follow element order.
pub(super) fn producer_interpolation(data: &[u8], stage: u8, outputs: &mut [SigElement]) {
    let Some(records) = producer_records(data, stage) else {
        return;
    };
    for record in records {
        let (rows, start, packing) = (record[8], record[9], record[10]);
        let (kind, comp, mode) = (record[11], record[12], record[13]);
        if kind != 0 || packing & 0x40 == 0 || !(1..=7).contains(&mode) {
            continue;
        }
        for e in outputs.iter_mut().filter(|e| {
            e.kind == 0
                && e.sig_comp() == u32::from(comp)
                && e.col == (packing >> 4) & 3
                && e.cols == packing & 15
                && e.row.is_some_and(|row| {
                    (u32::from(start)..u32::from(start) + u32::from(rows)).contains(&row)
                })
        }) {
            e.interp = mode;
        }
    }
}

/// `ISG1`/`OSG1`: 32-byte rows, then the names.
pub(crate) fn signature(elements: &[SigElement], input: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend((elements.len() as u32).to_le_bytes());
    out.extend(8u32.to_le_bytes());
    // Validator 1.6 writes one name per element in element order, repeats
    // included, except that system values share theirs (it keys names by
    // the semantic's string, which is one static string per system value);
    // the rows stably sorted by register (unallocated last). Elements
    // sharing a register keep their metadata order, regardless of column.
    let mut offsets: Vec<u32> = Vec::new();
    let mut next = 8 + 32 * elements.len() as u32;
    let mut names = Vec::new();
    for (i, e) in elements.iter().enumerate() {
        let shared = (e.kind != 0)
            .then(|| (0..i).find(|&j| elements[j].kind == e.kind))
            .flatten();
        match shared {
            Some(j) => offsets.push(offsets[j]),
            None => {
                let name = e.semantic_name();
                offsets.push(next);
                next += name.len() as u32 + 1;
                names.push(name);
            }
        }
    }
    let mut order: Vec<usize> = (0..elements.len()).collect();
    order.sort_by_key(|&i| elements[i].row.unwrap_or(u32::MAX));
    for i in order {
        let (e, offset) = (&elements[i], offsets[i]);
        let used = e.usage << e.row.map_or(0, |_| e.col) & e.mask();
        let rw = if input { used } else { !used & 0xf };
        for v in [
            0,
            offset,
            e.index,
            e.system_value().map_or(0, |(value, _)| value),
            e.sig_comp(),
            e.row.unwrap_or(u32::MAX),
        ] {
            out.extend(v.to_le_bytes());
        }
        out.extend([e.mask(), rw, 0, 0]);
        out.extend(0u32.to_le_bytes());
    }
    for name in names {
        out.extend(name.as_bytes());
        out.push(0);
    }
    out
}

/// A resource bound by register, as `PSV0` lists it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding {
    /// PSV resource type: 1 sampler, 2 CBV.
    pub kind: u32,
    pub space: u32,
    pub register: u32,
    /// DXIL resource kind (13 cbuffer, 14 sampler).
    pub dxil_kind: u32,
}

pub(crate) struct Psv<'a> {
    /// PSV shader kind: 0 pixel, 1 vertex, 5 compute.
    pub stage: u8,
    /// Vertex: writes `SV_Position`. Pixel: writes depth.
    pub flag: bool,
    pub inputs: &'a [SigElement],
    pub outputs: &'a [SigElement],
    /// Mesh: per-primitive outputs.
    pub prims: &'a [SigElement],
    /// Mesh: its state and the group-shared bytes it declares.
    pub mesh: Option<(super::shader::Mesh, u32)>,
    /// Geometry (triangles in, triangle strips out on stream 0): its
    /// maximum vertex count. `flag`: writes `SV_Position`.
    pub geometry: Option<u32>,
    pub bindings: &'a [Binding],
    /// Compute: thread-group size and required wave size.
    pub threads: [u32; 3],
    pub wave_size: Option<u32>,
    /// Validator 1.8: runtime info 3 and the entry-point name.
    pub validator18: bool,
}

/// `PSV0` with runtime info version 2, or 3 for validator 1.8.
pub(crate) fn psv(p: &Psv) -> Vec<u8> {
    let vectors = |els: &[SigElement]| {
        els.iter()
            .filter_map(|e| e.row)
            .map(|r| r + 1)
            .max()
            .unwrap_or(0)
    };
    let (in_vectors, out_vectors) = (vectors(p.inputs), vectors(p.outputs));
    let mut out = Vec::new();
    let u32s = |out: &mut Vec<u8>, vs: &[u32]| {
        for v in vs {
            out.extend(v.to_le_bytes());
        }
    };
    let info_size = if p.validator18 { 52 } else { 48 };
    u32s(&mut out, &[info_size]);
    let mut info = [0u8; 52];
    info[0] = u8::from(p.flag);
    if p.stage == 0 {
        // PSInfo.SampleFrequency: an input interpolated per sample, or the
        // sample index.
        info[1] = u8::from(
            p.inputs
                .iter()
                .any(|e| matches!(e.interp, 6 | 7) || e.kind == 12),
        );
    }
    let (min, max) = p.wave_size.map_or((0, u32::MAX), |n| (n, n));
    info[16..20].copy_from_slice(&min.to_le_bytes());
    info[20..24].copy_from_slice(&max.to_le_bytes());
    info[24] = p.stage;
    for (k, n) in p.threads.into_iter().enumerate() {
        info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
    }
    info[28] = p.inputs.len() as u8;
    info[29] = p.outputs.len() as u8;
    info[31] = in_vectors as u8;
    info[32] = out_vectors as u8;
    if let Some((mesh, shared)) = p.mesh {
        info[0..4].copy_from_slice(&shared.to_le_bytes());
        info[12..14].copy_from_slice(&(mesh.max_vertices as u16).to_le_bytes());
        info[14..16].copy_from_slice(&(mesh.max_primitives as u16).to_le_bytes());
        info[26] = vectors(p.prims) as u8;
        info[27] = mesh.topology;
        info[30] = p.prims.len() as u8;
    }
    if let Some(max_vertices) = p.geometry {
        // Input primitive (3: triangles), output topology (5: triangle
        // strips), stream mask, position written; the vertex maximum.
        info[0..4].copy_from_slice(&3u32.to_le_bytes());
        info[4..8].copy_from_slice(&5u32.to_le_bytes());
        info[8..12].copy_from_slice(&1u32.to_le_bytes());
        info[12] = u8::from(p.flag);
        info[26..28].copy_from_slice(&(max_vertices as u16).to_le_bytes());
    }
    if p.validator18 {
        // Runtime info 3 names the entry point at offset 1 in the strings.
        info[48..52].copy_from_slice(&1u32.to_le_bytes());
    }
    out.extend(&info[..info_size as usize]);
    u32s(&mut out, &[p.bindings.len() as u32]);
    if !p.bindings.is_empty() {
        u32s(&mut out, &[24]);
        for b in p.bindings {
            u32s(
                &mut out,
                &[b.kind, b.space, b.register, b.register, b.dxil_kind, 0],
            );
        }
    }
    // The optional entry point and arbitrary semantics (system values use ""),
    // then indices.
    let mut strings = if p.validator18 {
        b"\0main\0".to_vec()
    } else {
        vec![0]
    };
    let mut indices: Vec<u32> = Vec::new();
    let mut elements = Vec::new();
    for e in p.inputs.iter().chain(p.outputs).chain(p.prims) {
        let name = if e.kind == 0 {
            let needle: Vec<u8> = e.name.bytes().chain([0]).collect();
            match find(&strings, &needle) {
                Some(at) => at,
                None => {
                    strings.extend(&needle);
                    strings.len() - needle.len()
                }
            }
        } else {
            0
        };
        let index = match indices.iter().position(|&i| i == e.index) {
            Some(at) => at,
            None => {
                indices.push(e.index);
                indices.len() - 1
            }
        };
        let (start, alloc) = match e.row {
            Some(r) => (r as u8, 0x40 | (e.col << 4)),
            None => (0, 0),
        };
        elements.push((name as u32, index as u32, start, e.cols | alloc, e));
    }
    strings.resize(strings.len().next_multiple_of(4), 0);
    u32s(&mut out, &[strings.len() as u32]);
    out.extend(&strings);
    u32s(&mut out, &[indices.len() as u32]);
    u32s(&mut out, &indices);
    if !elements.is_empty() {
        u32s(&mut out, &[16]);
        for (name, index, start, cols, e) in elements {
            u32s(&mut out, &[name, index]);
            out.extend([1, start, cols, e.kind, e.sig_comp() as u8, e.interp, 0, 0]);
        }
    }
    // Input-to-output dependencies, as the view-id state declares them:
    // every output scalar on every input scalar.
    if in_vectors > 0 && out_vectors > 0 {
        let scalars = |els: &[SigElement]| {
            els.iter()
                .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                .max()
                .unwrap_or(0)
        };
        let (ni, no) = (scalars(p.inputs), scalars(p.outputs));
        let per = (out_vectors * 4).div_ceil(32);
        for i in 0..in_vectors * 4 {
            for k in 0..per {
                let bits = if i < ni {
                    no.saturating_sub(32 * k).min(32)
                } else {
                    0
                };
                let mask = if bits == 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                };
                u32s(&mut out, &[mask]);
            }
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The container: parts in order, hashed.
pub(crate) fn build(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let table = 0x20 + 4 * parts.len();
    let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(b"DXBC");
    out.extend([0; 16]);
    out.extend(1u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend((parts.len() as u32).to_le_bytes());
    let mut offset = table;
    for (_, d) in parts {
        out.extend((offset as u32).to_le_bytes());
        offset += 8 + d.len();
    }
    for (fourcc, d) in parts {
        out.extend(*fourcc);
        out.extend((d.len() as u32).to_le_bytes());
        out.extend(*d);
    }
    let digest = hash(&out[20..]);
    out[4..20].copy_from_slice(&digest);
    out
}

/// The container hash D3D12 checks: MD5 whose last block carries the bit
/// count first and `2 × bytes + 1` last instead of standard padding.
pub(crate) fn hash(data: &[u8]) -> [u8; 16] {
    let len = data.len();
    let left = len % 64;
    let mut tail = Vec::with_capacity(128);
    if left < 56 {
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(60, 0);
    } else {
        tail.extend(&data[len - left..]);
        tail.push(0x80);
        tail.resize(64, 0);
        tail.extend(((len as u32) << 3).to_le_bytes());
        tail.resize(124, 0);
    }
    tail.extend(((len as u32) << 1 | 1).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data[..len - left]
        .as_chunks::<64>()
        .0
        .iter()
        .chain(tail.as_chunks::<64>().0)
    {
        md5_block(&mut state, block);
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

fn md5_block(state: &mut [u32; 4], block: &[u8; 64]) {
    const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let x: Vec<u32> = block
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32;
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(x[g])
            .rotate_left(S[(i / 16) * 4 + i % 4]);
        (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_float_retains_its_packed_producer_row() {
        let flat = SigElement {
            name: "VALUE".into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 1,
            row: Some(2),
            col: 3,
            cols: 1,
            usage: 1,
            half: false,
        };
        let integer = SigElement {
            col: 0,
            cols: 3,
            comp: 5,
            index: 1,
            ..flat.clone()
        };
        let linear = SigElement {
            row: Some(4),
            interp: 2,
            ..flat.clone()
        };
        let binding = Binding {
            kind: 2,
            space: 0,
            register: 0,
            dxil_kind: 13,
        };
        for stage in [1, 13] {
            let meta = psv(&Psv {
                stage,
                flag: false,
                inputs: std::slice::from_ref(&linear),
                outputs: &[integer.clone(), linear.clone(), flat.clone()],
                prims: &[],
                mesh: None,
                geometry: None,
                bindings: std::slice::from_ref(&binding),
                threads: [1; 3],
                wave_size: None,
                validator18: true,
            });
            let mut outputs = [
                SigElement {
                    interp: 2,
                    ..flat.clone()
                },
                integer.clone(),
                linear.clone(),
            ];
            producer_interpolation(&meta, 5, &mut outputs);
            assert_eq!(outputs[0].interp, 2);
            producer_interpolation(&meta[..meta.len() / 2], stage, &mut outputs);
            assert_eq!(outputs[0].interp, 2);
            producer_interpolation(&meta, stage, &mut outputs);
            assert_eq!(outputs, [flat.clone(), integer.clone(), linear.clone()]);
        }
    }

    #[test]
    fn md5_core_matches_the_standard() {
        // Standard MD5 of "abc" with standard padding, through md5_block.
        let mut block = b"abc".to_vec();
        block.push(0x80);
        block.resize(56, 0);
        block.extend(24u64.to_le_bytes());
        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        md5_block(&mut state, block.as_slice().try_into().unwrap());
        let hex: String = state
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
    }

    fn element(name: &str, row: u32, col: u8, interp: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp,
            row: Some(row),
            col,
            cols: 1,
            usage: 1,
            half: false,
        }
    }

    #[test]
    fn rows_of_mixed_interpolation_are_separated() {
        let mut e = vec![
            element("A", 0, 0, 2),
            element("B", 0, 1, 1),
            element("C", 1, 0, 2),
            element("D", 0, 2, 1),
            element("E", 0, 3, 2),
            element("F", 1, 1, 0),
        ];
        separate_interpolation(&mut e);
        let rows: Vec<u32> = e.iter().map(|e| e.row.unwrap()).collect();
        // B and D (constant) share a new row; A and E stay; F has no mode.
        assert_eq!(rows, [0, 2, 1, 2, 0, 1]);
    }
}
