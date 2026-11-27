//! Operand fields of GFX10 instructions, by encoding.
//!
//! Layouts follow AMD's RDNA ISA reference. Register ranges carry their size
//! in dwords, which depends on the opcode (read from its name) and, for lane
//! masks (VCC, EXEC, compare results), on the wave size.

use super::{Encoding, Instruction};

/// A source or destination of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// Scalar registers by operand code: 0-105 `s0`-`s105`, 106/107
    /// `vcc_lo`/`vcc_hi`, 108-123 `ttmp0`-`ttmp15`, 124 `m0`, 125 `null`,
    /// 126/127 `exec_lo`/`exec_hi`. Synthetic codes 128 and up represent
    /// scalars parked in VGPR lanes after decoding (`crate::ir::PARKED`).
    Scalar {
        code: u8,
        dwords: u8,
    },
    Vector {
        reg: u8,
        dwords: u8,
    },
    /// Inline constant or read-only special source, by operand code
    /// (128-192: 0-64, 193-208: -1 to -16, 240-248: floats, 251-254: vccz,
    /// execz, scc, lds_direct).
    Const(u8),
    /// The 32-bit literal that follows the instruction.
    Literal(u32),
    /// Interpolation attribute and channel (0-3 = x-w); `high` selects the
    /// upper f16 half.
    Attr {
        index: u8,
        chan: u8,
        high: bool,
    },
}

/// Operand code of `null` (no register; reads as 0).
pub const NULL: u8 = 125;
const VCC: u8 = 106;
const EXEC: u8 = 126;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cache {
    pub glc: bool,
    pub slc: bool,
    pub dlc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fields {
    /// SOP1, SOP2, SOPC, SOPK; `imm` is SOPK's 16-bit immediate.
    Sop {
        dst: Option<Operand>,
        src: Vec<Operand>,
        imm: Option<u16>,
    },
    /// The 16-bit immediate: branch offset in dwords, wait counts, message...
    Sopp {
        imm: u16,
    },
    Smem {
        data: Option<Operand>,
        base: Operand,
        /// Signed byte offset, added to `soffset`.
        offset: i32,
        soffset: Operand,
        cache: Cache,
    },
    Valu(Valu),
    Vintrp {
        dst: Operand,
        /// The i/j VGPR, or for `v_interp_mov_f32` the parameter (0 `p10`,
        /// 1 `p20`, 2 `p0`) as [`Operand::Const`].
        src: Operand,
        attr: Operand,
    },
    Ds {
        dst: Option<Operand>,
        addr: Option<Operand>,
        data: Vec<Operand>,
        offset0: u8,
        offset1: u8,
        gds: bool,
    },
    /// MUBUF, and MTBUF with `format` set.
    Buffer {
        /// Loaded, stored, or the atomic operand (and result with `glc`).
        data: Option<Operand>,
        addr: Operand,
        rsrc: Operand,
        soffset: Operand,
        offset: u16,
        offen: bool,
        idxen: bool,
        format: Option<u8>,
        lds: bool,
        tfe: bool,
        cache: Cache,
    },
    Image {
        data: Operand,
        /// One range, or one VGPR per address with the NSA form.
        addr: Vec<Operand>,
        rsrc: Operand,
        sampler: Option<Operand>,
        dmask: u8,
        dim: u8,
        unorm: bool,
        r128: bool,
        a16: bool,
        d16: bool,
        tfe: bool,
        lwe: bool,
        cache: Cache,
    },
    /// FLAT, GLOBAL and SCRATCH.
    Flat {
        dst: Option<Operand>,
        addr: Option<Operand>,
        data: Option<Operand>,
        saddr: Option<Operand>,
        offset: i16,
        lds: bool,
        cache: Cache,
    },
    Exp {
        target: u8,
        /// Per channel; `None` when disabled.
        src: [Option<Operand>; 4],
        compr: bool,
        done: bool,
        vm: bool,
    },
}

/// VOP1, VOP2, VOPC, VOP3 and VOP3P, with their SDWA and DPP forms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Valu {
    /// A VGPR range, an SGPR (`v_readlane_b32`...) or a lane mask (compares).
    pub dst: Option<Operand>,
    /// Carry-out or the second result of VOP3B (`v_add_co_u32`, `v_div_scale_f32`...).
    pub carry: Option<Operand>,
    /// In operand order, including the implicit VCC of VOP2 `v_cndmask_b32`
    /// and carry-in, and the literal of `v_madmk`/`v_madak` forms.
    pub src: Vec<Operand>,
    /// Bit i applies to `src[i]`. For VOP3P, `neg` is the low half's.
    pub neg: u8,
    pub abs: u8,
    /// VOP3P: negate the high half (`v_fma_mix*`: absolute value).
    pub neg_hi: u8,
    /// VOP3: bit 3 is the destination. VOP3P with `opsel_hi`: the halves used.
    pub opsel: u8,
    pub opsel_hi: u8,
    pub clamp: bool,
    /// Output modifier: 1 ×2, 2 ×4, 3 ÷2.
    pub omod: u8,
    pub sdwa: Option<Sdwa>,
    pub dpp: Option<Dpp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sdwa {
    /// Selects: 0-3 byte, 4-5 word, 6 dword. Compares have no `dst_sel`.
    pub dst_sel: u8,
    /// 0 pad with zeros, 1 sign-extend, 2 preserve.
    pub dst_unused: u8,
    pub src_sel: [u8; 2],
    /// Bit i sign-extends `src[i]`.
    pub sext: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dpp {
    /// DPP16: `ctrl` is the 9-bit permutation (quad_perm, row shifts...).
    Row {
        ctrl: u16,
        row_mask: u8,
        bank_mask: u8,
        bound_ctrl: bool,
        fi: bool,
    },
    /// DPP8: eight 3-bit lane selects.
    Lanes { sel: u32, fi: bool },
}

impl Instruction {
    /// Decodes the operand fields. `wave64` sizes lane masks.
    pub fn fields(&self, wave64: bool) -> Fields {
        let name = self.name().unwrap_or("");
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let (w0, w1) = (w(0), w(1));
        let bit = |word: u32, n: u32| word >> n & 1 != 0;
        let cache = |glc, slc, dlc| Cache { glc, slc, dlc };
        match self.encoding {
            Encoding::Sop2 | Encoding::Sop1 | Encoding::Sopc => {
                let (dst, src0, src1) = salu_dwords(name);
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, dst);
                let ssrc = |shift: u32, dwords| source(w0 >> shift & 0xFF, dwords, w1);
                let (dst, src) = match self.encoding {
                    Encoding::Sop2 => (Some(sdst), vec![ssrc(0, src0), ssrc(8, src1)]),
                    Encoding::Sopc => (None, vec![ssrc(0, src0), ssrc(8, src1)]),
                    _ => match name {
                        "s_getpc_b64" => (Some(sdst), vec![]),
                        "s_setpc_b64" | "s_rfe_b64" => (None, vec![ssrc(0, src0)]),
                        _ => (Some(sdst), vec![ssrc(0, src0)]),
                    },
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: None,
                }
            }
            Encoding::Sopk => {
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, 1 + u8::from(name == "s_call_b64"));
                let (dst, src) = match name {
                    "s_setreg_imm32_b32" => (None, vec![Operand::Literal(w1)]),
                    "s_version" => (None, vec![]),
                    n if n.starts_with("s_cmpk_")
                        || n.starts_with("s_waitcnt_")
                        || n == "s_setreg_b32" =>
                    {
                        (None, vec![sdst])
                    }
                    _ => (Some(sdst), vec![]),
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: Some(w0 as u16),
                }
            }
            Encoding::Sopp => Fields::Sopp { imm: w0 as u16 },
            Encoding::Smem => {
                let sdata = (w0 >> 6 & 0x7F) as u8;
                // s_atc_probe*: SDATA is a 7-bit immediate, not an SGPR. The
                // buffer form's resource is 4 dwords. GLC and DLC are not operands.
                let probe = matches!(name, "s_atc_probe" | "s_atc_probe_buffer");
                let data = if probe {
                    Some(Operand::Literal(u32::from(sdata)))
                } else {
                    match name {
                        "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb" => None,
                        n if n.starts_with("s_dcache_discard") => None,
                        "s_memtime" | "s_memrealtime" => Some(scalar(sdata, 2)),
                        _ => Some(scalar(sdata, data_dwords(name))),
                    }
                };
                let base =
                    2 + 2 * u8::from(name.starts_with("s_buffer_") || name == "s_atc_probe_buffer");
                Fields::Smem {
                    data,
                    base: scalar(((w0 & 0x3F) << 1) as u8, base),
                    offset: (w1 << 11) as i32 >> 11,
                    soffset: scalar((w1 >> 25) as u8, 1),
                    cache: if probe {
                        Cache::default()
                    } else {
                        cache(bit(w0, 16), false, bit(w0, 14))
                    },
                }
            }
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 | Encoding::Vop3p => {
                Fields::Valu(self.valu(name, if wave64 { 2 } else { 1 }))
            }
            Encoding::Vintrp => {
                let vsrc = (w0 & 0xFF) as u8;
                Fields::Vintrp {
                    dst: vector(w0 >> 18, 1),
                    src: if name == "v_interp_mov_f32" {
                        Operand::Const(vsrc)
                    } else {
                        vector(vsrc.into(), 1)
                    },
                    attr: Operand::Attr {
                        index: (w0 >> 10 & 0x3F) as u8,
                        chan: (w0 >> 8 & 3) as u8,
                        high: false,
                    },
                }
            }
            Encoding::Ds => {
                let ty = data_dwords(name);
                let pair = ds_pair(name);
                let returns = name.contains("read")
                    || name.contains("_rtn")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_permute_b32"
                            | "ds_bpermute_b32"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                    );
                let no_addr = name.starts_with("ds_gws")
                    || name.contains("addtid")
                    || matches!(name, "ds_nop" | "ds_append" | "ds_consume");
                let data = if name.contains("read")
                    || name.contains("src2")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_nop"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                            | "ds_gws_sema_v"
                            | "ds_gws_sema_p"
                            | "ds_gws_sema_release_all"
                    ) {
                    0
                } else if pair || name.contains("cmpst") || name.contains("mskor") {
                    2
                } else {
                    1
                };
                Fields::Ds {
                    dst: returns.then(|| vector(w1 >> 24, ty * (1 + u8::from(pair)))),
                    addr: (!no_addr).then(|| vector(w1 & 0xFF, 1)),
                    data: (0..data)
                        .map(|i| vector(w1 >> (8 + 8 * i) & 0xFF, ty))
                        .collect(),
                    offset0: w0 as u8,
                    offset1: (w0 >> 8) as u8,
                    gds: bit(w0, 17),
                }
            }
            Encoding::Mubuf | Encoding::Mtbuf => {
                let (offen, idxen, tfe) = (bit(w0, 12), bit(w0, 13), bit(w1, 23));
                let mtbuf = self.encoding == Encoding::Mtbuf;
                Fields::Buffer {
                    data: (!name.ends_with("_inv"))
                        .then(|| vector(w1 >> 8 & 0xFF, data_dwords(name) + u8::from(tfe))),
                    addr: vector(w1 & 0xFF, 1 + u8::from(offen && idxen)),
                    rsrc: scalar(((w1 >> 16 & 0x1F) << 2) as u8, 4),
                    soffset: source(w1 >> 24, 1, 0),
                    offset: (w0 & 0xFFF) as u16,
                    offen,
                    idxen,
                    format: mtbuf.then_some((w0 >> 19 & 0x7F) as u8),
                    lds: !mtbuf && bit(w0, 16),
                    tfe,
                    cache: cache(bit(w0, 14), bit(w1, 22), bit(w0, 15)),
                }
            }
            Encoding::Mimg => {
                let (dim, dmask, a16, d16) = (
                    (w0 >> 3 & 7) as u8,
                    (w0 >> 8 & 0xF) as u8,
                    bit(w1, 30),
                    bit(w1, 31),
                );
                let (r128, tfe, lwe) = (bit(w0, 15), bit(w0, 16), bit(w0, 17));
                // A ray query always returns four dwords and reads a four-dword
                // descriptor, whatever its `dmask` and `r128` bits say.
                let bvh = name.starts_with("image_bvh");
                let mut data = if name.starts_with("image_gather4") || bvh {
                    4
                } else {
                    dmask.count_ones().max(1) as u8
                };
                if d16 && !bvh {
                    data = data.div_ceil(2);
                }
                let count = image_addr_dwords(name, dim, a16);
                let addr = if w0 >> 1 & 3 == 0 {
                    vec![vector(w1 & 0xFF, count)]
                } else {
                    // NSA: one VGPR per address, the first in dword 1, the rest
                    // in the extra dwords.
                    (0..usize::from(count))
                        .map(|i| match i {
                            0 => vector(w1 & 0xFF, 1),
                            i => vector(w(2 + (i - 1) / 4) >> (8 * ((i - 1) % 4)) & 0xFF, 1),
                        })
                        .collect()
                };
                let sampled = ["image_sample", "image_gather4", "image_get_lod"]
                    .iter()
                    .any(|p| name.starts_with(p));
                Fields::Image {
                    data: vector(w1 >> 8 & 0xFF, data + u8::from(tfe || lwe)),
                    addr,
                    rsrc: scalar(
                        ((w1 >> 16 & 0x1F) << 2) as u8,
                        if r128 || bvh { 4 } else { 8 },
                    ),
                    sampler: sampled.then(|| scalar(((w1 >> 21 & 0x1F) << 2) as u8, 4)),
                    dmask,
                    dim,
                    unorm: bit(w0, 12),
                    r128,
                    a16,
                    d16,
                    tfe,
                    lwe,
                    cache: cache(bit(w0, 13), bit(w0, 25), bit(w0, 7)),
                }
            }
            Encoding::Flat | Encoding::Global | Encoding::Scratch => {
                let saddr = (w1 >> 16 & 0x7F) as u8;
                let glc = bit(w0, 16);
                let n = data_dwords(name);
                let (load, atomic) = (name.contains("_load_"), name.contains("_atomic_"));
                let addr = match self.encoding {
                    Encoding::Flat => Some(vector(w1 & 0xFF, 2)),
                    Encoding::Global => Some(vector(w1 & 0xFF, 1 + u8::from(saddr == NULL))),
                    // With an SGPR base, scratch takes no VGPR.
                    _ => (saddr == NULL).then(|| vector(w1 & 0xFF, 1)),
                };
                let ret = if atomic && name.contains("cmpswap") {
                    n / 2
                } else {
                    n
                };
                Fields::Flat {
                    dst: (load || atomic && glc).then(|| vector(w1 >> 24, ret)),
                    addr,
                    data: (!load).then(|| vector(w1 >> 8 & 0xFF, n)),
                    saddr: (self.encoding != Encoding::Flat)
                        .then(|| scalar(saddr, 1 + u8::from(self.encoding == Encoding::Global))),
                    offset: match self.encoding {
                        Encoding::Flat => (w0 & 0x7FF) as i16,
                        _ => ((w0 << 4) as i16) >> 4,
                    },
                    lds: bit(w0, 13),
                    cache: cache(glc, bit(w0, 17), bit(w0, 12)),
                }
            }
            Encoding::Exp => {
                let compr = bit(w0, 10);
                Fields::Exp {
                    target: (w0 >> 4 & 0x3F) as u8,
                    src: std::array::from_fn(|i| {
                        let slot = if compr { i / 2 } else { i };
                        bit(w0, i as u32).then(|| vector(w1 >> (8 * slot) & 0xFF, 1))
                    }),
                    compr,
                    done: bit(w0, 11),
                    vm: bit(w0, 12),
                }
            }
        }
    }

    fn valu(&self, name: &str, lanes: u8) -> Valu {
        // No operands; the console compiler stores a shader hash in their fields.
        if matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp") {
            return Valu::default();
        }
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let w0 = w(0);
        let (dst_n, src_n) = valu_dwords(name);
        let compare = match self.encoding {
            Encoding::Vopc => true,
            Encoding::Vop3 => self.opcode < 0x100,
            _ => false,
        };
        let vop3b = VOP3B.contains(&name);
        let carry_in = name.ends_with("_co_ci_u32") || name == "v_cndmask_b32";
        let mut v = Valu::default();
        match self.encoding {
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc => {
                let ext = w(1);
                let mut src0 = w0 & 0x1FF;
                let vsrc1 = 256 + (w0 >> 9 & 0xFF);
                let mut src1 = vsrc1;
                match src0 {
                    0xF9 => {
                        let sdwa = Sdwa {
                            dst_sel: (ext >> 8 & 7) as u8,
                            dst_unused: (ext >> 11 & 3) as u8,
                            src_sel: [(ext >> 16 & 7) as u8, (ext >> 24 & 7) as u8],
                            sext: (ext >> 19 & 1 | ext >> 26 & 2) as u8,
                        };
                        // The s0/s1 bits select an SGPR or constant instead of a VGPR.
                        src0 = (ext & 0xFF) | if ext >> 23 & 1 == 0 { 256 } else { 0 };
                        if ext >> 31 == 1 {
                            src1 = vsrc1 - 256;
                        }
                        v.neg = (ext >> 20 & 1 | ext >> 27 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 28 & 2) as u8;
                        if compare {
                            if ext >> 15 & 1 == 1 {
                                v.dst = Some(scalar((ext >> 8 & 0x7F) as u8, lanes));
                            }
                            v.sdwa = Some(Sdwa {
                                dst_sel: 6,
                                dst_unused: 0,
                                ..sdwa
                            });
                        } else {
                            v.clamp = ext >> 13 & 1 == 1;
                            v.omod = (ext >> 14 & 3) as u8;
                            v.sdwa = Some(sdwa);
                        }
                    }
                    0xFA => {
                        src0 = 256 + (ext & 0xFF);
                        v.neg = (ext >> 20 & 1 | ext >> 21 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 22 & 2) as u8;
                        v.dpp = Some(Dpp::Row {
                            ctrl: (ext >> 8 & 0x1FF) as u16,
                            row_mask: (ext >> 28) as u8,
                            bank_mask: (ext >> 24 & 0xF) as u8,
                            bound_ctrl: ext >> 19 & 1 == 1,
                            fi: ext >> 18 & 1 == 1,
                        });
                    }
                    0xE9 | 0xEA => {
                        v.dpp = Some(Dpp::Lanes {
                            sel: ext >> 8,
                            fi: src0 == 0xEA,
                        });
                        src0 = 256 + (ext & 0xFF);
                    }
                    _ => {}
                }
                let op0 = source(src0, src_n[0], ext);
                let op1 = source(src1, src_n[1], ext);
                match self.encoding {
                    Encoding::Vopc => {
                        let mask = if name.starts_with("v_cmpx") {
                            EXEC
                        } else {
                            VCC
                        };
                        v.dst = v.dst.or(Some(scalar(mask, lanes)));
                        v.src = vec![op0, op1];
                    }
                    Encoding::Vop1 => {
                        v.dst = Some(valu_dst(name, w0 >> 17 & 0xFF, dst_n));
                        v.src = vec![op0];
                    }
                    _ => {
                        v.dst = Some(vector(w0 >> 17 & 0xFF, dst_n));
                        v.src = match name {
                            n if n.ends_with("mk_f32") || n.ends_with("mk_f16") => {
                                vec![op0, Operand::Literal(ext), op1]
                            }
                            n if n.ends_with("ak_f32") || n.ends_with("ak_f16") => {
                                vec![op0, op1, Operand::Literal(ext)]
                            }
                            _ => vec![op0, op1],
                        };
                        if carry_in {
                            v.src.push(scalar(VCC, lanes));
                        }
                        if name.ends_with("_co_ci_u32") {
                            v.carry = Some(scalar(VCC, lanes));
                        }
                    }
                }
            }
            Encoding::Vop3 | Encoding::Vop3p => {
                let w1 = w(1);
                let field = |i: u32| w1 >> (9 * i) & 0x1FF;
                let count = match self.opcode {
                    _ if self.encoding == Encoding::Vop3p => {
                        if name.starts_with("v_pk_")
                            && !name.contains("_fma_")
                            && !name.contains("_mad_")
                        {
                            2
                        } else {
                            3
                        }
                    }
                    0x000..0x100 => 2,
                    0x100..0x140 => 2 + usize::from(carry_in),
                    0x180..0x200 => 1,
                    _ if VOP3_TWO_SOURCES.contains(&name) => 2,
                    _ => 3,
                };
                v.src = (0..count)
                    .map(|i| {
                        let code = field(i as u32);
                        if i == 0 && name.starts_with("v_interp_") {
                            Operand::Attr {
                                index: (code & 0x3F) as u8,
                                chan: (code >> 6 & 3) as u8,
                                high: code >> 8 & 1 == 1,
                            }
                        } else if i == 2 && carry_in {
                            source(code, lanes, w(2))
                        } else {
                            source(code, src_n[i], w(2))
                        }
                    })
                    .collect();
                v.clamp = w0 >> 15 & 1 == 1;
                v.neg = (w1 >> 29) as u8;
                if self.encoding == Encoding::Vop3p {
                    v.neg_hi = (w0 >> 8 & 7) as u8;
                    v.opsel = (w0 >> 11 & 7) as u8;
                    v.opsel_hi = (w1 >> 27 & 3 | w0 >> 12 & 4) as u8;
                } else {
                    v.omod = (w1 >> 27 & 3) as u8;
                    if vop3b {
                        v.carry = Some(scalar((w0 >> 8 & 0x7F) as u8, lanes));
                    } else {
                        v.abs = (w0 >> 8 & 7) as u8;
                        v.opsel = (w0 >> 11 & 0xF) as u8;
                    }
                }
                let vdst = w0 & 0xFF;
                v.dst = Some(if compare {
                    scalar(vdst as u8, lanes)
                } else {
                    valu_dst(name, vdst, dst_n)
                });
            }
            _ => unreachable!("not a VALU encoding"),
        }
        v
    }
}

/// VOP3-only opcodes that take two sources (the rest take three).
const VOP3_TWO_SOURCES: &[&str] = &[
    "v_add_f64",
    "v_mul_f64",
    "v_min_f64",
    "v_max_f64",
    "v_ldexp_f64",
    "v_mul_lo_u32",
    "v_mul_hi_u32",
    "v_mul_lo_i32",
    "v_mul_hi_i32",
    "v_trig_preop_f64",
    "v_lshlrev_b64",
    "v_lshrrev_b64",
    "v_ashrrev_i64",
    "v_add_nc_u16",
    "v_sub_nc_u16",
    "v_mul_lo_u16",
    "v_lshrrev_b16",
    "v_ashrrev_i16",
    "v_max_u16",
    "v_max_i16",
    "v_min_u16",
    "v_min_i16",
    "v_add_nc_i16",
    "v_sub_nc_i16",
    "v_add_co_u32",
    "v_sub_co_u32",
    "v_subrev_co_u32",
    "v_pack_b32_f16",
    "v_cvt_pknorm_i16_f16",
    "v_cvt_pknorm_u16_f16",
    "v_lshlrev_b16",
    "v_interp_p1ll_f16",
    "v_readlane_b32",
    "v_writelane_b32",
    "v_ldexp_f32",
    "v_bfm_b32",
    "v_bcnt_u32_b32",
    "v_mbcnt_lo_u32_b32",
    "v_mbcnt_hi_u32_b32",
    "v_cvt_pknorm_i16_f32",
    "v_cvt_pknorm_u16_f32",
    "v_cvt_pk_u16_u32",
    "v_cvt_pk_i16_i32",
    "v_sub_nc_i32",
    "v_add_nc_i32",
];

/// VOP3 opcodes whose bits 14:8 hold a scalar destination instead of `abs`/`opsel`.
const VOP3B: &[&str] = &[
    "v_add_co_u32",
    "v_sub_co_u32",
    "v_subrev_co_u32",
    "v_add_co_ci_u32",
    "v_sub_co_ci_u32",
    "v_subrev_co_ci_u32",
    "v_div_scale_f32",
    "v_div_scale_f64",
    "v_mad_u64_u32",
    "v_mad_i64_i32",
];

fn scalar(code: u8, dwords: u8) -> Operand {
    Operand::Scalar { code, dwords }
}

fn vector(reg: u32, dwords: u8) -> Operand {
    Operand::Vector {
        reg: reg as u8,
        dwords,
    }
}

/// A 9-bit source field (8-bit scalar fields are the same codes below 256).
fn source(code: u32, dwords: u8, literal: u32) -> Operand {
    match code {
        0..=127 => scalar(code as u8, dwords),
        255 => Operand::Literal(literal),
        256.. => vector(code - 256, dwords),
        _ => Operand::Const(code as u8),
    }
}

/// VALU destinations that are SGPRs.
fn valu_dst(name: &str, field: u32, dwords: u8) -> Operand {
    match name {
        "v_readlane_b32" | "v_readfirstlane_b32" => scalar(field as u8, 1),
        _ => vector(field, dwords),
    }
}

/// LDS opcodes on two addresses (`ds_read2_b32`...), with `offset0`/`offset1`.
pub(super) fn ds_pair(name: &str) -> bool {
    name.split('_').any(|t| {
        ["read2", "write2", "wrxchg2"]
            .iter()
            .any(|p| t.starts_with(p))
    })
}

/// Dwords of a type suffix such as `b64` or `f32`.
fn type_dwords(token: &str) -> u8 {
    match token {
        "b64" | "u64" | "i64" | "f64" => 2,
        _ => 1,
    }
}

/// (sdst, ssrc0, ssrc1) dwords of a scalar ALU opcode.
fn salu_dwords(name: &str) -> (u8, u8, u8) {
    match name {
        "s_lshl_b64" | "s_lshr_b64" | "s_ashr_i64" | "s_bfe_u64" | "s_bfe_i64"
        | "s_bitcmp0_b64" | "s_bitcmp1_b64" => (2, 2, 1),
        "s_bfm_b64" | "s_bitset0_b64" | "s_bitset1_b64" | "s_bitreplicate_b64_b32" => (2, 1, 1),
        "s_bcnt0_i32_b64" | "s_bcnt1_i32_b64" | "s_ff0_i32_b64" | "s_ff1_i32_b64"
        | "s_flbit_i32_b64" | "s_flbit_i32_i64" => (1, 2, 2),
        _ => {
            let n = type_dwords(name.rsplit('_').next().unwrap_or(""));
            (n, n, n)
        }
    }
}

/// (vdst, [src0, src1, src2]) dwords of a VALU opcode.
fn valu_dwords(name: &str) -> (u8, [u8; 3]) {
    let parts: Vec<&str> = name.split('_').collect();
    match name {
        "v_lshlrev_b64" | "v_lshrrev_b64" | "v_ashrrev_i64" => (2, [1, 2, 2]),
        "v_ldexp_f64" | "v_trig_preop_f64" | "v_cmp_class_f64" | "v_cmpx_class_f64" => {
            (2, [2, 1, 2])
        }
        "v_mad_u64_u32" | "v_mad_i64_i32" => (2, [1, 1, 2]),
        "v_qsad_pk_u16_u8" | "v_mqsad_pk_u16_u8" => (2, [2, 1, 2]),
        "v_mqsad_u32_u8" => (4, [2, 1, 4]),
        "v_frexp_exp_i32_f64" => (1, [2; 3]),
        _ if parts.len() == 4 && parts[1] == "cvt" => {
            (type_dwords(parts[2]), [type_dwords(parts[3]); 3])
        }
        _ => {
            let n = type_dwords(parts.last().copied().unwrap_or(""));
            (n, [n; 3])
        }
    }
}

/// Dwords of loaded, stored or atomic data, from the opcode name.
fn data_dwords(name: &str) -> u8 {
    let mut n: u8 = 1;
    for token in name.split('_') {
        n = match token {
            "dwordx2" | "x2" | "xy" | "b64" | "u64" | "i64" | "f64" => 2,
            "dwordx3" | "xyz" | "b96" => 3,
            "dwordx4" | "xyzw" | "b128" => 4,
            "dwordx8" => 8,
            "dwordx16" => 16,
            _ => n,
        };
    }
    if name.contains("cmpswap") {
        n *= 2;
    }
    if name.contains("format_d16") {
        n = n.div_ceil(2);
    }
    n
}

/// Address dwords of an image opcode: offset, bias, compare, derivatives,
/// coordinates, then LOD/clamp/mip, packed in pairs with `a16`.
fn image_addr_dwords(name: &str, dim: u8, a16: bool) -> u8 {
    // (coordinates, derivatives per direction) for 1D 2D 3D CUBE 1D_ARRAY
    // 2D_ARRAY 2D_MSAA 2D_MSAA_ARRAY.
    const DIMS: [(u8, u8); 8] = [
        (1, 1),
        (2, 2),
        (3, 3),
        (3, 2),
        (2, 1),
        (3, 2),
        (3, 2),
        (4, 2),
    ];
    let (coords, grads) = DIMS[usize::from(dim & 7)];
    match name {
        "image_bvh_intersect_ray" => return if a16 { 8 } else { 11 },
        "image_bvh64_intersect_ray" => return if a16 { 9 } else { 12 },
        "image_get_resinfo" => return 1,
        _ => {}
    }
    let (mut whole, mut derivs, mut packed): (u8, u8, u8) = (0, 0, coords);
    if let Some(rest) = name
        .strip_prefix("image_sample")
        .or_else(|| name.strip_prefix("image_gather4"))
    {
        for token in rest.split('_') {
            match token {
                "o" | "b" | "c" => whole += 1,
                "d" | "cd" => derivs = grads,
                "l" | "cl" => packed += 1,
                _ => {}
            }
        }
    } else if name.contains("_mip") {
        packed += 1;
    }
    // On GFX10.3, `a16` packs coordinates/LOD/clamp into 16-bit pairs (and
    // bias stays in its own VGPR as an f16 in the low half), while derivatives
    // remain 32-bit unless the opcode is a `_g16` form.
    let g16 = name.ends_with("_g16");
    let deriv_dwords = if g16 {
        2 * derivs.div_ceil(2)
    } else {
        2 * derivs
    };
    let coord_dwords = if a16 { packed.div_ceil(2) } else { packed };
    whole + deriv_dwords + coord_dwords
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::decode;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn first(words: &[u32]) -> Instruction {
        let code: Vec<u8> = words
            .iter()
            .chain(&[S_ENDPGM])
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let mut list = decode(&code).expect("synthetic words decode");
        let inst = list.remove(0);
        assert_eq!(inst.words, words, "instruction length");
        inst
    }

    fn fields(words: &[u32]) -> (&'static str, Fields) {
        let inst = first(words);
        (inst.name().expect("known opcode"), inst.fields(false))
    }

    fn sgpr(code: u8, dwords: u8) -> Operand {
        Operand::Scalar { code, dwords }
    }

    fn vgpr(reg: u8, dwords: u8) -> Operand {
        Operand::Vector { reg, dwords }
    }

    fn valu(words: &[u32], wave64: bool) -> (&'static str, Valu) {
        let inst = first(words);
        let Fields::Valu(v) = inst.fields(wave64) else {
            panic!("not a VALU instruction");
        };
        (inst.name().expect("known opcode"), v)
    }

    /// MIMG words: opcode, dimension, data mask, `a16`, extra NSA dwords.
    fn mimg(op: u32, dim: u32, dmask: u32, a16: bool, nsa: u32) -> [u32; 2] {
        [
            0xF000_0000 | (op & 0x7F) << 18 | (op >> 7 & 1) | nsa << 1 | dim << 3 | dmask << 8,
            8 | 12 << 8 | 3 << 16 | 2 << 21 | u32::from(a16) << 30,
        ]
    }

    /// Dimension codes: 1D 2D 3D CUBE 1D_ARRAY 2D_ARRAY 2D_MSAA 2D_MSAA_ARRAY.
    const DIMS: std::ops::Range<u8> = 0..8;

    /// Address dwords per dimension for `image_sample_d` without `a16` or
    /// `_g16`: 32-bit gradients in two directions, then 32-bit coordinates.
    const SAMPLE_D: [u8; 8] = [3, 6, 9, 7, 4, 7, 7, 8];
    /// The same with `a16`: coordinates in pairs, gradients still 32-bit.
    const SAMPLE_D_A16: [u8; 8] = [3, 5, 8, 6, 3, 6, 6, 6];
    /// The same with `_g16` only: gradients in pairs per direction.
    const SAMPLE_D_G16: [u8; 8] = [3, 4, 7, 5, 4, 5, 5, 6];
    /// The same with `_g16` and `a16`.
    const SAMPLE_D_G16_A16: [u8; 8] = [3, 3, 6, 4, 3, 4, 4, 4];

    #[test]
    fn sample_d_gradients_stay_32_bit_under_a16() {
        for dim in DIMS {
            let i = usize::from(dim);
            assert_eq!(image_addr_dwords("image_sample_d", dim, false), SAMPLE_D[i]);
            assert_eq!(
                image_addr_dwords("image_sample_d", dim, true),
                SAMPLE_D_A16[i]
            );
        }
        assert_eq!(image_addr_dwords("image_sample_d", 1, true), 5);
    }

    #[test]
    fn g16_packs_gradients_without_a16() {
        for dim in DIMS {
            let got = image_addr_dwords("image_sample_d_g16", dim, false);
            assert_eq!(got, SAMPLE_D_G16[usize::from(dim)], "dim {dim}");
        }
        assert_eq!(image_addr_dwords("image_sample_d_g16", 1, false), 4);
    }

    #[test]
    fn g16_with_a16_packs_both() {
        for dim in DIMS {
            let got = image_addr_dwords("image_sample_d_g16", dim, true);
            assert_eq!(got, SAMPLE_D_G16_A16[usize::from(dim)], "dim {dim}");
        }
    }

    #[test]
    fn offset_compare_clamp_words_stay_whole_in_gradient_forms() {
        // (opcode, whole dwords, packed extras) around 2D, whose gradients take
        // 4 dwords (32-bit) or 2 (g16) and coordinates 2 (or 1 with a16).
        let cases = [
            ("image_sample_d_o", 1, 0, false),
            ("image_sample_d_cl", 0, 1, false),
            ("image_sample_d_cl_o", 1, 1, false),
            ("image_sample_c_d", 1, 0, false),
            ("image_sample_c_d_cl_o", 2, 1, false),
            ("image_sample_d_o_g16", 1, 0, true),
            ("image_sample_d_cl_g16", 0, 1, true),
            ("image_sample_d_cl_o_g16", 1, 1, true),
            ("image_sample_c_d_g16", 1, 0, true),
            ("image_sample_c_d_o_g16", 2, 0, true),
            ("image_sample_c_d_cl_o_g16", 2, 1, true),
        ];
        for (name, whole, clamp, g16) in cases {
            let grads = if g16 { 2 } else { 4 };
            let base = whole + grads;
            assert_eq!(
                image_addr_dwords(name, 1, false),
                base + 2 + clamp,
                "{name}"
            );
            // a16 packs the coordinates and the clamp together: 3 halves.
            let packed = if clamp == 1 { 2 } else { 1 };
            assert_eq!(
                image_addr_dwords(name, 1, true),
                base + packed,
                "{name} a16"
            );
        }
    }

    #[test]
    fn cd_gradients_never_pack() {
        for dim in DIMS {
            let plain = image_addr_dwords("image_sample_cd", dim, false);
            assert_eq!(plain, SAMPLE_D[usize::from(dim)]);
            let a16 = image_addr_dwords("image_sample_cd", dim, true);
            assert_eq!(a16, SAMPLE_D_A16[usize::from(dim)]);
        }
        assert_eq!(image_addr_dwords("image_sample_c_cd_cl_o", 1, false), 9);
        assert_eq!(image_addr_dwords("image_sample_c_cd_cl_o", 1, true), 8);
    }

    #[test]
    fn non_gradient_image_forms_pack_with_a16() {
        // (opcode, dim, whole + packed halves)
        let cases: [(&str, u8, u8, u8); 12] = [
            ("image_sample", 1, 0, 2),
            ("image_sample_l", 1, 0, 3),
            ("image_sample_lz", 2, 0, 3),
            ("image_sample_b", 1, 1, 2),
            ("image_sample_c_b_cl", 5, 2, 4),
            ("image_sample_c_l_o", 1, 2, 3),
            ("image_gather4_lz_o", 1, 1, 2),
            ("image_gather4_c_b_cl_o", 1, 3, 3),
            ("image_get_lod", 2, 0, 3),
            ("image_load", 2, 0, 3),
            ("image_load_mip", 5, 0, 4),
            ("image_store_mip_pck", 1, 0, 3),
        ];
        for (name, dim, whole, halves) in cases {
            assert_eq!(
                image_addr_dwords(name, dim, false),
                whole + halves,
                "{name}"
            );
            assert_eq!(
                image_addr_dwords(name, dim, true),
                whole + halves.div_ceil(2),
                "{name} a16"
            );
        }
    }

    #[test]
    fn size_queries_and_ray_queries_ignore_dimension_packing() {
        for dim in DIMS {
            assert_eq!(image_addr_dwords("image_get_resinfo", dim, false), 1);
            assert_eq!(image_addr_dwords("image_get_resinfo", dim, true), 1);
        }
        assert_eq!(image_addr_dwords("image_bvh_intersect_ray", 0, false), 11);
        assert_eq!(image_addr_dwords("image_bvh_intersect_ray", 0, true), 8);
        assert_eq!(image_addr_dwords("image_bvh64_intersect_ray", 0, false), 12);
        assert_eq!(image_addr_dwords("image_bvh64_intersect_ray", 0, true), 9);
    }

    #[test]
    fn scalar_ops_decode_operands_and_immediates() {
        let (name, f) = fields(&[0xBE85_0303]);
        assert_eq!(name, "s_mov_b32");
        let want = Fields::Sop {
            dst: Some(sgpr(5, 1)),
            src: vec![sgpr(3, 1)],
            imm: None,
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0x8001_0302]);
        assert_eq!(name, "s_add_u32");
        let want = Fields::Sop {
            dst: Some(sgpr(1, 1)),
            src: vec![sgpr(2, 1), sgpr(3, 1)],
            imm: None,
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xBF06_8001]);
        assert_eq!(name, "s_cmp_eq_u32");
        let want = Fields::Sop {
            dst: None,
            src: vec![sgpr(1, 1), Operand::Const(128)],
            imm: None,
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xB002_1234]);
        assert_eq!(name, "s_movk_i32");
        let want = Fields::Sop {
            dst: Some(sgpr(2, 1)),
            src: vec![],
            imm: Some(0x1234),
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xB182_0007]);
        assert_eq!(name, "s_cmpk_eq_i32");
        let want = Fields::Sop {
            dst: None,
            src: vec![sgpr(2, 1)],
            imm: Some(7),
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xBF8C_C07F]);
        assert_eq!(name, "s_waitcnt");
        assert_eq!(f, Fields::Sopp { imm: 0xC07F });
    }

    #[test]
    fn smem_decodes_sizes_offsets_and_cache_bits() {
        let (name, f) = fields(&[0xF408_0A02, 0xFA00_0040]);
        assert_eq!(name, "s_load_dwordx4");
        let want = Fields::Smem {
            data: Some(sgpr(40, 4)),
            base: sgpr(4, 2),
            offset: 0x40,
            soffset: sgpr(125, 1),
            cache: Cache::default(),
        };
        assert_eq!(f, want);
        // GLC (bit 16) and DLC (bit 14), and a negative 21-bit offset.
        let (_, f) = fields(&[0xF409_4A02, 0xFA1F_FFFC]);
        let Fields::Smem { offset, cache, .. } = f else {
            panic!("not SMEM");
        };
        assert_eq!(offset, -4);
        assert_eq!(
            cache,
            Cache {
                glc: true,
                slc: false,
                dlc: true
            }
        );
        let (name, f) = fields(&[0xF420_0A02, 0xFA00_0000]);
        assert_eq!(name, "s_buffer_load_dword");
        let Fields::Smem { data, base, .. } = f else {
            panic!("not SMEM");
        };
        assert_eq!((data, base), (Some(sgpr(40, 1)), sgpr(4, 4)));
    }

    #[test]
    fn sdwa_selects_modifiers_and_sign_extension() {
        let (name, v) = valu(&[0x0602_06F9, 0x261D_7402], false);
        assert_eq!(name, "v_add_f32");
        assert_eq!(v.dst, Some(vgpr(1, 1)));
        assert_eq!(v.src, [vgpr(2, 1), vgpr(3, 1)]);
        let want = Sdwa {
            dst_sel: 4,
            dst_unused: 2,
            src_sel: [5, 6],
            sext: 1,
        };
        assert_eq!(v.sdwa, Some(want));
        assert_eq!((v.neg, v.abs), (1, 2));
        assert_eq!((v.clamp, v.omod), (true, 1));
        assert_eq!(v.dpp, None);
    }

    #[test]
    fn dpp16_decodes_control_masks_and_modifiers() {
        let (_, v) = valu(&[0x0602_06FA, 0xA56C_1B02], false);
        assert_eq!(v.src, [vgpr(2, 1), vgpr(3, 1)]);
        let want = Dpp::Row {
            ctrl: 0x1B,
            row_mask: 0xA,
            bank_mask: 5,
            bound_ctrl: true,
            fi: true,
        };
        assert_eq!(v.dpp, Some(want));
        assert_eq!((v.neg, v.abs), (2, 1));
        assert_eq!(v.sdwa, None);
    }

    #[test]
    fn dpp8_decodes_lane_selects_and_fetch_inactive() {
        let sel: u32 = (0..8u32).map(|i| (7 - i) << (3 * i)).sum();
        for (src0, fi) in [(0xE9, false), (0xEA, true)] {
            let (_, v) = valu(&[0x0602_0600 | src0, 2 | sel << 8], false);
            assert_eq!(v.dpp, Some(Dpp::Lanes { sel, fi }));
            assert_eq!(v.src, [vgpr(2, 1), vgpr(3, 1)]);
        }
    }

    #[test]
    fn vop3b_carry_follows_wave_size() {
        let words = [0xD70F_8A01, 258 | 129 << 9];
        for (wave64, dwords) in [(false, 1), (true, 2)] {
            let (name, v) = valu(&words, wave64);
            assert_eq!(name, "v_add_co_u32");
            assert_eq!(v.dst, Some(vgpr(1, 1)));
            assert_eq!(v.carry, Some(sgpr(10, dwords)));
            assert_eq!(v.src, [vgpr(2, 1), Operand::Const(129)]);
            assert!(v.clamp);
        }
        let (name, v) = valu(&[0xD56D_6A03, 258 | 259 << 9 | 260 << 18], false);
        assert_eq!(name, "v_div_scale_f32");
        assert_eq!(v.carry, Some(sgpr(0x6A, 1)));
        assert_eq!(v.src, [vgpr(2, 1), vgpr(3, 1), vgpr(4, 1)]);
    }

    #[test]
    fn vop3p_decodes_packed_half_selects() {
        let (name, v) = valu(&[0xCC0F_DD04, 0x5002_0501], false);
        assert_eq!(name, "v_pk_add_f16");
        assert_eq!(v.dst, Some(vgpr(4, 1)));
        assert_eq!(v.src, [vgpr(1, 1), vgpr(2, 1)]);
        assert_eq!((v.opsel, v.opsel_hi), (3, 6));
        assert_eq!((v.neg, v.neg_hi), (2, 5));
        assert!(v.clamp);
    }

    #[test]
    fn interpolation_names_attribute_channel_and_source() {
        let (name, f) = fields(&[0xC80C_1607]);
        assert_eq!(name, "v_interp_p1_f32");
        let want = Fields::Vintrp {
            dst: vgpr(3, 1),
            src: vgpr(7, 1),
            attr: Operand::Attr {
                index: 5,
                chan: 2,
                high: false,
            },
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xC802_1601]);
        assert_eq!(name, "v_interp_mov_f32");
        let Fields::Vintrp { src, .. } = f else {
            panic!("not VINTRP");
        };
        assert_eq!(src, Operand::Const(1));
    }

    #[test]
    fn lds_pairs_double_the_result_and_split_offsets() {
        let (name, f) = fields(&[0xD8DC_0703, 0x0400_0001]);
        assert_eq!(name, "ds_read2_b32");
        let want = Fields::Ds {
            dst: Some(vgpr(4, 2)),
            addr: Some(vgpr(1, 1)),
            data: vec![],
            offset0: 3,
            offset1: 7,
            gds: false,
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xD838_0000 | 1 << 17, 0x0003_0201]);
        assert_eq!(name, "ds_write2_b32");
        let want = Fields::Ds {
            dst: None,
            addr: Some(vgpr(1, 1)),
            data: vec![vgpr(2, 1), vgpr(3, 1)],
            offset0: 0,
            offset1: 0,
            gds: true,
        };
        assert_eq!(f, want);
    }

    #[test]
    fn typed_and_untyped_buffers_decode_addressing() {
        let (name, f) = fields(&[0xE030_7123, 0x8084_0602]);
        assert_eq!(name, "buffer_load_dword");
        let want = Fields::Buffer {
            data: Some(vgpr(6, 2)),
            addr: vgpr(2, 2),
            rsrc: sgpr(16, 4),
            soffset: Operand::Const(128),
            offset: 0x123,
            offen: true,
            idxen: true,
            format: None,
            lds: false,
            tfe: true,
            cache: Cache {
                glc: true,
                slc: false,
                dlc: false,
            },
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xE928_0005, 0x8001_0201]);
        assert_eq!(name, "tbuffer_load_format_x");
        let Fields::Buffer {
            data,
            rsrc,
            format,
            offset,
            lds,
            ..
        } = f
        else {
            panic!("not a buffer instruction");
        };
        assert_eq!((data, rsrc), (Some(vgpr(2, 1)), sgpr(4, 4)));
        assert_eq!((format, offset, lds), (Some(0x25), 5, false));
    }

    #[test]
    fn image_sequential_address_range_follows_a16_and_g16() {
        // (opcode, a16, address dwords): image_sample_d_g16 2D.
        for (op, a16, count) in [
            (0x22, false, 6),
            (0x22, true, 5),
            (0xA2, false, 4),
            (0xA2, true, 3),
        ] {
            let words = mimg(op, 1, 0xF, a16, 0);
            let (_, f) = fields(&words);
            let Fields::Image {
                data,
                addr,
                rsrc,
                sampler,
                dmask,
                dim,
                a16: got,
                ..
            } = f
            else {
                panic!("not an image instruction");
            };
            assert_eq!(addr, [vgpr(8, count)], "op {op:#x} a16 {a16}");
            assert_eq!((data, rsrc), (vgpr(12, 4), sgpr(12, 8)));
            assert_eq!((sampler, dmask, dim, got), (Some(sgpr(8, 4)), 0xF, 1, a16));
        }
    }

    #[test]
    fn image_nsa_gives_one_register_per_packed_address() {
        // image_sample_d_o_g16 3D with a16: offset, 4 gradient dwords, 2 coordinate dwords.
        let [w0, w1] = mimg(0xB2, 2, 0xF, true, 2);
        let words = [
            w0,
            w1 & !0xFF | 19,
            20 | 21 << 8 | 22 << 16 | 23 << 24,
            24 | 25 << 8,
        ];
        let (name, f) = fields(&words);
        assert_eq!(name, "image_sample_d_o_g16");
        let Fields::Image { addr, a16, .. } = f else {
            panic!("not an image instruction");
        };
        assert!(a16);
        assert_eq!(addr, (19..26).map(|r| vgpr(r, 1)).collect::<Vec<_>>());
    }

    #[test]
    fn flat_global_and_scratch_decode_segment_forms() {
        let (name, f) = fields(&[0xDC31_8FFC, 0x0506_0002]);
        assert_eq!(name, "global_load_dword");
        let want = Fields::Flat {
            dst: Some(vgpr(5, 1)),
            addr: Some(vgpr(2, 1)),
            data: None,
            saddr: Some(sgpr(6, 2)),
            offset: -4,
            lds: false,
            cache: Cache {
                glc: true,
                slc: false,
                dlc: false,
            },
        };
        assert_eq!(f, want);
        let (name, f) = fields(&[0xDC30_0010, 0x057D_0002]);
        assert_eq!(name, "flat_load_dword");
        let Fields::Flat {
            addr,
            saddr,
            offset,
            ..
        } = f
        else {
            panic!("not FLAT");
        };
        assert_eq!((addr, saddr, offset), (Some(vgpr(2, 2)), None, 16));
        let (name, f) = fields(&[0xDC70_4000, 0x007D_0403]);
        assert_eq!(name, "scratch_store_dword");
        let want = Fields::Flat {
            dst: None,
            addr: Some(vgpr(3, 1)),
            data: Some(vgpr(4, 1)),
            saddr: Some(sgpr(125, 1)),
            offset: 0,
            lds: false,
            cache: Cache::default(),
        };
        assert_eq!(f, want);
    }

    #[test]
    fn exports_decode_enabled_channels_and_compression() {
        let (name, f) = fields(&[0xF800_18C5, 0x000B_000A]);
        assert_eq!(name, "exp");
        let want = Fields::Exp {
            target: 12,
            src: [Some(vgpr(10, 1)), None, Some(vgpr(11, 1)), None],
            compr: false,
            done: true,
            vm: true,
        };
        assert_eq!(f, want);
        let (_, f) = fields(&[0xF800_040F, 0x0403]);
        let want = Fields::Exp {
            target: 0,
            src: [
                Some(vgpr(3, 1)),
                Some(vgpr(3, 1)),
                Some(vgpr(4, 1)),
                Some(vgpr(4, 1)),
            ],
            compr: true,
            done: false,
            vm: false,
        };
        assert_eq!(f, want);
    }
}
