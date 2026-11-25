//! A DXIL vertex, pixel, compute or mesh shader under construction: the module,
//! the `dx.op` declarations it calls, its signatures and bound resources,
//! and the metadata and container that wrap it up (SM 6.6; validator 1.6,
//! or 1.8 for compute derivatives).

use super::bitcode::{Attr, Md, Module, Ty, V};
use super::container::{self, Binding, Psv, SigElement};

/// Where the shader reads its user data: `b0` in this space.
pub const USER_DATA_SPACE: u32 = 0;
/// Where immediate samplers are bound: `s<n>` in this space.
pub const SAMPLER_SPACE: u32 = 0;
/// Where root CBVs and SRVs are bound: `b<root parameter index>` and
/// `t<root parameter index>` in this space.
pub const ROOT_CBV_SPACE: u32 = 1;

/// `ShaderFlags` bits (metadata) and their `SFI0` feature bits.
pub(crate) mod flags {
    pub const RAW_BUFFERS: u64 = 1 << 4;
    pub const VIEWPORT_RT_INDEX: u64 = 1 << 9;
    pub const STENCIL_REF: u64 = 1 << 11;
    pub const TYPED_UAV_LOADS: u64 = 1 << 13;
    pub const WAVE_OPS: u64 = 1 << 19;
    pub const INT64: u64 = 1 << 20;
    pub const BARYCENTRICS: u64 = 1 << 22;
    pub const ATOMIC_TYPED64: u64 = 1 << 27;
    pub const ATOMIC_HEAP64: u64 = 1 << 32;
    pub const RESOURCE_HEAP: u64 = 1 << 30;
    pub const SAMPLER_HEAP: u64 = 1 << 31;
}

pub(crate) struct Types {
    pub void: Ty,
    pub i1: Ty,
    pub i32: Ty,
    pub f32: Ty,
    pub handle: Ty,
    pub props: Ty,
    pub bind: Ty,
    pub cbuf_ret: Ty,
    pub res_ret: Ty,
}

/// What a mesh shader declares beyond its thread-group size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mesh {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
}

impl Mesh {
    pub(super) const MAX_OUTPUT_BYTES: u64 = 32_768;

    /// DXIL charges for declared components and rounds each output count
    /// up to a multiple of 32 independently of the signature packing.
    pub(super) fn output_bytes(self, vertices: &[SigElement], primitives: &[SigElement]) -> u64 {
        let bytes = |count: u32, elements: &[SigElement]| {
            let components: u64 = elements.iter().map(|e| u64::from(e.cols)).sum();
            u64::from(count).next_multiple_of(32) * 4 * components
        };
        bytes(self.max_vertices, vertices) + bytes(self.max_primitives, primitives)
    }
}

/// What a geometry shader declares: triangles in, triangle strips out on
/// stream 0, one instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub max_vertices: u32,
}

pub(crate) struct Shader {
    pub m: Module,
    /// Test builds: typed loads assembled from a dword-aligned pair.
    #[cfg(test)]
    pub unaligned_typed_loads: u32,
    /// Test builds: constant byte offsets of `s_buffer_load_dword*`.
    #[cfg(test)]
    pub smem_buffer_bytes: Vec<u32>,
    /// Test builds: indices into the address list of image instructions that
    /// were read.
    #[cfg(test)]
    pub image_address_reads: Vec<usize>,
    pub t: Types,
    pub pixel: bool,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// A mesh shader's per-primitive outputs.
    pub prims: Vec<SigElement>,
    /// A mesh shader (`threads` is its thread-group size).
    pub mesh: Option<Mesh>,
    pub geometry: Option<Geometry>,
    /// Dwords of user data the shader reads (0: no user-data buffer).
    pub user_data: u32,
    /// Immediate samplers, bound as `s0..` in [`SAMPLER_SPACE`].
    pub samplers: u32,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    /// Root CBVs read, by root parameter index, with their handles.
    pub root_cbvs: Vec<(u8, V)>,
    /// Root SRVs (raw buffers) read, likewise.
    pub root_srvs: Vec<(u8, V)>,
    /// Root UAVs (raw buffers) read and written, likewise.
    pub root_uavs: Vec<(u8, V)>,
    pub flags: u64,
    /// Heap handles made with a non-uniform index.
    pub nonuniform_handles: u32,
    /// Heap handles annotated as constant buffers.
    pub cbuffer_handles: u32,
    /// A compute shader with this thread-group size.
    pub threads: Option<[u32; 3]>,
    /// The wave size a compute shader requires (`WaveSize`).
    pub wave_size: Option<u32>,
    /// Compute derivatives require validator 1.8 rather than 1.6 or 1.7.
    compute_derivatives: bool,
    /// Thread-group shared memory, in dwords.
    pub lds_dwords: u32,
    lds: Option<V>,
    main: V,
    pub entry: usize,
}

impl Shader {
    pub fn new(pixel: bool) -> Self {
        let mut m = Module::default();
        let void = m.void();
        let i1 = m.int(1);
        let i8 = m.int(8);
        let i32 = m.int(32);
        let f32 = m.float();
        let i8p = m.ptr(i8);
        let handle = m.named_struct("dx.types.Handle", vec![i8p]);
        let props = m.named_struct("dx.types.ResourceProperties", vec![i32, i32]);
        let bind = m.named_struct("dx.types.ResBind", vec![i32, i32, i32, i8]);
        let cbuf_ret = m.named_struct("dx.types.CBufRet.i32", vec![i32, i32, i32, i32]);
        let res_ret = m.named_struct("dx.types.ResRet.f32", vec![f32, f32, f32, f32, i32]);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        let main = m.function("main", main_ty, None, true);
        let entry = m.new_block();
        Shader {
            m,
            #[cfg(test)]
            unaligned_typed_loads: 0,
            #[cfg(test)]
            smem_buffer_bytes: Vec::new(),
            #[cfg(test)]
            image_address_reads: Vec::new(),
            t: Types {
                void,
                i1,
                i32,
                f32,
                handle,
                props,
                bind,
                cbuf_ret,
                res_ret,
            },
            pixel,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh: None,
            geometry: None,
            user_data: 0,
            samplers: 0,
            cmp_samplers: 0,
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            flags: 0,
            nonuniform_handles: 0,
            cbuffer_handles: 0,
            threads: None,
            wave_size: None,
            compute_derivatives: false,
            lds_dwords: 0,
            lds: None,
            main,
            entry,
        }
    }

    /// The thread-group shared memory: `[lds_dwords x i32]` in address
    /// space 3.
    pub fn lds(&mut self) -> V {
        if let Some(g) = self.lds {
            return g;
        }
        let arr = self.m.array(self.lds_dwords, self.t.i32);
        let g = self.m.global("lds", arr, 3);
        self.lds = Some(g);
        g
    }

    /// Calls `dx.op.<name>` (class and overload) with `opcode` and `args`.
    pub fn op(
        &mut self,
        block: usize,
        name: &str,
        attr: Attr,
        ret: Ty,
        opcode: u32,
        args: &[V],
    ) -> V {
        if self.threads.is_some()
            && self.mesh.is_none()
            && matches!(opcode, 53..=56 | 60 | 61 | 64 | 81)
        {
            self.compute_derivatives = true;
        }
        let mut params = vec![self.t.i32];
        params.extend(args.iter().map(|&a| self.m.type_of(a)));
        let fty = self.m.func_ty(ret, params);
        self.m.ptr(fty);
        let f = self
            .m
            .function(&format!("dx.op.{name}"), fty, Some(attr), false);
        let mut all = vec![self.m.i32(opcode)];
        all.extend(args);
        self.m.call(block, f, all)
    }

    /// The handle of the user-data constant buffer, made in the entry block.
    pub fn user_data_handle(&mut self) -> V {
        let bind = [
            self.m.i32(0),
            self.m.i32(0),
            self.m.i32(USER_DATA_SPACE),
            self.m.i8(2), // CBV
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let block = self.entry;
        let (i, f) = (self.m.i32(0), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        // The size is known in `finish`, which fixes this placeholder.
        self.annotate(block, h, 13, u32::MAX)
    }

    /// The handle of the root CBV of root parameter `slot`, made in the
    /// entry block.
    pub fn root_cbv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_cbvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(2)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 13, 65536);
        self.root_cbvs.push((slot, h));
        h
    }

    /// The handle of the root SRV (a raw buffer) of root parameter `slot`.
    pub fn root_srv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_srvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_srvs.push((slot, h));
        h
    }

    /// The handle of the root UAV (a raw buffer the shader may write) of
    /// root parameter `slot`: `u<slot>` in [`ROOT_CBV_SPACE`].
    pub fn root_uav_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_uavs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(1)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        // A raw buffer with unordered access.
        let h = self.annotate(block, h, 11 | 0x1000, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_uavs.push((slot, h));
        h
    }

    /// The handle of immediate sampler `n` (a comparison sampler when its
    /// bit in `cmp_samplers` is set).
    pub fn sampler_handle(&mut self, block: usize, n: u32) -> V {
        let bind = [
            self.m.i32(n),
            self.m.i32(n),
            self.m.i32(SAMPLER_SPACE),
            self.m.i8(3), // sampler
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let (i, f) = (self.m.i32(n), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        let cmp = if self.cmp_samplers >> n & 1 != 0 {
            0x8000
        } else {
            0
        };
        self.annotate(block, h, 14 | cmp, 0)
    }

    pub fn annotate(&mut self, block: usize, h: V, props0: u32, props1: u32) -> V {
        let p = [self.m.i32(props0), self.m.i32(props1)];
        let p = self.m.aggregate(self.t.props, &p);
        self.op(
            block,
            "annotateHandle",
            Attr::ReadNone,
            self.t.handle,
            216,
            &[h, p],
        )
    }

    /// A descriptor heap handle, annotated with `props`; `nonuniform` when
    /// the index may differ between lanes (`NonUniformResourceIndex`).
    /// Indices at or past [`super::VIEW_HEAP_LIMIT`] /
    /// [`super::SAMPLER_HEAP_LIMIT`] (garbage or negative values from
    /// dropped passes) are clamped to slot 0, so `ResourceDescriptorHeap` /
    /// `SamplerDescriptorHeap` reads never go far past any heap. The bound
    /// is the largest heap, not the one bound: an index just past a smaller
    /// heap's end still reads past it.
    pub fn heap_handle(
        &mut self,
        block: usize,
        index: V,
        sampler: bool,
        nonuniform: bool,
        props: (u32, u32),
    ) -> V {
        self.flags |= if sampler {
            flags::SAMPLER_HEAP
        } else {
            flags::RESOURCE_HEAP
        };
        let limit = self.m.i32(if sampler {
            super::SAMPLER_HEAP_LIMIT
        } else {
            super::VIEW_HEAP_LIMIT
        });
        let zero = self.m.i32(0);
        let in_bounds = self.m.cmp(block, super::bitcode::pred::IULT, index, limit);
        let index = self.m.select(block, in_bounds, index, zero);
        self.nonuniform_handles += u32::from(nonuniform);
        self.cbuffer_handles += u32::from(props.0 == 13);
        let (s, n) = (self.m.bool(sampler), self.m.bool(nonuniform));
        let h = self.op(
            block,
            "createHandleFromHeap",
            Attr::ReadNone,
            self.t.handle,
            218,
            &[index, s, n],
        );
        self.annotate(block, h, props.0, props.1)
    }

    /// Omit unread user semantics from a translated pixel shader so an upstream
    /// mesh shader can spend its output budget on what the pixel shader reads.
    pub(super) fn prune_pixel_inputs(&mut self) {
        if !self.pixel || !self.inputs.iter().any(|e| e.kind == 0 && e.usage == 0) {
            return;
        }
        let mut remap = std::collections::HashMap::new();
        let mut next = 0u32;
        for (index, element) in self.inputs.iter().enumerate() {
            if element.kind != 0 || element.usage != 0 {
                if index as u32 != next {
                    remap.insert(self.m.i32(index as u32), self.m.i32(next));
                }
                next += 1;
            }
        }
        self.m.remap_input_loads(&remap);
        self.inputs.retain(|e| e.kind != 0 || e.usage != 0);
    }

    /// The finished container.
    pub fn finish(mut self) -> Vec<u8> {
        let ud_size = (self.user_data * 4).next_multiple_of(16);
        self.fix_user_data_size(ud_size);
        // The clean-up, before anything is declared about the code: a
        // feature whose last use it removed must not be declared (the
        // validator compares the flags with what the code uses).
        let passes = super::shader_opt() & super::bitcode::opt::MODULE;
        if passes != 0 {
            self.m.optimise(passes);
            let used = self.m.usage();
            for (flag, used) in [
                (flags::WAVE_OPS, used.wave_ops),
                (flags::INT64, used.int64),
                (flags::TYPED_UAV_LOADS, used.typed_uav_loads),
                (flags::RESOURCE_HEAP, used.heap_views),
                (flags::SAMPLER_HEAP, used.heap_samplers),
                (flags::ATOMIC_TYPED64 | flags::ATOMIC_HEAP64, used.atomic64),
            ] {
                if !used {
                    self.flags &= !flag;
                }
            }
        }
        container::separate_interpolation(&mut self.inputs);
        container::separate_interpolation(&mut self.outputs);
        let mut resources = None;
        let mut bindings = Vec::new();
        let i32c = |s: &mut Self, v: u32| {
            let c = s.m.i32(v);
            s.m.md_value(c)
        };
        if self.user_data > 0
            || self.samplers > 0
            || !self.root_cbvs.is_empty()
            || !self.root_srvs.is_empty()
            || !self.root_uavs.is_empty()
        {
            let empty = self.m.md_str("");
            let mut cbvs = None;
            let mut cbv_list = Vec::new();
            if self.user_data > 0 {
                let arr = self.m.array(self.user_data.next_multiple_of(4), self.t.i32);
                let ty = self.m.named_struct("UserData", vec![arr]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let ops = vec![
                    Some(i32c(&mut self, 0)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, USER_DATA_SPACE)),
                    Some(i32c(&mut self, 0)),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, ud_size)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: USER_DATA_SPACE,
                    register: 0,
                    dxil_kind: 13,
                });
            }
            let blob = self.m.array(16384, self.t.i32);
            let ty = self.m.named_struct("RootCbv", vec![blob]);
            let p = self.m.ptr(ty);
            let u = self.m.undef(p);
            for (slot, _) in self.root_cbvs.clone() {
                let ops = vec![
                    Some(i32c(&mut self, cbv_list.len() as u32)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, ROOT_CBV_SPACE)),
                    Some(i32c(&mut self, u32::from(slot))),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, 65536)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: ROOT_CBV_SPACE,
                    register: u32::from(slot),
                    dxil_kind: 13,
                });
            }
            if !cbv_list.is_empty() {
                cbvs = Some(self.m.md_node(cbv_list));
            }
            let mut srvs = None;
            if !self.root_srvs.is_empty() {
                let ty = self
                    .m
                    .named_struct("struct.ByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                for (n, (slot, _)) in self.root_srvs.clone().into_iter().enumerate() {
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, ROOT_CBV_SPACE)),
                        Some(i32c(&mut self, u32::from(slot))),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, 11)),
                        Some(i32c(&mut self, 0)),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 4,
                        space: ROOT_CBV_SPACE,
                        register: u32::from(slot),
                        dxil_kind: 11,
                    });
                }
                srvs = Some(self.m.md_node(list));
            }
            let mut uavs = None;
            if !self.root_uavs.is_empty() {
                let ty = self
                    .m
                    .named_struct("struct.RWByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let no = self.m.bool(false);
                let no = self.m.md_value(no);
                let mut list = Vec::new();
                for (n, (slot, _)) in self.root_uavs.clone().into_iter().enumerate() {
                    // Id, variable, name, space, lower bound, range size,
                    // shape (11: a raw buffer), globally coherent, counter,
                    // rasterizer ordered, extra.
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, ROOT_CBV_SPACE)),
                        Some(i32c(&mut self, u32::from(slot))),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, 11)),
                        Some(no),
                        Some(no),
                        Some(no),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    // PSV0 kind 7: a raw buffer with unordered access.
                    bindings.push(Binding {
                        kind: 7,
                        space: ROOT_CBV_SPACE,
                        register: u32::from(slot),
                        dxil_kind: 11,
                    });
                }
                uavs = Some(self.m.md_node(list));
            }
            let mut samplers = None;
            if self.samplers > 0 {
                let ty = self.m.named_struct("struct.SamplerState", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                for n in 0..self.samplers {
                    let kind = (self.cmp_samplers >> n & 1) as u32;
                    let ops = vec![
                        Some(i32c(&mut self, n)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, SAMPLER_SPACE)),
                        Some(i32c(&mut self, n)),
                        Some(i32c(&mut self, 1)),
                        // Sampler kind: 1 comparison.
                        Some(i32c(&mut self, kind)),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 1,
                        space: SAMPLER_SPACE,
                        register: n,
                        dxil_kind: 14,
                    });
                }
                samplers = Some(self.m.md_node(list));
            }
            resources = Some(self.m.md_node(vec![srvs, uavs, cbvs, samplers]));
            // PSV0 lists them as the module does: CBVs, samplers, SRVs, UAVs.
            bindings.sort_by_key(|b| match b.kind {
                2 => 0,
                1 => 1,
                7 => 3,
                _ => 2,
            });
        }
        if let Some(r) = resources {
            self.m.named_md("dx.resources", vec![r]);
        }

        let graphics = self.threads.is_none() || self.mesh.is_some();
        if graphics {
            // shortcut: every output is declared to depend on every input; the
            // exact sets need a dependence pass. Drivers read this state.
            let scalars = |els: &[SigElement]| {
                els.iter()
                    .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                    .max()
                    .unwrap_or(0)
            };
            let (ni, no) = (scalars(&self.inputs), scalars(&self.outputs));
            let mut view = vec![ni, no];
            // Mesh shaders have no inputs: then the primitive outputs' count.
            if self.mesh.is_some() {
                view.push(scalars(&self.prims));
            }
            for _ in 0..ni {
                for k in 0..no.div_ceil(32) {
                    let bits = (no - 32 * k).min(32);
                    view.push(if bits == 32 {
                        u32::MAX
                    } else {
                        (1 << bits) - 1
                    });
                }
            }
            // A geometry shader lists four streams: the other three have
            // no outputs.
            if self.geometry.is_some() {
                view.extend([0, 0, 0]);
            }
            let arr = self.m.array(view.len() as u32, self.t.i32);
            let elems: Vec<V> = view.iter().map(|&v| self.m.i32(v)).collect();
            let arr = self.m.aggregate(arr, &elems);
            let arr = self.m.md_value(arr);
            let view_node = self.m.md_node(vec![Some(arr)]);
            self.m.named_md("dx.viewIdState", vec![view_node]);
        }

        let sigs = if graphics {
            let ins = self.signature_md(self.inputs.clone());
            let outs = self.signature_md(self.outputs.clone());
            let prims = self.signature_md(self.prims.clone());
            Some(self.m.md_node(vec![ins, outs, prims]))
        } else {
            None
        };
        // A pixel shader with an SV_StencilRef output (as DXC collects it).
        if self.pixel && self.outputs.iter().any(|o| o.kind == 20) {
            self.flags |= flags::STENCIL_REF;
        }
        if self.threads.is_none()
            && !self.pixel
            && self.geometry.is_none()
            && self.outputs.iter().any(|o| matches!(o.kind, 4 | 5))
        {
            self.flags |= flags::VIEWPORT_RT_INDEX;
        }
        // Entry properties: shader flags (tag 0), thread-group size (4),
        // wave size (11).
        let mut props = Vec::new();
        if self.flags != 0 {
            props.push(i32c(&mut self, 0));
            let i64t = self.m.int(64);
            let f = self.m.int_const(i64t, self.flags as i64);
            props.push(self.m.md_value(f));
        }
        if let Some(g) = self.geometry {
            // Input primitive (3: triangles), maximum vertices, stream
            // mask, output topology (5: triangle strips), instances.
            props.push(i32c(&mut self, 1));
            let state = [3, g.max_vertices, 1, 5, 1].map(|n| Some(i32c(&mut self, n)));
            props.push(self.m.md_node(state.to_vec()));
        }
        if let Some(threads) = self.threads {
            let t = threads.map(|n| Some(i32c(&mut self, n)));
            let t = self.m.md_node(t.to_vec());
            match self.mesh {
                // Mesh state (tag 9): threads, maximum vertices and
                // primitives, topology, payload bytes.
                Some(mesh) => {
                    props.push(i32c(&mut self, 9));
                    let state = [
                        mesh.max_vertices,
                        mesh.max_primitives,
                        mesh.topology.into(),
                        0,
                    ]
                    .map(|n| Some(i32c(&mut self, n)));
                    let mut node = vec![Some(t)];
                    node.extend(state);
                    props.push(self.m.md_node(node));
                }
                None => {
                    props.push(i32c(&mut self, 4));
                    props.push(t);
                }
            }
        }
        if let Some(n) = self.wave_size {
            props.push(i32c(&mut self, 11));
            let n = Some(i32c(&mut self, n));
            props.push(self.m.md_node(vec![n]));
        }
        let props =
            (!props.is_empty()).then(|| self.m.md_node(props.into_iter().map(Some).collect()));
        let main = self.m.md_value(self.main);
        let name = self.m.md_str("main");
        let entry = self
            .m
            .md_node(vec![Some(main), Some(name), sigs, resources, props]);

        let ident = self.m.md_str("victory");
        let ident = self.m.md_node(vec![Some(ident)]);
        self.m.named_md("llvm.ident", vec![ident]);
        let one = i32c(&mut self, 1);
        let six = i32c(&mut self, 6);
        let version = self.m.md_node(vec![Some(one), Some(six)]);
        self.m.named_md("dx.version", vec![version]);
        let val_minor = if self.compute_derivatives { 8 } else { 6 };
        let val_minor = i32c(&mut self, val_minor);
        let val_version = self.m.md_node(vec![Some(one), Some(val_minor)]);
        self.m.named_md("dx.valver", vec![val_version]);
        // Program kinds: 0 pixel, 1 vertex, 5 compute, 13 mesh.
        let (name, kind) = match (self.threads, self.pixel) {
            _ if self.mesh.is_some() => ("ms", 13u32),
            _ if self.geometry.is_some() => ("gs", 2),
            (Some(_), _) => ("cs", 5u32),
            (None, true) => ("ps", 0),
            (None, false) => ("vs", 1),
        };
        let name = self.m.md_str(name);
        let sm = self.m.md_node(vec![Some(name), Some(six), Some(six)]);
        self.m.named_md("dx.shaderModel", vec![sm]);
        self.m.named_md("dx.entryPoints", vec![entry]);

        // Compute lowering can generate unused register results that otherwise
        // keep arithmetic and entire chains of merge values in the host module.
        let bitcode = self.m.write(!graphics);
        let mut dxil = Vec::with_capacity(24 + bitcode.len());
        dxil.extend((kind << 16 | 0x66).to_le_bytes());
        dxil.extend((((24 + bitcode.len()) / 4) as u32).to_le_bytes());
        dxil.extend(b"DXIL");
        dxil.extend(0x106u32.to_le_bytes());
        dxil.extend(16u32.to_le_bytes());
        dxil.extend((bitcode.len() as u32).to_le_bytes());
        dxil.extend(&bitcode);

        let mut features = 0u64;
        if self.flags & flags::STENCIL_REF != 0 {
            features |= 0x200;
        }
        if self.flags & flags::TYPED_UAV_LOADS != 0 {
            features |= 0x800;
        }
        if self.flags & flags::VIEWPORT_RT_INDEX != 0 {
            features |= 0x2000;
        }
        if self.flags & flags::WAVE_OPS != 0 {
            features |= 0x4000;
        }
        if self.flags & flags::INT64 != 0 {
            features |= 0x8000;
        }
        if self.flags & flags::BARYCENTRICS != 0 {
            features |= 0x2_0000;
        }
        if self.flags & flags::RESOURCE_HEAP != 0 {
            features |= super::SFI0_RESOURCE_HEAP;
        }
        if self.flags & flags::SAMPLER_HEAP != 0 {
            features |= super::SFI0_SAMPLER_HEAP;
        }
        if self.flags & flags::ATOMIC_TYPED64 != 0 {
            features |= 0x40_0000;
        }
        if self.flags & flags::ATOMIC_HEAP64 != 0 {
            features |= 0x1000_0000;
        }
        let sfi0 = features.to_le_bytes();
        let isg1 = container::signature(&self.inputs, true);
        let osg1 = container::signature(&self.outputs, false);
        let flag = if self.pixel {
            self.outputs.iter().any(|o| (17..=19).contains(&o.kind))
        } else {
            self.outputs.iter().any(|o| o.kind == 3)
        };
        let psv0 = container::psv(&Psv {
            stage: kind as u8,
            flag: flag && self.threads.is_none(),
            inputs: &self.inputs,
            outputs: &self.outputs,
            prims: &self.prims,
            mesh: self.mesh.map(|m| (m, self.lds_dwords * 4)),
            geometry: self.geometry.map(|g| g.max_vertices),
            bindings: &bindings,
            threads: self.threads.unwrap_or_default(),
            wave_size: self.wave_size,
            validator18: self.compute_derivatives,
        });
        let hash = [0u8; 20];
        let psg1 = container::signature(&self.prims, false);
        let mut parts: Vec<(&[u8; 4], &[u8])> =
            vec![(b"SFI0", &sfi0), (b"ISG1", &isg1), (b"OSG1", &osg1)];
        if self.mesh.is_some() {
            parts.push((b"PSG1", &psg1));
        }
        parts.extend([(b"PSV0", &psv0[..]), (b"HASH", &hash), (b"DXIL", &dxil)]);
        container::build(&parts)
    }

    /// The user-data buffer's annotation carries its size, known only now.
    fn fix_user_data_size(&mut self, size: u32) {
        let old = [self.m.i32(13), self.m.i32(u32::MAX)];
        let old = self.m.aggregate(self.t.props, &old);
        let new = [self.m.i32(13), self.m.i32(size)];
        let new = self.m.aggregate(self.t.props, &new);
        self.m.replace_const(old, new);
    }

    fn signature_md(&mut self, elements: Vec<SigElement>) -> Option<Md> {
        if elements.is_empty() {
            return None;
        }
        let mut list = Vec::new();
        for (id, e) in elements.iter().enumerate() {
            let c32 = |s: &mut Self, v: u32| {
                let c = s.m.i32(v);
                Some(s.m.md_value(c))
            };
            let c8 = |s: &mut Self, v: u8| {
                let c = s.m.i8(v);
                Some(s.m.md_value(c))
            };
            let idx = c32(self, e.index);
            let indices = Some(self.m.md_node(vec![idx]));
            let usage = if e.usage != 0 {
                let (tag, mask) = (c32(self, 3), c32(self, u32::from(e.usage)));
                Some(self.m.md_node(vec![tag, mask]))
            } else {
                None
            };
            let ops = vec![
                c32(self, id as u32),
                Some(self.m.md_str(e.semantic_name())),
                c8(self, e.comp),
                c8(self, e.kind),
                indices,
                c8(self, e.interp),
                c32(self, 1),
                c8(self, e.cols),
                c32(self, e.row.unwrap_or(u32::MAX)),
                c8(self, if e.row.is_some() { e.col } else { u8::MAX }),
                usage,
            ];
            list.push(Some(self.m.md_node(ops)));
        }
        Some(self.m.md_node(list))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::bitcode::Term;
    use super::super::validator;
    use super::*;

    use crate::dxbc::{Container, FourCC};

    fn element(name: &str, row: u32, col: u8, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind: 0,
            comp: 9,
            interp: 2,
            row: Some(row),
            col,
            cols,
            usage: 0,
            half: false,
        }
    }

    fn change_part(dxil: &[u8], part: FourCC, edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let c = Container::parse(dxil).unwrap();
        let part = c.part(part).unwrap();
        let start = part.offset + 8;
        let mut changed = dxil.to_vec();
        edit(&mut changed[start..start + part.data.len()]);
        let digest = container::hash(&changed[20..]);
        changed[4..20].copy_from_slice(&digest);
        changed
    }

    #[test]
    fn packed_signature_records_keep_metadata_order_even_for_unused_elements() {
        let Some(mut validator) = validator::Validator::for_test() else {
            return;
        };
        let mut shader = Shader::new(false);
        // A packed element at w precedes an unread element at x, as it does
        // in module metadata. The signature part must preserve that order.
        let mut high = element("HIGH", 0, 3, 1);
        high.usage = 1;
        shader.outputs = vec![high, element("LOW", 0, 0, 1)];
        shader.inputs = shader.outputs.clone();
        for e in &mut shader.inputs {
            e.interp = 0;
        }
        shader.m.terminate(shader.entry, Term::Ret);
        let mut dxil = shader.finish();
        for (part, label, unused) in [
            (FourCC::ISG1, "Program Input Signature", 0),
            (FourCC::OSG1, "Program Output Signature", 15),
        ] {
            let mut swapped = change_part(&dxil, part, |data| {
                assert_eq!(data[8 + 24], 8);
                assert_eq!(data[40 + 24], 1);
                assert_eq!(data[40 + 25], unused);
                data[8..72].rotate_left(32);
            });
            let error = validator.validate(&mut swapped).unwrap_err();
            assert!(
                error.contains(&format!("'{label}' does not match expected for module")),
                "{error}"
            );
        }
        validator.validate(&mut dxil).unwrap();
    }

    #[test]
    fn system_value_names_match_in_vertex_and_primitive_signatures() {
        let Some(mut validator) = validator::Validator::for_test() else {
            return;
        };
        let mut vertex = Shader::new(false);
        let mut viewport = element("sV_vIeWpOrTaRrAyInDeX", 0, 0, 1);
        viewport.kind = 5;
        viewport.comp = 5;
        viewport.interp = 1;
        viewport.usage = 1;
        vertex.outputs.push(viewport.clone());
        let zero = vertex.m.i32(0);
        let col = vertex.m.i8(0);
        vertex.op(
            vertex.entry,
            "storeOutput.i32",
            Attr::NoUnwind,
            vertex.t.void,
            5,
            &[zero, zero, col, zero],
        );
        vertex.m.terminate(vertex.entry, Term::Ret);
        let mut dxil = vertex.finish();
        let mut misnamed = change_part(&dxil, FourCC::OSG1, |data| {
            let name = crate::dxbc::u32_at(data, 12).unwrap() as usize;
            assert_eq!(&data[name..name + 22], b"SV_ViewportArrayIndex\0");
            data[name] = b's';
        });
        let error = validator.validate(&mut misnamed).unwrap_err();
        assert!(
            error.contains("'Program Output Signature' does not match expected for module"),
            "{error}"
        );
        validator.validate(&mut dxil).unwrap();

        let mut mesh = empty_mesh(1, 1);
        viewport.usage = 0;
        mesh.prims.push(viewport);
        let mut dxil = mesh.finish();
        let c = Container::parse(&dxil).unwrap();
        let psg = c.part(FourCC::PSG1).unwrap().data;
        let name = crate::dxbc::u32_at(psg, 12).unwrap() as usize;
        assert_eq!(&psg[name..], b"SV_ViewportArrayIndex\0");
        validator.validate(&mut dxil).unwrap();
    }

    fn empty_mesh(max_vertices: u32, max_primitives: u32) -> Shader {
        let mut s = Shader::new(false);
        s.threads = Some([32, 1, 1]);
        s.mesh = Some(Mesh {
            max_vertices,
            max_primitives,
            topology: 2,
        });
        let zero = s.m.i32(0);
        s.op(
            s.entry,
            "setMeshOutputCounts",
            Attr::NoUnwind,
            s.t.void,
            168,
            &[zero, zero],
        );
        s.m.terminate(s.entry, Term::Ret);
        s
    }

    #[test]
    fn mesh_output_price_counts_declared_components_and_rounds_each_count() {
        let Some(mut validator) = validator::Validator::for_test() else {
            return;
        };
        for (vertices, primitives, rows, cols, prim_rows, stride, expected_bytes) in [
            (256, 0, 8, 4, 0, 2, 32_768),
            (256, 0, 32, 1, 0, 1, 32_768),
            (224, 1, 9, 4, 1, 1, 32_768),
            (224, 33, 9, 4, 1, 1, 33_280),
            (225, 0, 9, 4, 0, 1, 36_864),
        ] {
            let mut s = empty_mesh(vertices, primitives);
            for row in 0..rows {
                s.outputs.push(SigElement {
                    index: row,
                    ..element("VALUE", row * stride, 0, cols)
                });
            }
            for row in 0..prim_rows {
                s.prims.push(SigElement {
                    index: row,
                    interp: 1,
                    ..element("PRIM", row, 0, 4)
                });
            }
            assert_eq!(
                s.mesh.unwrap().output_bytes(&s.outputs, &s.prims),
                expected_bytes
            );
            let result = validator.validate(&mut s.finish());
            if expected_bytes <= Mesh::MAX_OUTPUT_BYTES {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.contains("vertex plus primitive output size is greater than 32768"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn compute_implicit_sample_validates() {
        let mut s = Shader::new(false);
        s.threads = Some([32, 1, 1]);
        s.wave_size = Some(32);
        s.samplers = 1;
        let block = s.entry;
        let index = s.m.i32(0);
        let texture = s.heap_handle(block, index, false, false, (2, 1033));
        let sampler = s.sampler_handle(block, 0);
        let zero = s.m.f32(0);
        let fu = s.m.undef(s.t.f32);
        let iu = s.m.undef(s.t.i32);
        let args = [texture, sampler, zero, zero, fu, fu, iu, iu, iu, fu];
        let ret = s.t.res_ret;
        s.op(block, "sample.f32", Attr::ReadOnly, ret, 60, &args);
        s.m.terminate(block, Term::Ret);
        let mut dxil = s.finish();
        let path = validator::Validator::default_path();
        let Ok(mut validator) = validator::Validator::load(path.as_deref()) else {
            eprintln!("no dxil.dll; skipped");
            return;
        };
        validator.validate(&mut dxil).unwrap();
    }
}
