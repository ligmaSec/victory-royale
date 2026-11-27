//! Instruction lowering and CFG finalization for the shader lowerer.

use super::*;

impl Lower<'_> {
    /// Source `k` of a 16-bit operation in the low 16 bits (the rest 0):
    /// op_sel (or an SDWA word select) picks the high half; inline float
    /// constants are f16 values here.
    fn half_src(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let mut high = v.opsel >> k & 1 != 0;
        if let Some(sd) = v.sdwa {
            match sd.src_sel.get(k).copied().unwrap_or(6) {
                4 => {}
                5 => high = true,
                6 => {}
                _ => return Err("a byte select on a 16-bit operation"),
            }
        }
        let x = match self.src0 {
            Some(x) if k == 0 => x,
            _ => self.f16_operand(inst, &op)?,
        };
        let c = self.cur;
        let x = if high {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// An f16 source as an f32 (exact), with its abs/neg modifiers.
    fn half_fsrc(&mut self, inst: &Inst, k: usize) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let h = self.half_src(inst, k)?;
        let f32t = self.s.t.f32;
        let mut f = self
            .s
            .op(self.cur, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
        if v.abs >> k & 1 != 0 {
            f = self.unary(6, f);
        }
        if v.neg >> k & 1 != 0 {
            let z = self.s.m.f32(0x8000_0000);
            f = self.s.m.bin(self.cur, Bin::Sub, z, f);
        }
        Ok(f)
    }

    /// Writes a 16-bit result (low bits of `x`) into the destination's low
    /// half, or its high half with op_sel bit 3 (or an SDWA WORD_1 select);
    /// the other half is kept (GFX10).
    fn write_half(&mut self, inst: &Inst, x: V, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a 16-bit result outside a VGPR");
        };
        let mut high = v.opsel >> 3 & 1 != 0;
        // SDWA: the word written, the other one kept (preserve), bit 15
        // copied upward (sext: dst_unused 1 with WORD_0), or zero (pad; a sign
        // extension above the high word or a whole dword adds nothing). The
        // reserved dst_unused 3 is refused. (Upstream VIctory #3124: a 16-bit
        // -1 written with sext came out as 0x0000FFFF.)
        let mut keep = true;
        let mut sext = false;
        if let Some(sd) = v.sdwa {
            match (sd.dst_sel, sd.dst_unused) {
                (4, 2) => {}
                (5, 2) => high = true,
                (4, 1) => (keep, sext) = (false, true),
                (4, 0) | (6, 0..=2) => keep = false,
                (5, 0 | 1) => (high, keep) = (true, false),
                _ => return Err("an SDWA destination of a 16-bit operation"),
            }
        }
        let c = self.cur;
        let old = match inst.read(Reg::V(reg)).map(|o| self.i32_of(o)) {
            Some(Ok(o)) if keep => o,
            _ => self.s.m.i32(0),
        };
        let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
        let x = self.s.m.bin(c, Bin::And, x, low);
        let r = if sext {
            let t = self.s.m.bin(c, Bin::Shl, x, k16);
            self.s.m.bin(c, Bin::AShr, t, k16)
        } else if high {
            let o = self.s.m.bin(c, Bin::And, old, low);
            let x = self.s.m.bin(c, Bin::Shl, x, k16);
            self.s.m.bin(c, Bin::Or, o, x)
        } else {
            let keep = self.s.m.i32(0xFFFF_0000);
            let o = self.s.m.bin(c, Bin::And, old, keep);
            self.s.m.bin(c, Bin::Or, o, x)
        };
        self.write_placed(inst, Reg::V(reg), r, guard);
        Ok(())
    }

    /// 16-bit operations, computed in f32 or i32: f16 sums, differences and
    /// products are exact there and round once to f16 (f32 has the 2 x 11 +
    /// 2 bits that make double rounding harmless); `None` if `name` is none.
    pub(super) fn valu16(
        &mut self,
        inst: &Inst,
        name: &str,
        guard: Option<V>,
    ) -> Option<Result<(), Why>> {
        const FLOAT2: &[&str] = &[
            "v_add_f16",
            "v_sub_f16",
            "v_subrev_f16",
            "v_mul_f16",
            "v_max_f16",
            "v_min_f16",
        ];
        const FLOAT3: &[&str] = &[
            "v_fma_f16",
            "v_mad_f16",
            "v_fmac_f16",
            "v_mac_f16",
            "v_max3_f16",
            "v_min3_f16",
            "v_med3_f16",
        ];
        const FLOAT1: &[&str] = &[
            "v_rcp_f16",
            "v_sqrt_f16",
            "v_rsq_f16",
            "v_log_f16",
            "v_exp_f16",
            "v_fract_f16",
            "v_trunc_f16",
            "v_floor_f16",
            "v_ceil_f16",
            "v_rndne_f16",
            "v_sin_f16",
            "v_cos_f16",
            "v_cvt_f16_u16",
            "v_cvt_f16_i16",
        ];
        const INT: &[&str] = &[
            "v_add_nc_u16",
            "v_sub_nc_u16",
            "v_subrev_nc_u16",
            "v_mul_lo_u16",
            "v_lshlrev_b16",
            "v_lshrrev_b16",
            "v_ashrrev_i16",
            "v_max_u16",
            "v_max_i16",
            "v_min_u16",
            "v_min_i16",
            "v_cvt_u16_f16",
            "v_cvt_i16_f16",
            "v_cvt_norm_i16_f16",
            "v_cvt_norm_u16_f16",
        ];
        let all = [FLOAT1, FLOAT2, FLOAT3, INT];
        if !all.iter().any(|l| l.contains(&name))
            && !matches!(name, "v_cvt_f16_f32" | "v_cvt_f32_f16")
        {
            return None;
        }
        Some(self.valu16_op(inst, name, guard, FLOAT1, FLOAT2, FLOAT3))
    }

    #[allow(clippy::too_many_arguments)]
    fn valu16_op(
        &mut self,
        inst: &Inst,
        name: &str,
        guard: Option<V>,
        float1: &[&str],
        float2: &[&str],
        float3: &[&str],
    ) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let c = self.cur;
        let i32t = self.s.t.i32;
        let f32t = self.s.t.f32;
        if name == "v_cvt_f32_f16" {
            let f = self.half_fsrc(inst, 0)?;
            return self.write_f(inst, f, guard);
        }
        let float_result = |l: &mut Self, mut f: V| -> Result<(), Why> {
            match v.omod {
                0 => {}
                k => {
                    let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(k)];
                    let k = l.s.m.f32(factor.to_bits());
                    f = l.s.m.bin(l.cur, Bin::Mul, f, k);
                }
            }
            if v.clamp {
                f = l.unary(7, f);
            }
            // The console rounds to nearest; the host's conversion may round toward zero
            // (NVIDIA's does), one step of the last place lower where the two differ.
            // shortcut: the host's conversion unless `VICTORY_F16_NEAREST=1`
            // ([`f16_nearest_on`]), which costs a dozen instructions per result.
            let h = if f16_nearest_on() {
                l.f16_nearest(f)
            } else {
                l.s.op(l.cur, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            };
            l.write_half(inst, h, guard)
        };
        if name == "v_cvt_f16_f32" {
            let f = self.fsrc(inst, 0)?;
            return float_result(self, f);
        }
        if float2.contains(&name) {
            let (a, b) = (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?);
            let r = match name {
                "v_add_f16" => self.s.m.bin(c, Bin::Add, a, b),
                "v_sub_f16" => self.s.m.bin(c, Bin::Sub, a, b),
                "v_subrev_f16" => self.s.m.bin(c, Bin::Sub, b, a),
                "v_mul_f16" => self.s.m.bin(c, Bin::Mul, a, b),
                "v_max_f16" => self.max_min(true, a, b),
                _ => self.max_min(false, a, b),
            };
            return float_result(self, r);
        }
        if float3.contains(&name) {
            let (a, b) = (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?);
            let acc = if matches!(name, "v_fmac_f16" | "v_mac_f16") {
                // The accumulator is the destination's half that is written.
                let Some(Operand::Vector { reg, .. }) = v.dst else {
                    return Err("an accumulator");
                };
                let x = self.i32_of(inst.read(Reg::V(reg)).ok_or("an accumulator")?)?;
                let x = if v.opsel >> 3 & 1 != 0 {
                    let k16 = self.s.m.i32(16);
                    self.s.m.bin(c, Bin::LShr, x, k16)
                } else {
                    x
                };
                let m = self.s.m.i32(0xFFFF);
                let x = self.s.m.bin(c, Bin::And, x, m);
                self.s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x])
            } else {
                self.half_fsrc(inst, 2)?
            };
            let r = match name {
                // shortcut: fused f16 multiply-adds as FMad in f32 (the
                // product is exact; the sum rounds twice).
                "v_fma_f16" | "v_fmac_f16" | "v_mad_f16" | "v_mac_f16" => self.fmad(a, b, acc),
                "v_max3_f16" => {
                    let t = self.max_min(true, a, b);
                    self.max_min(true, t, acc)
                }
                "v_min3_f16" => {
                    let t = self.max_min(false, a, b);
                    self.max_min(false, t, acc)
                }
                _ => self.med3(a, b, acc),
            };
            return float_result(self, r);
        }
        if float1.contains(&name) {
            let r = match name {
                "v_cvt_f16_u16" | "v_cvt_f16_i16" => {
                    let mut x = self.half_src(inst, 0)?;
                    let op = if name == "v_cvt_f16_i16" {
                        let k16 = self.s.m.i32(16);
                        let t = self.s.m.bin(c, Bin::Shl, x, k16);
                        x = self.s.m.bin(c, Bin::AShr, t, k16);
                        Cast::SiToFp
                    } else {
                        Cast::UiToFp
                    };
                    self.s.m.cast(c, op, x, f32t)
                }
                "v_rcp_f16" => {
                    let a = self.half_fsrc(inst, 0)?;
                    let one = self.s.m.f32(1.0f32.to_bits());
                    self.s.m.bin(c, Bin::SDiv, one, a)
                }
                _ => {
                    let a = self.half_fsrc(inst, 0)?;
                    let op = match name {
                        "v_sqrt_f16" => 24,
                        "v_rsq_f16" => 25,
                        "v_log_f16" => 23,
                        "v_exp_f16" => 21,
                        "v_fract_f16" => 22,
                        "v_trunc_f16" => 29,
                        "v_floor_f16" => 27,
                        "v_ceil_f16" => 28,
                        "v_rndne_f16" => 26,
                        "v_sin_f16" => 13,
                        _ => 12,
                    };
                    match op {
                        12 | 13 => self.trig_turns(a, op == 12),
                        // Any f16 in (-2^-11, 0) has fract >= 1 - 2^-12; clamp to 0x3bff (1 - 2^-11).
                        22 => self.fract(a, 1.0 - 1.0 / 4096.0, 1.0 - 1.0 / 2048.0),
                        _ => self.unary(op, a),
                    }
                }
            };
            return float_result(self, r);
        }
        if matches!(name, "v_cvt_u16_f16" | "v_cvt_i16_f16") {
            if v.clamp || v.omod != 0 {
                return Err("modifiers on a 16-bit integer operation");
            }
            // Toward zero, saturating at the 16-bit range (infinities
            // included); NaN gives 0. Clamped in f32 first, where every
            // f16 is exact, so the conversion never leaves i32's range
            // (upstream PR #384's arithmetic; the source's abs and neg
            // modifiers are applied by `half_fsrc`, as before).
            let a = self.half_fsrc(inst, 0)?;
            let (lo, hi) = if name == "v_cvt_u16_f16" {
                (0.0f32, 65535.0f32)
            } else {
                (-32768.0, 32767.0)
            };
            let (lo, hi) = (self.s.m.f32(lo.to_bits()), self.s.m.f32(hi.to_bits()));
            let t = self.binary(35, a, lo);
            let t = self.binary(36, t, hi);
            let r = self.s.m.cast(c, Cast::FpToSi, t, i32t);
            let nan = self.s.m.cmp(c, pred::UNO, a, a);
            let z = self.s.m.i32(0);
            let r = self.s.m.select(c, nan, z, r);
            return self.write_half(inst, r, guard);
        }
        if matches!(name, "v_cvt_norm_i16_f16" | "v_cvt_norm_u16_f16") {
            if v.clamp || v.omod != 0 {
                return Err("modifiers on a 16-bit integer operation");
            }
            let r = self.cvt_norm16(inst, name == "v_cvt_norm_i16_f16")?;
            return self.write_half(inst, r, guard);
        }
        // 16-bit integers: computed on the low halves, the low 16 bits kept.
        if v.clamp || v.omod != 0 || v.abs != 0 || v.neg != 0 {
            return Err("modifiers on a 16-bit integer operation");
        }
        let r = {
            let (mut a, mut b) = (self.half_src(inst, 0)?, self.half_src(inst, 1)?);
            let k16 = self.s.m.i32(16);
            let sext = |l: &mut Self, x: V| {
                let t = l.s.m.bin(c, Bin::Shl, x, k16);
                l.s.m.bin(c, Bin::AShr, t, k16)
            };
            if name.ends_with("_i16") {
                a = sext(self, a);
                b = sext(self, b);
            }
            let k15 = self.s.m.i32(15);
            match name {
                "v_add_nc_u16" => self.s.m.bin(c, Bin::Add, a, b),
                "v_sub_nc_u16" => self.s.m.bin(c, Bin::Sub, a, b),
                "v_subrev_nc_u16" => self.s.m.bin(c, Bin::Sub, b, a),
                "v_mul_lo_u16" => self.s.m.bin(c, Bin::Mul, a, b),
                "v_lshlrev_b16" | "v_lshrrev_b16" | "v_ashrrev_i16" => {
                    // src0 is the shift (4 bits), src1 the value.
                    let sh = self.s.m.bin(c, Bin::And, a, k15);
                    let x = if name == "v_ashrrev_i16" {
                        sext(self, b)
                    } else {
                        b
                    };
                    let op = match name {
                        "v_lshlrev_b16" => Bin::Shl,
                        "v_lshrrev_b16" => Bin::LShr,
                        _ => Bin::AShr,
                    };
                    self.s.m.bin(c, op, x, sh)
                }
                _ => {
                    let p = match name {
                        "v_max_u16" => pred::IUGT,
                        "v_min_u16" => pred::IULT,
                        "v_max_i16" => pred::SGT,
                        _ => pred::SLT,
                    };
                    let t = self.s.m.cmp(c, p, a, b);
                    self.s.m.select(c, t, a, b)
                }
            }
        };
        self.write_half(inst, r, guard)
    }

    /// `v_cvt_norm_i16_f16` / `v_cvt_norm_u16_f16`: the f16 source (with its
    /// abs/neg modifiers) clamped to [-1, 1] or [0, 1], times 32767 or 65535,
    /// rounded to nearest even once from the exact product; NaN gives 0.
    /// Worked on the f16's bits in integers, because in f32 the product
    /// (up to 26 significant bits) would round before the rounding to an
    /// integer.
    fn cvt_norm16(&mut self, inst: &Inst, signed: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let c = self.cur;
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let mut h = self.half_src(inst, 0)?;
        if v.abs & 1 != 0 {
            let m = k(self, 0x7FFF);
            h = self.s.m.bin(c, Bin::And, h, m);
        }
        if v.neg & 1 != 0 {
            let m = k(self, 0x8000);
            h = self.s.m.bin(c, Bin::Xor, h, m);
        }
        let scale = if signed { 0x7FFF } else { 0xFFFF };
        let (k0, k1, k10) = (k(self, 0), k(self, 1), k(self, 10));
        let mag_mask = k(self, 0x7FFF);
        let mag = self.s.m.bin(c, Bin::And, h, mag_mask);
        let k15 = k(self, 15);
        let neg = self.s.m.bin(c, Bin::LShr, h, k15);
        let neg = self.s.m.cmp(c, pred::NE, neg, k0);
        // |x| < 1: x = m * 2^-s with m < 2^11 and 11 <= s <= 24.
        let exp = self.s.m.bin(c, Bin::LShr, mag, k10);
        let man_mask = k(self, 0x3FF);
        let man = self.s.m.bin(c, Bin::And, mag, man_mask);
        let implicit = k(self, 0x400);
        let normal = self.s.m.bin(c, Bin::Or, man, implicit);
        let sub = self.s.m.cmp(c, pred::EQ, exp, k0);
        let m = self.s.m.select(c, sub, man, normal);
        let k25 = k(self, 25);
        let s_norm = self.s.m.bin(c, Bin::Sub, k25, exp);
        let k24 = k(self, 24);
        let sh = self.s.m.select(c, sub, k24, s_norm);
        let ks = k(self, scale);
        let p = self.s.m.bin(c, Bin::Mul, m, ks);
        let q = self.s.m.bin(c, Bin::LShr, p, sh);
        let one_s = self.s.m.bin(c, Bin::Shl, k1, sh);
        let low_mask = self.s.m.bin(c, Bin::Sub, one_s, k1);
        let rem = self.s.m.bin(c, Bin::And, p, low_mask);
        let half = self.s.m.bin(c, Bin::LShr, one_s, k1);
        let above = self.s.m.cmp(c, pred::IUGT, rem, half);
        let tie = self.s.m.cmp(c, pred::EQ, rem, half);
        let odd = self.s.m.bin(c, Bin::And, q, k1);
        let odd = self.s.m.cmp(c, pred::NE, odd, k0);
        let tie_up = self.s.m.bin(c, Bin::And, tie, odd);
        let up = self.s.m.bin(c, Bin::Or, above, tie_up);
        let r = self.s.m.select(c, up, k1, k0);
        let r = self.s.m.bin(c, Bin::Add, q, r);
        // |x| >= 1 (and infinity) saturates; NaN gives 0.
        let one_h = k(self, 0x3C00);
        let big = self.s.m.cmp(c, pred::IUGE, mag, one_h);
        let r = self.s.m.select(c, big, ks, r);
        let inf = k(self, 0x7C00);
        let nan = self.s.m.cmp(c, pred::IUGT, mag, inf);
        let r = self.s.m.select(c, nan, k0, r);
        let r = if signed {
            let n = self.s.m.bin(c, Bin::Sub, k0, r);
            self.s.m.select(c, neg, n, r)
        } else {
            self.s.m.select(c, neg, k0, r)
        };
        let m16 = k(self, 0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, r, m16))
    }

    /// Half `high` of source `k` of a packed (VOP3P) operation, in the low
    /// 16 bits: op_sel picks the half the low result reads, op_sel_hi the
    /// half the high result reads. Packed integer instructions see the f32
    /// bits of an inline float constant.
    fn packed_src(&mut self, inst: &Inst, k: usize, high: bool, float: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let sel = if high { v.opsel_hi } else { v.opsel } >> k & 1 != 0;
        let x = if float {
            self.f16_operand(inst, &op)?
        } else {
            self.operand(inst, &op)?
        };
        let c = self.cur;
        let x = if sel {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// Half `high` of source `k` of `v_pk_fmac_f16`, in the low 16 bits: a
    /// VOP2 operation (no op_sel) whose third source is its destination.
    fn fmac_half(&mut self, inst: &Inst, k: usize, high: bool) -> Result<V, Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = if k == 2 { v.dst } else { v.src.get(k).copied() };
        let op = op.ok_or("a missing operand")?;
        if matches!(op, Operand::Const(_) | Operand::Literal(_)) {
            // shortcut: no constant operand seen; its high half is not known.
            return Err("a constant operand of v_pk_fmac_f16");
        }
        let x = self.operand(inst, &op)?;
        let c = self.cur;
        let x = if high {
            let k16 = self.s.m.i32(16);
            self.s.m.bin(c, Bin::LShr, x, k16)
        } else {
            x
        };
        let m = self.s.m.i32(0xFFFF);
        Ok(self.s.m.bin(c, Bin::And, x, m))
    }

    /// Packed 16-bit operations (`v_pk_*`): each half computed as the
    /// 16-bit operation does, f16 through f32 as in [`Self::valu16`].
    pub(super) fn packed16(&mut self, inst: &Inst, op: &str, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let float = op.ends_with("_f16");
        let n = match op {
            "fma_f16" | "fmac_f16" | "mad_u16" | "mad_i16" => 3,
            "add_f16" | "mul_f16" | "min_f16" | "max_f16" | "add_u16" | "sub_u16" | "add_i16"
            | "sub_i16" | "mul_lo_u16" | "lshlrev_b16" | "lshrrev_b16" | "ashrrev_i16"
            | "max_u16" | "min_u16" | "max_i16" | "min_i16" => 2,
            _ => return Err("a packed 16-bit operation"),
        };
        if v.omod != 0 || !float && (v.clamp || v.neg != 0 || v.neg_hi != 0) {
            return Err("modifiers on a packed integer operation");
        }
        let c = self.cur;
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let mut halves = Vec::new();
        for high in [false, true] {
            let mut xs = Vec::new();
            for k in 0..n {
                let h = if op == "fmac_f16" {
                    self.fmac_half(inst, k, high)?
                } else {
                    self.packed_src(inst, k, high, float)?
                };
                if float {
                    let mut f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    let neg = if high { v.neg_hi } else { v.neg };
                    if neg >> k & 1 != 0 {
                        let z = self.s.m.f32(0x8000_0000);
                        f = self.s.m.bin(c, Bin::Sub, z, f);
                    }
                    xs.push(f);
                } else {
                    xs.push(h);
                }
            }
            let r = if float {
                let mut f = match op {
                    "add_f16" => self.s.m.bin(c, Bin::Add, xs[0], xs[1]),
                    "mul_f16" => self.s.m.bin(c, Bin::Mul, xs[0], xs[1]),
                    "min_f16" => self.max_min(false, xs[0], xs[1]),
                    "max_f16" => self.max_min(true, xs[0], xs[1]),
                    // shortcut: FMad in f32, as v_fma_f16.
                    _ => self.fmad(xs[0], xs[1], xs[2]),
                };
                if v.clamp {
                    f = self.unary(7, f);
                }
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            } else {
                let k16 = self.s.m.i32(16);
                let sext = |l: &mut Self, x: V| {
                    let t = l.s.m.bin(c, Bin::Shl, x, k16);
                    l.s.m.bin(c, Bin::AShr, t, k16)
                };
                if op.ends_with("_i16") {
                    for x in &mut xs {
                        *x = sext(self, *x);
                    }
                }
                let k15 = self.s.m.i32(15);
                match op {
                    "add_u16" | "add_i16" => self.s.m.bin(c, Bin::Add, xs[0], xs[1]),
                    "sub_u16" | "sub_i16" => self.s.m.bin(c, Bin::Sub, xs[0], xs[1]),
                    "mul_lo_u16" => self.s.m.bin(c, Bin::Mul, xs[0], xs[1]),
                    "mad_u16" | "mad_i16" => {
                        let m = self.s.m.bin(c, Bin::Mul, xs[0], xs[1]);
                        self.s.m.bin(c, Bin::Add, m, xs[2])
                    }
                    "lshlrev_b16" | "lshrrev_b16" | "ashrrev_i16" => {
                        let sh = self.s.m.bin(c, Bin::And, xs[0], k15);
                        let o = match op {
                            "lshlrev_b16" => Bin::Shl,
                            "lshrrev_b16" => Bin::LShr,
                            _ => Bin::AShr,
                        };
                        let x = if op == "ashrrev_i16" {
                            sext(self, xs[1])
                        } else {
                            xs[1]
                        };
                        self.s.m.bin(c, o, x, sh)
                    }
                    _ => {
                        let p = match op {
                            "max_u16" => pred::IUGT,
                            "min_u16" => pred::IULT,
                            "max_i16" => pred::SGT,
                            _ => pred::SLT,
                        };
                        let t = self.s.m.cmp(c, p, xs[0], xs[1]);
                        self.s.m.select(c, t, xs[0], xs[1])
                    }
                }
            };
            let m = self.s.m.i32(0xFFFF);
            halves.push(self.s.m.bin(c, Bin::And, r, m));
        }
        let k16 = self.s.m.i32(16);
        let hi = self.s.m.bin(c, Bin::Shl, halves[1], k16);
        let r = self.s.m.bin(c, Bin::Or, halves[0], hi);
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a packed result outside a VGPR");
        };
        self.write(inst, Reg::V(reg), r, guard);
        Ok(())
    }

    pub(super) fn integer(&mut self, inst: &Inst, name: &'static str) -> Result<V, Why> {
        let c = self.cur;
        let s = |l: &mut Self, k| l.src(inst, k);
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let m31 = |l: &mut Self, x: V| {
            let k31 = l.s.m.i32(31);
            l.s.m.bin(c, Bin::And, x, k31)
        };
        Ok(match name {
            "v_and_b32" | "v_or_b32" | "v_xor_b32" | "v_add_nc_u32" | "v_sub_nc_u32"
            | "v_subrev_nc_u32" | "v_mul_lo_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (op, a, b) = match name {
                    "v_and_b32" => (Bin::And, a, b),
                    "v_or_b32" => (Bin::Or, a, b),
                    "v_xor_b32" => (Bin::Xor, a, b),
                    "v_add_nc_u32" => (Bin::Add, a, b),
                    "v_sub_nc_u32" => (Bin::Sub, a, b),
                    "v_subrev_nc_u32" => (Bin::Sub, b, a),
                    _ => (Bin::Mul, a, b),
                };
                self.s.m.bin(c, op, a, b)
            }
            "v_xnor_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let x = self.s.m.bin(c, Bin::Xor, a, b);
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, x, ones)
            }
            "v_not_b32" => {
                let a = s(self, 0)?;
                let ones = k(self, u32::MAX);
                self.s.m.bin(c, Bin::Xor, a, ones)
            }
            "v_lshlrev_b32" | "v_lshrrev_b32" | "v_ashrrev_i32" => {
                let (sh, a) = (s(self, 0)?, s(self, 1)?);
                let sh = m31(self, sh);
                let op = match name {
                    "v_lshlrev_b32" => Bin::Shl,
                    "v_lshrrev_b32" => Bin::LShr,
                    _ => Bin::AShr,
                };
                self.s.m.bin(c, op, a, sh)
            }
            "v_lshl_add_u32" | "v_lshl_or_b32" | "v_add_lshl_u32" | "v_and_or_b32"
            | "v_or3_b32" | "v_xad_u32" | "v_add3_u32" | "v_xor3_b32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                match name {
                    "v_lshl_add_u32" | "v_lshl_or_b32" => {
                        let b = m31(self, b);
                        let t = self.s.m.bin(c, Bin::Shl, a, b);
                        let op = if name == "v_lshl_add_u32" {
                            Bin::Add
                        } else {
                            Bin::Or
                        };
                        self.s.m.bin(c, op, t, d)
                    }
                    "v_add_lshl_u32" => {
                        let t = self.s.m.bin(c, Bin::Add, a, b);
                        let d = m31(self, d);
                        self.s.m.bin(c, Bin::Shl, t, d)
                    }
                    _ => {
                        let (o1, o2) = match name {
                            "v_and_or_b32" => (Bin::And, Bin::Or),
                            "v_or3_b32" => (Bin::Or, Bin::Or),
                            "v_xad_u32" => (Bin::Xor, Bin::Add),
                            "v_add3_u32" => (Bin::Add, Bin::Add),
                            _ => (Bin::Xor, Bin::Xor),
                        };
                        let t = self.s.m.bin(c, o1, a, b);
                        self.s.m.bin(c, o2, t, d)
                    }
                }
            }
            "v_mul_u32_u24" | "v_mul_i32_i24" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mul_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.s.m.bin(c, Bin::Mul, a, b)
            }
            "v_mul_hi_u32_u24" | "v_mul_hi_i32_i24" => {
                // The high dword of the same 48-bit product: the low 24 bits
                // of each operand, zero- or sign-extended, multiplied.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let signed = name == "v_mul_hi_i32_i24";
                let (sh, op) = (k(self, 8), if signed { Bin::AShr } else { Bin::LShr });
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                self.mul_hi(a, b, signed)
            }
            "v_bfe_u32" | "v_bfe_i32" => {
                let (a, off, w) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let w = m31(self, w);
                self.bfe(a, off, w, name == "v_bfe_i32")
            }
            "v_bfi_b32" => {
                let (m, a, b) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let ones = k(self, u32::MAX);
                let nm = self.s.m.bin(c, Bin::Xor, m, ones);
                let x = self.s.m.bin(c, Bin::And, m, a);
                let y = self.s.m.bin(c, Bin::And, nm, b);
                self.s.m.bin(c, Bin::Or, x, y)
            }
            "v_max_i32" | "v_min_i32" | "v_max_u32" | "v_min_u32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let p = match name {
                    "v_max_i32" => pred::SGT,
                    "v_min_i32" => pred::SLT,
                    "v_max_u32" => pred::IUGT,
                    _ => pred::IULT,
                };
                let t = self.s.m.cmp(c, p, a, b);
                self.s.m.select(c, t, a, b)
            }
            "v_mul_hi_u32" | "v_mul_hi_i32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                self.mul_hi(a, b, name == "v_mul_hi_i32")
            }
            "v_alignbit_b32" | "v_alignbyte_b32" => {
                // The low dword of {a, b} >> shift.
                let (a, b, sh) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let sh = if name == "v_alignbyte_b32" {
                    let three = k(self, 3);
                    let t = self.s.m.bin(c, Bin::And, sh, three);
                    let k3 = k(self, 3);
                    self.s.m.bin(c, Bin::Shl, t, k3)
                } else {
                    m31(self, sh)
                };
                let k32 = k(self, 32);
                let lo = self.s.m.bin(c, Bin::LShr, b, sh);
                let left = self.s.m.bin(c, Bin::Sub, k32, sh);
                let left = m31(self, left);
                let hi = self.s.m.bin(c, Bin::Shl, a, left);
                let zero = k(self, 0);
                let none = self.s.m.cmp(c, pred::EQ, sh, zero);
                let hi = self.s.m.select(c, none, zero, hi);
                self.s.m.bin(c, Bin::Or, lo, hi)
            }
            "v_bfrev_b32" => {
                let a = s(self, 0)?;
                self.s
                    .op(c, "unary.i32", Attr::ReadNone, self.s.t.i32, 30, &[a])
            }
            "v_bcnt_u32_b32" => {
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[a]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            "v_mad_u32_u24" | "v_mad_i32_i24" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (sh, op) = (
                    k(self, 8),
                    if name == "v_mad_i32_i24" {
                        Bin::AShr
                    } else {
                        Bin::LShr
                    },
                );
                let a = self.s.m.bin(c, Bin::Shl, a, sh);
                let a = self.s.m.bin(c, op, a, sh);
                let b = self.s.m.bin(c, Bin::Shl, b, sh);
                let b = self.s.m.bin(c, op, b, sh);
                let m = self.s.m.bin(c, Bin::Mul, a, b);
                self.s.m.bin(c, Bin::Add, m, d)
            }
            "v_sad_u8" | "v_sad_hi_u8" | "v_sad_u16" | "v_sad_u32" | "v_msad_u8" => {
                // src2 plus the sum of the unsigned differences |a - b| of
                // the operands' bytes, halves or whole dwords, wrapping at 32
                // bits. v_sad_hi_u8 adds the sum shifted to the high half;
                // v_msad_u8 leaves out a byte whose mask byte (src1) is 0.
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let (width, parts) = match name {
                    "v_sad_u16" => (16, 2),
                    "v_sad_u32" => (32, 1),
                    _ => (8, 4),
                };
                let zero = k(self, 0);
                let mut sum = zero;
                for i in 0..parts {
                    let (mut x, mut y) = (a, b);
                    if width < 32 {
                        let (sh, mask) = (k(self, width * i), k(self, (1 << width) - 1));
                        x = self.s.m.bin(c, Bin::LShr, x, sh);
                        x = self.s.m.bin(c, Bin::And, x, mask);
                        y = self.s.m.bin(c, Bin::LShr, y, sh);
                        y = self.s.m.bin(c, Bin::And, y, mask);
                    }
                    let above = self.s.m.cmp(c, pred::IUGT, x, y);
                    let hi = self.s.m.select(c, above, x, y);
                    let lo = self.s.m.select(c, above, y, x);
                    let mut diff = self.s.m.bin(c, Bin::Sub, hi, lo);
                    if name == "v_msad_u8" {
                        let masked = self.s.m.cmp(c, pred::NE, y, zero);
                        diff = self.s.m.select(c, masked, diff, zero);
                    }
                    sum = self.s.m.bin(c, Bin::Add, sum, diff);
                }
                if name == "v_sad_hi_u8" {
                    let k16 = k(self, 16);
                    sum = self.s.m.bin(c, Bin::Shl, sum, k16);
                }
                self.s.m.bin(c, Bin::Add, sum, d)
            }
            "v_min3_i32" | "v_max3_i32" | "v_min3_u32" | "v_max3_u32" | "v_med3_i32"
            | "v_med3_u32" => {
                let (a, b, d) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                let signed = name.ends_with("i32");
                let (lt, gt) = if signed {
                    (pred::SLT, pred::SGT)
                } else {
                    (pred::IULT, pred::IUGT)
                };
                let pick = |l: &mut Self, p, x, y| {
                    let t = l.s.m.cmp(c, p, x, y);
                    l.s.m.select(c, t, x, y)
                };
                match &name[2..6] {
                    "min3" => {
                        let t = pick(self, lt, a, b);
                        pick(self, lt, t, d)
                    }
                    "max3" => {
                        let t = pick(self, gt, a, b);
                        pick(self, gt, t, d)
                    }
                    _ => {
                        let lo = pick(self, lt, a, b);
                        let hi = pick(self, gt, a, b);
                        let t = pick(self, lt, hi, d);
                        pick(self, gt, lo, t)
                    }
                }
            }
            "v_perm_b32" => {
                // Byte i of the result is byte S of {src0, src1} (src1 the
                // low dword) for selector byte S below 8; 8-11 repeat the
                // sign of bytes 1 and 3 of src1, then of src0; 12 is 0x00,
                // higher 0xFF.
                let Fields::Valu(v) = &inst.fields else {
                    unreachable!()
                };
                let sel = match v.src.get(2) {
                    Some(&Operand::Literal(x)) => x,
                    Some(&Operand::Const(k)) => crate::resource::inline(k).ok_or(name)?,
                    _ => {
                        // A computed selector: each byte picked at run time.
                        let (a, b, sv) = (s(self, 0)?, s(self, 1)?, s(self, 2)?);
                        let mut r = k(self, 0);
                        for i in 0..4u32 {
                            let (sh, ff) = (k(self, 8 * i), k(self, 0xFF));
                            let sel = self.s.m.bin(c, Bin::LShr, sv, sh);
                            let sel = self.s.m.bin(c, Bin::And, sel, ff);
                            // Select a dword and a byte within it.
                            // Later selects handle codes 8 and above.
                            let four = k(self, 4);
                            let zero = k(self, 0);
                            let high = self.s.m.bin(c, Bin::And, sel, four);
                            let high = self.s.m.cmp(c, pred::NE, high, zero);
                            let word = self.s.m.select(c, high, a, b);
                            let three = k(self, 3);
                            let index = self.s.m.bin(c, Bin::And, sel, three);
                            let bits = self.s.m.bin(c, Bin::Shl, index, three);
                            let x = self.s.m.bin(c, Bin::LShr, word, bits);
                            let mut byte = self.s.m.bin(c, Bin::And, x, ff);
                            // 8-11: the sign of bytes 1, 3 of src1, then of src0.
                            for (n, src, bit) in [(8, b, 15), (9, b, 31), (10, a, 15), (11, a, 31)]
                            {
                                let left = k(self, 31 - bit);
                                let t = self.s.m.bin(c, Bin::Shl, src, left);
                                let k31 = k(self, 31);
                                let t = self.s.m.bin(c, Bin::AShr, t, k31);
                                let t = self.s.m.bin(c, Bin::And, t, ff);
                                let kn = k(self, n);
                                let is = self.s.m.cmp(c, pred::EQ, sel, kn);
                                byte = self.s.m.select(c, is, t, byte);
                            }
                            let (k12, z) = (k(self, 12), k(self, 0));
                            let is12 = self.s.m.cmp(c, pred::EQ, sel, k12);
                            byte = self.s.m.select(c, is12, z, byte);
                            let above = self.s.m.cmp(c, pred::IUGT, sel, k12);
                            byte = self.s.m.select(c, above, ff, byte);
                            let at = k(self, 8 * i);
                            let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                            r = self.s.m.bin(c, Bin::Or, r, byte);
                        }
                        return Ok(r);
                    }
                };
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let mut r = k(self, 0);
                for i in 0..4 {
                    let pick = sel >> (8 * i) & 0xFF;
                    let byte = match pick {
                        0..=7 => {
                            let (src, n) = if pick < 4 { (b, pick) } else { (a, pick - 4) };
                            let sh = k(self, 8 * n);
                            let t = self.s.m.bin(c, Bin::LShr, src, sh);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        8..=11 => {
                            let src = if pick < 10 { b } else { a };
                            let bit = if pick % 2 == 0 { 15 } else { 31 };
                            let left = k(self, 31 - bit);
                            let t = self.s.m.bin(c, Bin::Shl, src, left);
                            let k31 = k(self, 31);
                            let t = self.s.m.bin(c, Bin::AShr, t, k31);
                            let ff = k(self, 0xFF);
                            self.s.m.bin(c, Bin::And, t, ff)
                        }
                        12 => continue,
                        _ => k(self, 0xFF),
                    };
                    let at = k(self, 8 * i);
                    let byte = self.s.m.bin(c, Bin::Shl, byte, at);
                    r = self.s.m.bin(c, Bin::Or, r, byte);
                }
                r
            }
            "v_bfm_b32" => {
                // ((1 << src0[4:0]) - 1) << src1[4:0]
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                let one = k(self, 1);
                let (a, b) = (m31(self, a), m31(self, b));
                let m = self.s.m.bin(c, Bin::Shl, one, a);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                self.s.m.bin(c, Bin::Shl, m, b)
            }
            "v_cvt_pk_u8_f32" => {
                // src0 rounded to nearest even and saturated to a byte, into
                // byte src1[1:0] of src2.
                let f = self.fsrc(inst, 0)?;
                let (b, d) = (s(self, 1)?, s(self, 2)?);
                let f = self.unary(26, f);
                let (zero, max) = (self.s.m.f32(0), self.s.m.f32(255.0f32.to_bits()));
                let f = self.binary(35, f, zero);
                let f = self.binary(36, f, max);
                let x = self.s.m.cast(c, Cast::FpToUi, f, self.s.t.i32);
                let (three, k3) = (k(self, 3), k(self, 3));
                let sh = self.s.m.bin(c, Bin::And, b, three);
                let sh = self.s.m.bin(c, Bin::Shl, sh, k3);
                let x = self.s.m.bin(c, Bin::Shl, x, sh);
                let ff = k(self, 0xFF);
                let hole = self.s.m.bin(c, Bin::Shl, ff, sh);
                let ones = k(self, u32::MAX);
                let hole = self.s.m.bin(c, Bin::Xor, hole, ones);
                let d = self.s.m.bin(c, Bin::And, d, hole);
                self.s.m.bin(c, Bin::Or, d, x)
            }
            "v_ffbl_b32" => {
                // -1 for zero, as DXIL's first-bit-low.
                let a = s(self, 0)?;
                self.s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 32, &[a])
            }
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => {
                // Bits of the mask's low (high) half below this lane, plus src1:
                // with an all-ones mask, the lane index.
                let (a, b) = (s(self, 0)?, s(self, 1)?);
                // Mesh lanes are the console's by construction; pixel and
                // vertex lanes are the host's, as their masks are.
                if self.mask_data == MaskData::Host {
                    // Host lanes: the mask must be one (a ballot) or all lanes.
                    let Fields::Valu(v) = &inst.fields else {
                        unreachable!()
                    };
                    let host = match v.src.first() {
                        Some(&Operand::Const(193)) => true,
                        Some(&Operand::Scalar { code, .. }) => {
                            inst.read(Reg::S(code)).is_some_and(|m| {
                                self.x.st.masks.contains_key(&m) || self.x.st.highs.contains_key(&m)
                            })
                        }
                        _ => false,
                    };
                    if !host {
                        return Err("v_mbcnt of a mask that is no ballot (console lane layout)");
                    }
                    self.s.flags |= flags::WAVE_OPS;
                } else if self.s.mesh.is_none() {
                    self.cross_lane(false)?;
                }
                let lane = self.console_lane();
                let below = if name == "v_mbcnt_lo_u32_b32" {
                    lane
                } else {
                    let (k32, z) = (k(self, 32), k(self, 0));
                    let t = self.s.m.bin(c, Bin::Sub, lane, k32);
                    let low = self.s.m.cmp(c, pred::IULT, lane, k32);
                    self.s.m.select(c, low, z, t)
                };
                // (1 << below) - 1, all ones from 32 on.
                let (one, k32, ones) = (k(self, 1), k(self, 32), k(self, u32::MAX));
                let sh = m31(self, below);
                let m = self.s.m.bin(c, Bin::Shl, one, sh);
                let m = self.s.m.bin(c, Bin::Sub, m, one);
                let full = self.s.m.cmp(c, pred::IUGE, below, k32);
                let m = self.s.m.select(c, full, ones, m);
                let x = self.s.m.bin(c, Bin::And, a, m);
                let n = self
                    .s
                    .op(c, "unaryBits.i32", Attr::ReadNone, self.s.t.i32, 31, &[x]);
                self.s.m.bin(c, Bin::Add, n, b)
            }
            _ => return Err(name),
        })
    }

    pub(super) fn compare(&mut self, inst: &Inst, rest: &str) -> Result<(), Why> {
        let (op, ty) = rest.rsplit_once('_').ok_or("a compare")?;
        let c = self.cur;
        let r = match ty {
            "f32" | "f16" if op == "class" => {
                // Bit k of src1 tests class k: signalling NaN, quiet NaN,
                // -inf, -normal, -denormal, -0, +0, +denormal, +normal, +inf.
                let f16 = ty == "f16";
                let x = if f16 {
                    let Fields::Valu(v) = &inst.fields else {
                        unreachable!()
                    };
                    let mut h = self.half_src(inst, 0)?;
                    if v.abs & 1 != 0 {
                        let m = self.s.m.i32(0x7FFF);
                        h = self.s.m.bin(c, Bin::And, h, m);
                    }
                    if v.neg & 1 != 0 {
                        let s = self.s.m.i32(0x8000);
                        h = self.s.m.bin(c, Bin::Xor, h, s);
                    }
                    h
                } else {
                    let x = self.fsrc(inst, 0)?;
                    self.as_int(x)
                };
                let mask = self.src(inst, 1)?;
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (exp_sh, sign_sh, exp_mask, mant_mask, quiet_bit) = if f16 {
                    (10, 15, 0x1F, 0x03FF, 0x0200)
                } else {
                    (23, 31, 0xFF, 0x7F_FFFF, 0x40_0000)
                };
                let (k_exp, k_sign, k_emax, mant, quiet) = (
                    k(self, exp_sh),
                    k(self, sign_sh),
                    k(self, exp_mask),
                    k(self, mant_mask),
                    k(self, quiet_bit),
                );
                let e = self.s.m.bin(c, Bin::LShr, x, k_exp);
                let e = self.s.m.bin(c, Bin::And, e, k_emax);
                let m = self.s.m.bin(c, Bin::And, x, mant);
                let neg = self.s.m.bin(c, Bin::LShr, x, k_sign);
                let zero = k(self, 0);
                let (e0, emax) = (
                    self.s.m.cmp(c, pred::EQ, e, zero),
                    self.s.m.cmp(c, pred::EQ, e, k_emax),
                );
                let m0 = self.s.m.cmp(c, pred::EQ, m, zero);
                let q = self.s.m.bin(c, Bin::And, x, quiet);
                let q = self.s.m.cmp(c, pred::NE, q, zero);
                // Positive classes 6-9; negative ones are 11 minus those.
                let (normal, inf, zero_c, denorm) =
                    (k(self, 8), k(self, 9), k(self, 6), k(self, 7));
                let t = self.s.m.select(c, m0, zero_c, denorm);
                let t = self.s.m.select(c, e0, t, normal);
                let t = self.s.m.select(c, emax, inf, t);
                let eleven = k(self, 11);
                let mirrored = self.s.m.bin(c, Bin::Sub, eleven, t);
                let is_neg = self.s.m.cmp(c, pred::NE, neg, zero);
                let t = self.s.m.select(c, is_neg, mirrored, t);
                let (one, snan) = (k(self, 1), k(self, 0));
                let nan_class = self.s.m.select(c, q, one, snan);
                let yes = self.s.m.bool(true);
                let nan = self.s.m.bin(c, Bin::Xor, m0, yes);
                let nan = self.s.m.bin(c, Bin::And, nan, emax);
                let t = self.s.m.select(c, nan, nan_class, t);
                let bit = self.s.m.bin(c, Bin::LShr, mask, t);
                let bit = self.s.m.bin(c, Bin::And, bit, one);
                self.s.m.cmp(c, pred::NE, bit, zero)
            }
            "f32" | "f16" => {
                let (a, b) = if ty == "f16" {
                    (self.half_fsrc(inst, 0)?, self.half_fsrc(inst, 1)?)
                } else {
                    (self.fsrc(inst, 0)?, self.fsrc(inst, 1)?)
                };
                if op == "lg" {
                    // Ordered and not equal, false for a NaN, as `olt | ogt`.
                    // `fcmp one` means the same, but the AMD D3D12 driver
                    // runs it as unordered (true for a NaN) where NVIDIA
                    // and WARP do not; olt and ogt agree on all three.
                    let lt = self.s.m.cmp(c, pred::OLT, a, b);
                    let gt = self.s.m.cmp(c, pred::OGT, a, b);
                    self.s.m.bin(c, Bin::Or, lt, gt)
                } else {
                    let p = match op {
                        "f" => pred::FALSE,
                        "lt" => pred::OLT,
                        "eq" => pred::OEQ,
                        "le" => pred::OLE,
                        "gt" => pred::OGT,
                        "ge" => pred::OGE,
                        "o" => pred::ORD,
                        "u" => pred::UNO,
                        "nge" => pred::ULT,
                        "nlg" => pred::UEQ,
                        "ngt" => pred::ULE,
                        "nle" => pred::UGT,
                        "neq" => pred::UNE,
                        "nlt" => pred::UGE,
                        "tru" => pred::TRUE,
                        _ => return Err(inst.name()),
                    };
                    self.s.m.cmp(c, p, a, b)
                }
            }
            "u64" | "i64" => {
                let (a, b) = (self.src64(inst, 0)?, self.src64(inst, 1)?);
                let signed = ty == "i64";
                let p = match op {
                    "eq" => pred::EQ,
                    "ne" | "lg" => pred::NE,
                    "lt" if signed => pred::SLT,
                    "le" if signed => pred::SLE,
                    "gt" if signed => pred::SGT,
                    "ge" if signed => pred::SGE,
                    "lt" => pred::IULT,
                    "le" => pred::IULE,
                    "gt" => pred::IUGT,
                    "ge" => pred::IUGE,
                    _ => return Err("a 64-bit compare"),
                };
                self.s.m.cmp(c, p, a, b)
            }
            "u32" | "i32" | "u16" | "i16" => {
                let (a, b) = if ty.ends_with("16") {
                    let (mut a, mut b) = (self.half_src(inst, 0)?, self.half_src(inst, 1)?);
                    if ty == "i16" {
                        let k16 = self.s.m.i32(16);
                        for x in [&mut a, &mut b] {
                            let t = self.s.m.bin(c, Bin::Shl, *x, k16);
                            *x = self.s.m.bin(c, Bin::AShr, t, k16);
                        }
                    }
                    (a, b)
                } else {
                    (self.src(inst, 0)?, self.src(inst, 1)?)
                };
                let signed = ty == "i32" || ty == "i16";
                let p = match op {
                    "eq" => pred::EQ,
                    "ne" | "lg" => pred::NE,
                    "lt" if signed => pred::SLT,
                    "le" if signed => pred::SLE,
                    "gt" if signed => pred::SGT,
                    "ge" if signed => pred::SGE,
                    "lt" => pred::IULT,
                    "le" => pred::IULE,
                    "gt" => pred::IUGT,
                    "ge" => pred::IUGE,
                    "f" | "t" => {
                        self.lane(inst, Some(op == "t"));
                        return Ok(());
                    }
                    _ => return Err("an integer compare"),
                };
                self.s.m.cmp(c, p, a, b)
            }
            _ => return Err(inst.name()),
        };
        self.lane_bits_for(inst, r);
        Ok(())
    }

    pub(super) fn lane(&mut self, inst: &Inst, value: Option<bool>) {
        let b = self.s.m.bool(value == Some(true));
        self.lane_bits_for(inst, b);
    }

    fn lane_bits_for(&mut self, inst: &Inst, r: V) {
        let Fields::Valu(v) = &inst.fields else {
            return;
        };
        let code = if inst.name().starts_with("v_cmpx") {
            Some(EXEC)
        } else {
            match v.dst {
                Some(Operand::Scalar { code, .. }) => Some(code),
                _ => None,
            }
        };
        if let Some(lo) = code.and_then(|c| inst.write(Reg::S(c))) {
            self.lane_bits.insert(lo, r);
        }
        for &(_, w) in &inst.writes {
            self.vals
                .entry(w)
                .or_insert(Val::Missing("a lane mask read as data"));
        }
    }

    pub(super) fn interp(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Some(&interp) = self.interps.get(&site) else {
            // The first half of a pair: its second half interpolates the
            // attribute itself, so nothing needs this half's partial sum.
            // But an f16 second half that writes the high half of the
            // register this one wrote keeps the low half as it was (the
            // console compiler reuses the register; the low half is not
            // read), so the register needs a value: 0.
            let name = inst.name();
            if name == "v_interp_p1_f32" || name == "v_interp_p1ll_f16" {
                let dst = match &inst.fields {
                    Fields::Vintrp { dst, .. } => Some(*dst),
                    Fields::Valu(v) => v.dst,
                    _ => None,
                };
                if let Some(Operand::Vector { reg, .. }) = dst {
                    let zero = self.s.m.i32(0);
                    self.write(inst, Reg::V(reg), zero, None);
                    return Ok(());
                }
            }
            return Err(FIRST_HALF);
        };
        let name = inst.name();
        let (dst, src) = match &inst.fields {
            Fields::Vintrp { dst, src, .. } => (*dst, Some(*src)),
            Fields::Valu(v) => {
                if v.abs != 0 || v.neg != 0 {
                    return Err("interpolation operand modifiers");
                }
                (v.dst.ok_or("an interpolation")?, v.src.get(1).copied())
            }
            _ => unreachable!(),
        };
        let Operand::Vector { reg, .. } = dst else {
            return Err("an interpolation");
        };
        // A float operand of the instruction.
        let float = |l: &mut Self, op: Option<Operand>| -> Result<V, Why> {
            let x = l.operand(inst, &op.ok_or("an interpolation operand")?)?;
            Ok(l.as_float(x))
        };
        // What the result adds to the evaluated channel (a second half's
        // first half, when the halves evaluate on their own).
        let mut add = None;
        let (attribute, eval) = match *interp {
            Interp::Interpolated {
                attribute,
                barycentric,
                ..
            } => {
                let mode = interp_mode(barycentric).ok_or("pull-model interpolation")?;
                let eval = if self.per_vertex(attribute.row) {
                    // The element is read per vertex elsewhere, so it is
                    // constant: weigh its vertices with SV_Barycentrics.
                    let mut ij = [None; 2];
                    for (index, w) in ij.iter_mut().enumerate() {
                        let x = match self.barycentric(barycentric, index as u8) {
                            Val::I32(x) => x,
                            Val::Missing(w) => return Err(w),
                            Val::Bit(_) => unreachable!(),
                        };
                        *w = Some(self.as_float(x));
                    }
                    Eval::Weights {
                        p0: true,
                        i: ij[0],
                        j: ij[1],
                    }
                } else {
                    Eval::Mode(mode)
                };
                (attribute, eval)
            }
            // P0, the provoking vertex: a flat (nointerpolation) input.
            Interp::Vertex {
                attribute,
                vertex: 2,
            } => (attribute, Eval::Mode(1)),
            // P10 and P20: what the first and second halves weigh, P1 - P0
            // and P2 - P0, which is what the hardware holds for an
            // attribute the program also interpolates.
            Interp::Vertex { attribute, vertex } if self.interpolated(attribute.row) => {
                let one = Some(self.s.m.f32(1f32.to_bits()));
                let (i, j) = if vertex == 0 {
                    (one, None)
                } else {
                    (None, one)
                };
                (attribute, Eval::Weights { p0: false, i, j })
            }
            Interp::Vertex { .. } => return Err("a non-provoking vertex value"),
            // A half of an interpolation with computed i or j: P0 + i P10,
            // or the old dst (f16: the third operand) + j P20.
            Interp::Weighted { attribute } if name.starts_with("v_interp_p1") => {
                let i = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: true,
                        i,
                        j: None,
                    },
                )
            }
            Interp::Weighted { attribute } => {
                let old = match &inst.fields {
                    Fields::Valu(v) => v.src.get(2).copied(),
                    _ => Some(dst),
                };
                add = Some(float(self, old)?);
                let j = Some(float(self, src)?);
                (
                    attribute,
                    Eval::Weights {
                        p0: false,
                        i: None,
                        j,
                    },
                )
            }
        };
        let packing = match &self.x.native.io {
            StageIo::Ps { inputs, .. } => inputs
                .iter()
                .find(|i| i.row == attribute.row && i.fp16)
                .map(|i| {
                    let packs = if i.pack2 {
                        input_packs(&self.x.native.io, &self.s.inputs)
                    } else {
                        Vec::new()
                    };
                    (i.pack2, packs)
                }),
            _ => None,
        };
        let f16 = name.ends_with("_f16");
        // The evaluated channel plus `add`, as a float.
        let total = |l: &mut Self, row: u8, ch: u8| -> Result<V, Why> {
            let x = l.eval_channel(row, ch, eval)?;
            let x = l.as_float(x);
            Ok(match add {
                Some(a) => l.s.m.bin(l.cur, Bin::Add, a, x),
                None => x,
            })
        };
        let x = match packing {
            None if f16 || attribute.high => return Err("a 16-bit read of a 32-bit input"),
            None => {
                let x = total(self, attribute.row, attribute.channel)?;
                self.as_int(x)
            }
            Some((pack2, packs)) => {
                // 16-bit values: one in the low half of each dword, or two
                // per dword (`packed_half`); the host's are 32-bit floats.
                let half = |l: &mut Self, h: u8| -> Result<Option<V>, Why> {
                    let src = if pack2 {
                        packed_half(&l.s.inputs, &packs, attribute.row, h)
                    } else {
                        h.is_multiple_of(2).then_some((attribute.row, h / 2))
                    };
                    match src {
                        Some((row, ch)) => total(l, row, ch).map(Some),
                        None => Ok(None),
                    }
                };
                let to_f16 = |l: &mut Self, f: V| {
                    let i32t = l.s.t.i32;
                    l.s.op(l.cur, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
                };
                let h = 2 * attribute.channel + u8::from(attribute.high);
                let c = self.cur;
                if name == "v_interp_p1ll_f16" {
                    // An f32 result from 16-bit vertex values.
                    let f = half(self, h)?.ok_or("a packed input half no element holds")?;
                    self.as_int(f)
                } else if f16 {
                    let mut f = half(self, h)?.ok_or("a packed input half no element holds")?;
                    if let Fields::Valu(v) = &inst.fields {
                        // omod, then clamp, before the one rounding to 16 bits; op_sel[3]
                        // picks the destination half (VIctory e29391f: the low half was
                        // always written, the high one's value never arrived).
                        let (omod, clamp) = (v.omod, v.clamp);
                        if omod != 0 {
                            let factor = [1.0f32, 2.0, 4.0, 0.5][usize::from(omod)];
                            let k = self.s.m.f32(factor.to_bits());
                            f = self.s.m.bin(c, Bin::Mul, f, k);
                        }
                        if clamp {
                            f = self.unary(7, f);
                        }
                        let lo = to_f16(self, f);
                        return self.write_half(inst, lo, None);
                    }
                    let lo = to_f16(self, f);
                    self.low_half(inst, reg, lo)
                } else if matches!(eval, Eval::Mode(1)) {
                    // The whole dword of a flat input: both halves.
                    let h = 2 * attribute.channel;
                    let z = self.s.m.i32(0);
                    let lo = match half(self, h)? {
                        Some(f) => to_f16(self, f),
                        None => z,
                    };
                    let hi = match half(self, h + 1)? {
                        Some(f) => to_f16(self, f),
                        None => z,
                    };
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                } else {
                    return Err("32-bit interpolation of a 16-bit input");
                }
            }
        };
        // A pixel interpolates in every lane; guards only hide inactive ones.
        self.write(inst, Reg::V(reg), x, None);
        Ok(())
    }

    /// Whether the program reads attribute row `row` per vertex (an
    /// interpolation with computed barycentrics, or P10 or P20): then its
    /// element is declared constant and read with `attributeAtVertex`.
    pub(super) fn per_vertex(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Weighted { attribute } => attribute.row == row,
            Interp::Vertex { attribute, vertex } => {
                attribute.row == row && vertex != 2 && self.interpolated(row)
            }
            Interp::Interpolated { .. } => false,
        })
    }

    /// Whether the program interpolates attribute row `row` (so the
    /// hardware holds P1 - P0 and P2 - P0 for it).
    fn interpolated(&self, row: u8) -> bool {
        self.x.io.interps.iter().any(|(_, i)| match *i {
            Interp::Interpolated { attribute, .. } | Interp::Weighted { attribute } => {
                attribute.row == row
            }
            Interp::Vertex { .. } => false,
        })
    }

    /// Channel `ch` of input row `row` evaluated as `eval` says, as raw bits.
    fn eval_channel(&mut self, row: u8, ch: u8, eval: Eval) -> Result<V, Why> {
        let (id, col) = self
            .find_input(row, ch)
            .ok_or("an input the signature lacks")?;
        let mode = match eval {
            Eval::Mode(mode) => mode,
            Eval::Weights { .. } => 1,
        };
        let e = &mut self.s.inputs[id];
        if e.interp != mode && e.usage != 0 {
            return Err("one input interpolated two ways");
        }
        e.interp = mode;
        let Eval::Weights { p0, i, j } = eval else {
            return match self.load_any_input(id, col) {
                Val::I32(x) => Ok(x),
                _ => Err("an input"),
            };
        };
        let v0 = self.vertex_input(id, col, 0);
        let c = self.cur;
        let mut acc = p0.then_some(v0);
        for (k, w) in [(1, i), (2, j)] {
            let Some(w) = w else { continue };
            let vk = self.vertex_input(id, col, k);
            let d = self.s.m.bin(c, Bin::Sub, vk, v0);
            acc = Some(match acc {
                Some(a) => self.fmad(w, d, a),
                None => self.s.m.bin(c, Bin::Mul, w, d),
            });
        }
        let acc = acc.ok_or("an interpolation")?;
        Ok(self.as_int(acc))
    }

    /// Vertex `k` (0 the provoking vertex, as `barycentric` orders them)
    /// of a constant input element's channel, as a float.
    fn vertex_input(&mut self, id: usize, col: u8, k: u8) -> V {
        let e = &mut self.s.inputs[id];
        e.usage |= 1 << col;
        let int = e.comp != 9;
        let (ty, suffix) = if int {
            (self.s.t.i32, ".i32")
        } else {
            (self.s.t.f32, ".f32")
        };
        self.s.flags |= flags::BARYCENTRICS;
        let entry = self.s.entry;
        let args = [
            self.s.m.i32(id as u32),
            self.s.m.i32(0),
            self.s.m.i8(col),
            self.s.m.i8(k),
        ];
        let v = self.s.op(
            entry,
            &format!("attributeAtVertex{suffix}"),
            Attr::ReadNone,
            ty,
            137,
            &args,
        );
        if int {
            self.s.m.cast(entry, Cast::Bitcast, v, self.s.t.f32)
        } else {
            v
        }
    }

    /// `x` (an f16 in the low bits) as the low half of VGPR `reg`: 16-bit
    /// results keep the register's high half (GFX10).
    pub(super) fn low_half(&mut self, inst: &Inst, reg: u8, x: V) -> V {
        let c = self.cur;
        let old = match inst.read(Reg::V(reg)).map(|o| self.i32_of(o)) {
            Some(Ok(o)) => o,
            _ => self.s.m.i32(0),
        };
        let (high, low) = (self.s.m.i32(0xFFFF_0000), self.s.m.i32(0xFFFF));
        let old = self.s.m.bin(c, Bin::And, old, high);
        let x = self.s.m.bin(c, Bin::And, x, low);
        self.s.m.bin(c, Bin::Or, old, x)
    }

    pub(super) fn image(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let sampler = self.descriptors.get(&((b, i), true)).copied();
        if let Some(Source::Phi { block, args }) = sampler
            && let [first @ Source::Constant(_), second @ Source::Constant(_)] = args.as_slice()
            && let [left, right] = self.x.p.blocks[*block].preds.as_slice()
        {
            // Immediate samplers are separate static bindings. Sample both with
            // the original derivatives, then select the texel on the same edge
            // that supplied the console's sampler.
            if self.block[*left].is_none() {
                return self.image_with_sampler(inst, b, i, Some(second));
            }
            if self.block[*right].is_none() {
                return self.image_with_sampler(inst, b, i, Some(first));
            }
            let edge = |pred: usize| {
                self.edges
                    .get(&(pred, *block))
                    .copied()
                    .or(self.tail[pred])
                    .ok_or("a sampler choice with an unfinished predecessor")
            };
            let (left, right) = (edge(*left)?, edge(*right)?);
            let join = self.block[*block].ok_or("a sampler choice without a host block")?;
            let chosen = self.s.m.phi(join, self.s.t.i1);
            let (yes, no) = (self.s.m.bool(true), self.s.m.bool(false));
            self.s.m.set_phi(chosen, vec![(yes, left), (no, right)]);
            self.image_with_sampler(inst, b, i, Some(first))?;
            let left = inst
                .writes
                .iter()
                .map(|&(_, v)| self.i32_of(v))
                .collect::<Result<Vec<_>, _>>()?;
            self.image_with_sampler(inst, b, i, Some(second))?;
            for (&(_, v), a) in inst.writes.iter().zip(left) {
                let b = self.i32_of(v)?;
                let result = self.s.m.select(self.cur, chosen, a, b);
                self.vals.insert(v, Val::I32(result));
            }
            return Ok(());
        }
        self.image_with_sampler(inst, b, i, sampler)
    }

    fn image_with_sampler(
        &mut self,
        inst: &Inst,
        b: usize,
        i: usize,
        sampler: Option<&Source>,
    ) -> Result<(), Why> {
        let Fields::Image {
            data,
            addr,
            dmask,
            dim,
            unorm,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        if *tfe || *lwe {
            return Err("an image access with tfe or lwe");
        }
        let name = inst.name();
        // A load or sample with no channel enabled returns the first.
        let dmask = if *dmask == 0 { &1 } else { dmask };
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let (kind, coords) = match dim {
            0 => (1, 1),
            1 => (2, 2),
            2 => (4, 3),
            // Sampled (face coordinates and face, see `cube_direction`) or
            // sized only.
            3 if name.starts_with("image_sample")
                || name.starts_with("image_gather")
                || name == "image_get_resinfo" =>
            {
                (5, 3)
            }
            4 => (6, 2),
            5 => (7, 3),
            _ => return Err("an image dimension (cube loads or MSAA)"),
        };
        // Address VGPRs in order.
        let mut regs = Vec::new();
        for a in addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        // Full 32-bit address VGPR at `reg_idx` (offset, bias, z-compare).
        let addr_u32 = |l: &mut Self, reg_idx: usize| -> Result<V, Why> {
            let r = *regs.get(reg_idx).ok_or("an image address")?;
            let v = inst.read(Reg::V(r)).ok_or("an image address")?;
            l.i32_of(v)
        };
        // With a16, two 16-bit components per VGPR starting at `base_reg`, low half first.
        let addr_packed = |l: &mut Self, base_reg: usize, k: usize| -> Result<V, Why> {
            let x = addr_u32(l, base_reg + if *a16 { k / 2 } else { k })?;
            if !*a16 {
                return Ok(x);
            }
            let c = l.cur;
            let (sh, mask) = (l.s.m.i32(16 * (k as u32 % 2)), l.s.m.i32(0xFFFF));
            let x = l.s.m.bin(c, Bin::LShr, x, sh);
            Ok(l.s.m.bin(c, Bin::And, x, mask))
        };
        let addr_val = |l: &mut Self, k: usize| -> Result<V, Why> { addr_packed(l, 0, k) };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked image")?;
        let store = name.starts_with("image_store");
        let uav = store
            || self
                .used_kind(src)
                .is_some_and(|u| u.kind == ResourceType::Uav);
        // A UAV inside an ordered section is a rasterizer ordered view.
        let ordered = uav && self.ordered.contains(&site);
        if ordered {
            self.s.flags |= flags::ROVS;
        }
        let rov = if ordered { 0x2000 } else { 0 };
        let props = (kind | if uav { 0x1000 } else { 0 } | rov, 0x409);
        if let Some(op) = name.strip_prefix("image_atomic_") {
            // GLC returns the old value. DXIL implements subtraction by adding
            // the two's complement of the operand at the same width.
            let code = match op {
                "add" | "sub" => 0,
                "and" => 1,
                "or" => 2,
                "xor" => 3,
                "smin" => 4,
                "smax" => 5,
                "umin" => 6,
                "umax" => 7,
                "swap" => 8,
                "cmpswap" => u32::MAX,
                _ => return Err(name),
            };
            let Fields::Image { cache, .. } = &inst.fields else {
                unreachable!()
            };
            // 16-bit coordinates (a16) are read as `addr_val` reads them.
            if *d16 {
                return Err("a 16-bit image atomic");
            }
            // Compare-exchange consumes a replacement followed by a comparand.
            let compare = code == u32::MAX;
            let wide = match (compare, *dmask) {
                (false, 1) | (true, 3) => false,
                (false, 3) | (true, 15) => true,
                _ => return Err("an image atomic data mask"),
            };
            let iu = self.s.m.undef(self.s.t.i32);
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let Operand::Vector { reg, .. } = *data else {
                return Err("image atomic data");
            };
            let width = if wide { 2 } else { 1 };
            let vals = self.vgprs(inst, data, if compare { 2 * width } else { width })?;
            let (ty, component) = if wide {
                self.s.flags |= flags::INT64 | flags::ATOMIC_TYPED64 | flags::ATOMIC_HEAP64;
                (self.s.m.int(64), 0x107) // one uint64_t in an R32G32_UINT view
            } else {
                (self.s.t.i32, 0x105)
            };
            let original = if wide {
                self.join64(vals[0], vals[1])
            } else {
                vals[0]
            };
            let value = if op == "sub" {
                let zero = self.s.m.int_const(ty, 0);
                self.s.m.bin(c, Bin::Sub, zero, original)
            } else {
                original
            };
            let h = self.handle(site, src, false, (kind | 0x1000 | rov, component))?;
            let mut args = vec![h];
            let (intrinsic, opcode) = if compare {
                let comparand = if wide {
                    self.join64(vals[2], vals[3])
                } else {
                    vals[1]
                };
                args.extend(cs);
                args.extend([comparand, value]);
                let intrinsic = if wide {
                    "atomicCompareExchange.i64"
                } else {
                    "atomicCompareExchange.i32"
                };
                (intrinsic, 79)
            } else {
                args.push(self.s.m.i32(code));
                args.extend(cs);
                args.push(value);
                (
                    if wide {
                        "atomicBinOp.i64"
                    } else {
                        "atomicBinOp.i32"
                    },
                    78,
                )
            };
            let at = self.begin_guarded(guard);
            let r = self
                .s
                .op(self.cur, intrinsic, Attr::NoUnwind, ty, opcode, &args);
            if cache.glc {
                let old = self.end_guarded(at, &[(r, original)])[0];
                if wide {
                    let (lo, hi) = self.split64(old);
                    self.write(inst, Reg::V(reg), lo, None);
                    self.write(inst, Reg::V(reg + 1), hi, None);
                } else {
                    self.write(inst, Reg::V(reg), old, None);
                }
            } else {
                self.end_guarded(at, &[]);
            }
            return Ok(());
        }
        if store {
            if name != "image_store" {
                return Err("an image store to a mip level");
            }
            // shortcut: stores are taken to write all four channels of a
            // float format (the console converts by the view's format).
            // One channel goes into a view the program also counts in.
            let counter = *dmask == 1 && self.is_counter(src);
            if *dmask != 0xF && !counter {
                return Err("an image store of some channels");
            }
            let iu = self.s.m.undef(self.s.t.i32);
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let Operand::Vector { reg, .. } = *data else {
                return Err("image store data");
            };
            if counter {
                let v = inst.read(Reg::V(reg)).ok_or("image store data")?;
                let x = self.i32_of(v)?;
                // A 16-bit integer, extended by the view's sign: the same
                // number either way only if it is known to be small.
                if *d16 && !self.s.m.int_k(x).is_some_and(|(_, k)| k < 0x8000) {
                    return Err("a 16-bit image store of one channel");
                }
                let h = self.handle(site, src, false, (kind | 0x1000 | rov, 0x105))?;
                let at = self.begin_guarded(guard);
                // All four channels are written (the validator wants them
                // all): the view has the first alone.
                let mut args = vec![h];
                args.extend(cs);
                args.extend([x; 4]);
                args.push(self.s.m.i8(0xF));
                let void = self.s.t.void;
                self.s.op(
                    self.cur,
                    "textureStore.i32",
                    Attr::NoUnwind,
                    void,
                    67,
                    &args,
                );
                self.end_guarded(at, &[]);
                return Ok(());
            }
            let mut vals = Vec::new();
            for k in 0..4u8 {
                // d16: two f16 channels per VGPR, low half first.
                let r = if *d16 { k / 2 } else { k };
                let v = inst.read(Reg::V(reg + r)).ok_or("image store data")?;
                vals.push(if *d16 {
                    let mut x = self.i32_of(v)?;
                    if k % 2 == 1 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(self.cur, Bin::LShr, x, k16);
                    }
                    let f32t = self.s.t.f32;
                    self.s
                        .op(self.cur, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x])
                } else {
                    self.float_of(v)?
                });
            }
            let h = self.handle(site, src, false, props)?;
            let at = self.begin_guarded(guard);
            let mut args = vec![h];
            args.extend(cs);
            args.extend(vals);
            args.push(self.s.m.i8(0xF));
            let void = self.s.t.void;
            self.s.op(
                self.cur,
                "textureStore.f32",
                Attr::NoUnwind,
                void,
                67,
                &args,
            );
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let has_cl = name.ends_with("_cl");
        let (op, sample) = match name {
            "image_sample" | "image_sample_o" | "image_sample_cl" => (60, true),
            "image_sample_lz" | "image_sample_l" | "image_sample_lz_o" => (62, true),
            "image_sample_b" | "image_sample_b_cl" => (61, true),
            "image_sample_c_lz" => (65, true),
            "image_sample_d" | "image_sample_d_cl" => (63, true),
            "image_get_lod" if self.s.pixel => (81, true),
            // Implicit derivatives: pixel shaders only.
            "image_sample_c" if self.s.pixel => (64, true),
            "image_sample_c_cl" if self.s.pixel => (64, true),
            "image_gather4" | "image_gather4_lz" | "image_gather4_lz_o" | "image_gather4_l"
            | "image_gather4_l_o" => (73, true),
            "image_load" | "image_load_mip" => (66, false),
            "image_get_resinfo" => (72, false),
            _ => return Err(name),
        };
        if op == 72 {
            // Width, height, depth or layers, levels (per dmask) of the level
            // in the first address: DXIL's dimensions; a channel DXIL leaves
            // out reads 1 (UAVs: one level).
            let h = self.handle(site, src, false, props)?;
            let mip = if uav {
                self.s.m.undef(self.s.t.i32)
            } else {
                addr_val(self, 0)?
            };
            let i32t = self.s.t.i32;
            let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
            let d = self
                .s
                .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
            let one = self.s.m.i32(1);
            let present = match kind {
                1 => [true, false, false, true],
                2 | 5 | 6 => [true, true, false, true],
                _ => [true, true, true, true],
            };
            let Operand::Vector { reg, .. } = *data else {
                return Err("an image result");
            };
            for (k, ch) in (0..4u32).filter(|ch| dmask >> ch & 1 != 0).enumerate() {
                let x = if ch == 3 && uav || !present[ch as usize] {
                    one
                } else {
                    self.s.m.extract(c, d, ch)
                };
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        if uav && op != 66 {
            return Err("sampling a UAV");
        }
        let h = self.handle(site, src, false, props)?;
        let fu = self.s.m.undef(self.s.t.f32);
        let iu = self.s.m.undef(self.s.t.i32);
        // A gather at a named level, done as four texel reads (below).
        let mut gathered: Option<[V; 4]> = None;
        let r = if sample {
            let ss = sampler.ok_or("an untracked sampler")?;
            // Comparison: the reference value is the first address after
            // the bias, and the sampler is a comparison sampler.
            let cmp = matches!(op, 64 | 65);
            if cmp && (*a16 || dmask.count_ones() != 1) {
                return Err("a comparison sample of 16-bit coordinates or several channels");
            }
            let mut sampler_choice: Option<(V, Vec<u32>)> = None;
            let sh = match ss {
                Source::Phi { block, args } => {
                    match self.sampler_phi_choice(*block, args, cmp, 8) {
                        Ok((None, regs)) => self.s.sampler_handle(self.cur, regs[0]),
                        Ok((Some(idx), regs)) => {
                            let first = self.s.sampler_handle(self.cur, regs[0]);
                            sampler_choice = Some((idx, regs));
                            first
                        }
                        Err(_) => {
                            self.handle(site, ss, true, (14 | if cmp { 0x8000 } else { 0 }, 0))?
                        }
                    }
                }
                _ => self.handle(site, ss, true, (14 | if cmp { 0x8000 } else { 0 }, 0))?,
            };
            // Texel offsets (`_o`) come first: x in bits 0-5, y in 8-13,
            // signed (programmable offsets: gathers only).
            let offset_word = name.ends_with("_o");
            let first = usize::from(offset_word)
                + usize::from(name.starts_with("image_sample_b"))
                + usize::from(cmp);
            // Coordinates the size divides (not the array slice); explicit
            // gradients (x then y) come before the coordinates.
            let spatial = coords - usize::from(kind == 6 || kind == 7);
            // A cube's gradients are the two of its face coordinates.
            let grads = if kind == 5 { 2 } else { spatial };
            let deriv_regs = if op == 63 {
                if *a16 { grads.div_ceil(2) } else { grads }
            } else {
                0
            };
            let coord_base = first + 2 * deriv_regs;
            let mut cs = vec![fu; 4];
            // With a16, sampling coordinates, gradients, LOD and clamp are f16.
            let to_float = |l: &mut Self, x: V| {
                if *a16 {
                    let f = l.s.t.f32;
                    l.s.op(l.cur, "legacyF16ToF32", Attr::ReadNone, f, 131, &[x])
                } else {
                    l.as_float(x)
                }
            };
            if kind == 5 && (*unorm || op == 81) {
                return Err("a cube sample with texel coordinates or a level query");
            }
            // LOD queries take only spatial coordinates, not the array slice.
            // Leave unused DXIL coordinate slots undef, as the validator requires.
            let used_coords = if op == 81 { spatial } else { coords };
            for (k, slot) in cs.iter_mut().enumerate().take(used_coords) {
                let x = addr_packed(self, coord_base, k)?;
                *slot = to_float(self, x);
            }
            if kind == 5 {
                let dir = self.cube_direction(cs[0], cs[1], cs[2]);
                cs[..3].copy_from_slice(&dir);
            }
            if *unorm {
                // Texel coordinates: divided by the size of level 0.
                let i32t = self.s.t.i32;
                let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                let z = self.s.m.i32(0);
                let d = self
                    .s
                    .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                for (k, slot) in cs.iter_mut().enumerate().take(spatial) {
                    let n = self.s.m.extract(c, d, k as u32);
                    let n = self.s.m.cast(c, Cast::UiToFp, n, self.s.t.f32);
                    *slot = self.s.m.bin(c, Bin::SDiv, *slot, n);
                }
            }
            if op == 81 {
                // The clamped then the unclamped level of detail.
                let Operand::Vector { reg, .. } = *data else {
                    return Err("an image result");
                };
                let f32t = self.s.t.f32;
                for (k, ch) in (0..2u32).filter(|ch| dmask >> ch & 1 != 0).enumerate() {
                    let clamped = self.s.m.bool(ch == 0);
                    let lod = self.s.op(
                        c,
                        "calculateLOD.f32",
                        Attr::ReadOnly,
                        f32t,
                        81,
                        &[h, sh, cs[0], cs[1], cs[2], clamped],
                    );
                    let x = self.as_int(lod);
                    self.write(inst, Reg::V(reg + k as u8), x, guard);
                }
                return Ok(());
            }
            let offsets = if kind == 5 {
                0
            } else {
                (coords - usize::from(kind == 6 || kind == 7)).min(3)
            };
            let zero = self.s.m.i32(0);
            let mut os = vec![iu; 3];
            for o in os.iter_mut().take(offsets) {
                *o = zero;
            }
            if offset_word && offsets > 0 {
                let w = addr_val(self, 0)?;
                for (k, at) in [(0, 26), (1, 18)].into_iter().take(offsets.min(2)) {
                    // Bits at..at+5 sign-extended: shift up, then down.
                    let left = self.s.m.i32(at);
                    let t = self.s.m.bin(c, Bin::Shl, w, left);
                    let k26 = self.s.m.i32(26);
                    os[k] = self.s.m.bin(c, Bin::AShr, t, k26);
                }
            }
            let mut args = vec![h, sh];
            args.extend(&cs);
            let clamp = if has_cl {
                let x = addr_packed(self, coord_base, coords)?;
                to_float(self, x)
            } else {
                fu
            };
            let call_sample = |l: &mut Self,
                               intrinsic: &str,
                               opcode: u32,
                               args: &[V],
                               gathered: &mut Option<[V; 4]>|
             -> V {
                let Some((idx, ref regs)) = sampler_choice else {
                    return l
                        .s
                        .op(c, intrinsic, Attr::ReadOnly, l.s.t.res_ret, opcode, args);
                };
                let merge = l.s.m.new_block();
                let mut cur_test = c;
                let mut per_ch: [Vec<(V, usize)>; 4] = Default::default();
                let mut last_r = args[0];
                for (i, &reg) in regs.iter().enumerate() {
                    let arm = if i + 1 == regs.len() {
                        cur_test
                    } else {
                        let arm = l.s.m.new_block();
                        let next = l.s.m.new_block();
                        let k = l.s.m.i32(i as u32);
                        let cond = l.s.m.cmp(cur_test, pred::EQ, idx, k);
                        l.s.m.terminate(cur_test, Term::CondBr(cond, arm, next));
                        cur_test = next;
                        arm
                    };
                    let mut arm_args = args.to_vec();
                    arm_args[1] = l.s.sampler_handle(arm, reg);
                    let r_i = l.s.op(
                        arm,
                        intrinsic,
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        opcode,
                        &arm_args,
                    );
                    for (ch, slot) in per_ch.iter_mut().enumerate() {
                        let v = l.s.m.extract(arm, r_i, ch as u32);
                        slot.push((v, arm));
                    }
                    l.s.m.terminate(arm, Term::Br(merge));
                    last_r = r_i;
                }
                let mut phis = [fu; 4];
                for (ch, incoming) in per_ch.into_iter().enumerate() {
                    let phi = l.s.m.phi(merge, l.s.t.f32);
                    l.s.m.set_phi(phi, incoming);
                    phis[ch] = phi;
                }
                *gathered = Some(phis);
                l.cur = merge;
                last_r
            };
            match op {
                // Outside a pixel shader there are no screen derivatives:
                // the validator refuses `sample` there ("features
                // incompatible with the shader stage": 730 compute programs,
                // among them the G-buffer resolves and the sky), and what a
                // driver does with it is its own affair.
                // shortcut: such a sample reads level 0 (a bias: that
                // level). The console takes derivatives from neighbouring
                // lanes there; compute them from the lanes if a resolve
                // shows the wrong mip.
                60 if !self.s.pixel => {
                    args.extend(&os);
                    args.push(self.s.m.f32(0));
                    call_sample(self, "sampleLevel.f32", 62, &args, &mut gathered)
                }
                60 => {
                    if offset_word {
                        // DXIL wants immediate offsets in -8..=7 here; the
                        // console's are 6-bit and may be computed. A
                        // constant word in range becomes immediates
                        // (upstream PR #634); anything else keeps the
                        // offsets computed above (upstream refuses those).
                        let w = addr_val(self, 0)?;
                        if let Some(w) = self.s.m.int_value(w) {
                            let w = w as u32;
                            let field = |k: usize| {
                                let x = (w >> (8 * k) & 0x3F) as i32;
                                if x >= 32 { x - 64 } else { x }
                            };
                            let n = offsets.min(2);
                            if (0..n).all(|k| (-8..=7).contains(&field(k))) {
                                for (k, o) in os.iter_mut().enumerate().take(n) {
                                    *o = self.s.m.i32(field(k) as u32);
                                }
                            }
                        }
                    }
                    args.extend(&os);
                    args.push(clamp);
                    call_sample(self, "sample.f32", 60, &args, &mut gathered)
                }
                61 => {
                    // With a16 the bias is an f16 in the low half of its
                    // word (the word is whole, the value is not).
                    let bias = addr_u32(self, usize::from(offset_word))?;
                    let bias = if *a16 {
                        let low = self.s.m.i32(0xFFFF);
                        let bias = self.s.m.bin(c, Bin::And, bias, low);
                        to_float(self, bias)
                    } else {
                        self.as_float(bias)
                    };
                    args.extend(&os);
                    if self.s.pixel {
                        args.extend([bias, clamp]);
                        call_sample(self, "sampleBias.f32", 61, &args, &mut gathered)
                    } else {
                        // As above: the bias is the level.
                        args.push(bias);
                        call_sample(self, "sampleLevel.f32", 62, &args, &mut gathered)
                    }
                }
                63 if kind == 5 => {
                    // A cube with explicit gradients: the host wants them
                    // as changes of the direction, the console gives the
                    // changes of the face coordinates (the face is one
                    // unit wide). The level they select is taken from the
                    // longer of the two, in texels, and sampled directly.
                    // shortcut: no anisotropic filtering for these samples
                    // (the longer axis alone decides the level).
                    let mut len = [fu; 2];
                    for (axis, slot) in len.iter_mut().enumerate() {
                        let mut sum = self.s.m.f32(0);
                        for k in 0..2 {
                            let x = addr_packed(self, first + axis * deriv_regs, k)?;
                            let x = to_float(self, x);
                            let sq = self.s.m.bin(c, Bin::Mul, x, x);
                            sum = self.s.m.bin(c, Bin::Add, sum, sq);
                        }
                        *slot = self.unary(24, sum); // square root
                    }
                    let longer = self.binary(35, len[0], len[1]); // maximum
                    let i32t = self.s.t.i32;
                    let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                    let z = self.s.m.i32(0);
                    let d = self
                        .s
                        .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
                    let width = self.s.m.extract(c, d, 0);
                    let width = self.s.m.cast(c, Cast::UiToFp, width, self.s.t.f32);
                    let texels = self.s.m.bin(c, Bin::Mul, longer, width);
                    // DXIL's logarithm is the one to base 2 (it was taken for
                    // the natural one: every level came out 1.44 times too
                    // high; DexNom/VIctory 921d8d3b). A length of 0 gives
                    // minus infinity, which the sampler clamps to level 0.
                    // SampleLevel takes no clamp: the instruction's least
                    // level is applied here.
                    let lod = self.unary(23, texels);
                    let lod = if has_cl {
                        self.binary(35, lod, clamp)
                    } else {
                        lod
                    };
                    args.extend(&os);
                    args.push(lod);
                    call_sample(self, "sampleLevel.f32", 62, &args, &mut gathered)
                }
                63 => {
                    let mut g = vec![fu; 6];
                    for k in 0..spatial {
                        let x = addr_packed(self, first, k)?;
                        g[k] = to_float(self, x);
                        let y = addr_packed(self, first + deriv_regs, k)?;
                        g[3 + k] = to_float(self, y);
                    }
                    args.extend(&os);
                    args.extend(g);
                    args.push(clamp);
                    call_sample(self, "sampleGrad.f32", 63, &args, &mut gathered)
                }
                64 | 65 => {
                    let reference = addr_u32(self, first - 1)?;
                    let reference = self.as_float(reference);
                    args.extend(&os);
                    args.push(reference);
                    let name = if op == 64 {
                        args.push(clamp);
                        "sampleCmp.f32"
                    } else {
                        "sampleCmpLevelZero.f32"
                    };
                    call_sample(self, name, op, &args, &mut gathered)
                }
                62 => {
                    let lod = if name == "image_sample_l" {
                        let x = addr_packed(self, coord_base, coords)?;
                        to_float(self, x)
                    } else {
                        self.s.m.f32(0)
                    };
                    args.extend(&os);
                    args.push(lod);
                    call_sample(self, "sampleLevel.f32", 62, &args, &mut gathered)
                }
                _ => {
                    if name == "image_gather4" {
                        return Err("a gather with implicit level");
                    }
                    if dmask.count_ones() != 1 {
                        return Err("a gather of several channels");
                    }
                    if matches!(name, "image_gather4_l" | "image_gather4_l_o")
                        && coords == 2
                        && spatial == 2
                    {
                        // The host's gather has no level: it read level 0
                        // where the title names one (the depth pyramid its
                        // occlusion tests read: most of the world was
                        // culled against the wrong depths). The four texels
                        // are read at the level instead, in the gather's
                        // order: (u0,v1) (u1,v1) (u1,v0) (u0,v0).
                        // With an offset too (`image_gather4_l_o`): the
                        // offset is in texels of that level.
                        // The title's meshlet culling takes a 4x4 block of
                        // the pyramid as one gather and three offset ones
                        // ((2,0), (0,2), (2,2)); the three read level 0,
                        // two texels beside the first one's place, so a
                        // meshlet three or four texels wide was tested
                        // against a quarter of what it covers.
                        // shortcut: the level is rounded to a whole one and
                        // coordinates past the edge are clamped, whatever
                        // the sampler says.
                        let lod = addr_packed(self, coord_base, coords)?;
                        let lod = to_float(self, lod);
                        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
                        let zero = self.s.m.f32(0);
                        let lod = self.binary(35, lod, zero); // maximum
                        let lod = self.unary(26, lod); // round to nearest
                        let mip = self.s.m.cast(c, Cast::FpToUi, lod, i32t);
                        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
                        let d = self
                            .s
                            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
                        let half = self.s.m.f32(0.5f32.to_bits());
                        let one = self.s.m.f32(1.0f32.to_bits());
                        let (mut lo, mut hi) = ([iu; 2], [iu; 2]);
                        for k in 0..2 {
                            let n = self.s.m.extract(c, d, k as u32);
                            let n = self.s.m.cast(c, Cast::UiToFp, n, f32t);
                            let last = self.s.m.bin(c, Bin::Sub, n, one);
                            let t = self.s.m.bin(c, Bin::Mul, cs[k], n);
                            let t = self.s.m.bin(c, Bin::Sub, t, half);
                            let first = self.unary(27, t); // round down
                            // The offset word's field (0 without one).
                            let by = self.s.m.cast(c, Cast::SiToFp, os[k], f32t);
                            let first = self.s.m.bin(c, Bin::Add, first, by);
                            let next = self.s.m.bin(c, Bin::Add, first, one);
                            for (slot, x) in [(&mut lo[k], first), (&mut hi[k], next)] {
                                let x = self.binary(35, x, zero); // maximum
                                let x = self.binary(36, x, last); // minimum
                                *slot = self.s.m.cast(c, Cast::FpToSi, x, i32t);
                            }
                        }
                        let ch = dmask.trailing_zeros();
                        let mut g = [fu; 4];
                        let mut read = None;
                        let texels = [
                            (lo[0], hi[1]),
                            (hi[0], hi[1]),
                            (hi[0], lo[1]),
                            (lo[0], lo[1]),
                        ];
                        for (slot, (x, y)) in g.iter_mut().zip(texels) {
                            let r = self.s.op(
                                c,
                                "textureLoad.f32",
                                Attr::ReadOnly,
                                self.s.t.res_ret,
                                66,
                                &[h, mip, x, y, iu, iu, iu, iu],
                            );
                            *slot = self.s.m.extract(c, r, ch);
                            read = Some(r);
                        }
                        gathered = Some(g);
                        read.expect("four reads")
                    } else {
                        args.extend(&os[..2]);
                        args.push(self.s.m.i32(dmask.trailing_zeros()));
                        call_sample(self, "textureGather.f32", 73, &args, &mut gathered)
                    }
                }
            }
        } else {
            let mut cs = vec![iu; 3];
            for (k, slot) in cs.iter_mut().enumerate().take(coords) {
                *slot = addr_val(self, k)?;
            }
            let mip = if name == "image_load_mip" {
                addr_val(self, coords)?
            } else if uav {
                // Our declarations have four channels.
                self.s.flags |= flags::TYPED_UAV_LOADS;
                iu
            } else {
                self.s.m.i32(0)
            };
            let mut args = vec![h, mip];
            args.extend(&cs);
            args.extend([iu, iu, iu]);
            self.s.op(
                c,
                "textureLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                66,
                &args,
            )
        };
        let Operand::Vector { reg, .. } = *data else {
            return Err("an image result");
        };
        let channels: Vec<u32> = if op == 73 {
            vec![0, 1, 2, 3]
        } else {
            (0..4).filter(|ch| dmask >> ch & 1 != 0).collect()
        };
        let mut xs = Vec::new();
        for ch in channels {
            let x = match gathered {
                Some(g) => g[ch as usize],
                None => self.s.m.extract(c, r, ch),
            };
            xs.push(if *d16 {
                // shortcut: d16 results are taken as floats (rounded to
                // nearest even); integer formats would keep 16 bits.
                let i32t = self.s.t.i32;
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[x])
            } else {
                self.as_int(x)
            });
        }
        if *d16 {
            // Two channels per VGPR, low half first (an odd last high half 0).
            let k16 = self.s.m.i32(16);
            xs = xs
                .chunks(2)
                .map(|p| match *p {
                    [lo, hi] => {
                        let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                        self.s.m.bin(c, Bin::Or, lo, hi)
                    }
                    _ => p[0],
                })
                .collect();
        }
        for (k, x) in xs.into_iter().enumerate() {
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    /// The direction a cube sample's face coordinates stand for: the
    /// console takes s and t in [1, 2] (`v_cubesc`/`v_cubetc` over
    /// `|v_cubema|`, plus 1.5) and the face id (`v_cubeid`), which the
    /// host's cube sampling turns back into the same texel.
    // shortcut: the view is taken as a cube, not a cube array (whose face
    // operand adds 8 × the slice).
    fn cube_direction(&mut self, s: V, t: V, face: V) -> [V; 3] {
        let c = self.cur;
        let k = |l: &mut Self, v: f32| l.s.m.f32(v.to_bits());
        let (two, three) = (k(self, 2.0), k(self, 3.0));
        let u = self.s.m.bin(c, Bin::Mul, s, two);
        let u = self.s.m.bin(c, Bin::Sub, u, three);
        let v = self.s.m.bin(c, Bin::Mul, t, two);
        let v = self.s.m.bin(c, Bin::Sub, v, three);
        let z = self.s.m.f32(0x8000_0000);
        let (nu, nv) = (
            self.s.m.bin(c, Bin::Sub, z, u),
            self.s.m.bin(c, Bin::Sub, z, v),
        );
        let (one, mone) = (k(self, 1.0), k(self, -1.0));
        let f = self.s.m.cast(c, Cast::FpToUi, face, self.s.t.i32);
        // Per face +x, -x, +y, -y, +z, -z.
        let table = [
            [one, mone, u, u, u, nu],
            [nv, nv, one, mone, nv, nv],
            [nu, u, v, nv, one, mone],
        ];
        table.map(|row| {
            let mut r = row[0];
            for (n, &x) in row.iter().enumerate().skip(1) {
                let kn = self.s.m.i32(n as u32);
                let is = self.s.m.cmp(c, pred::EQ, f, kn);
                r = self.s.m.select(c, is, x, r);
            }
            r
        })
    }

    pub(super) fn buffer(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        self.root_copy = None;
        let Fields::Buffer {
            data,
            addr,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        if name == "buffer_gl0_inv" || name == "buffer_gl1_inv" {
            // The host has no console buffer cache to invalidate.
            return Ok(());
        }
        if *lds || *tfe {
            return Err("a buffer access to LDS or with tfe");
        }
        if format.is_some() && !name.starts_with("tbuffer_load_format_") {
            return Err(name);
        }
        let site = (b, i);
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let store = name.starts_with("buffer_store_");
        // DXIL atomic operations: add, and, or, xor, signed and unsigned
        // min and max, exchange; compare-exchange apart.
        let atomic = match name.strip_prefix("buffer_atomic_") {
            None => None,
            Some("add") => Some(0),
            Some("and") => Some(1),
            Some("or") => Some(2),
            Some("xor") => Some(3),
            Some("smin") => Some(4),
            Some("smax") => Some(5),
            Some("umin") => Some(6),
            Some("umax") => Some(7),
            Some("swap") => Some(8),
            Some("cmpswap") => Some(u32::MAX),
            Some(_) => return Err(name),
        };
        // Byte and 16-bit accesses: bits, and whether a load sign-extends.
        let narrow = match name {
            "buffer_load_ubyte" | "buffer_store_byte" => Some((8, false)),
            "buffer_load_sbyte" => Some((8, true)),
            "buffer_load_ushort" | "buffer_store_short" => Some((16, false)),
            "buffer_load_sshort" => Some((16, true)),
            _ => None,
        };
        // d16: two 16-bit channels per VGPR, low half first (buffer and
        // tbuffer formats alike).
        let d16 = name.contains("_format_d16_");
        let plain = name.replace("_format_d16_", "_format_");
        let (typed, count) = match plain
            .strip_prefix("buffer_load_")
            .or(plain.strip_prefix("buffer_store_"))
            .or(plain.strip_prefix("tbuffer_load_"))
        {
            _ if atomic.is_some() || narrow.is_some() => (false, 1),
            Some("dword") => (false, 1),
            Some("dwordx2") => (false, 2),
            Some("dwordx3") => (false, 3),
            Some("dwordx4") => (false, 4),
            Some("format_x") => (true, 1),
            Some("format_xy") => (true, 2),
            Some("format_xyz") => (true, 3),
            Some("format_xyzw") => (true, 4),
            _ => return Err(name),
        };
        let Operand::Vector { reg: areg, .. } = *addr else {
            return Err("a buffer address");
        };
        let vaddr = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(areg + k)).ok_or("a buffer address")?;
            l.i32_of(v)
        };
        let index = if *idxen { Some(vaddr(self, 0)?) } else { None };
        let mut off = self.s.m.i32(u32::from(*offset));
        if *offen {
            let o = vaddr(self, u8::from(*idxen))?;
            off = self.s.m.bin(c, Bin::Add, off, o);
        }
        if !matches!(
            soffset,
            Operand::Scalar { code: NULL, .. } | Operand::Const(128)
        ) {
            let so = self.operand(inst, soffset)?;
            off = self.s.m.bin(c, Bin::Add, off, so);
        }
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked buffer")?;
        let used = self.used_kind(src);
        let (base, extra) = self.built_offset(src)?;
        if base != 0 {
            let b = self.s.m.i32(base);
            off = self.s.m.bin(c, Bin::Add, off, b);
        }
        if let Some(x) = extra {
            off = self.s.m.bin(c, Bin::Add, off, x);
        }
        // A buffer the program picks from the view heap by an index it
        // computes: no used-resource entry gives its stride, the host's
        // table by heap slot does; the access becomes a raw one at the
        // element's byte. (Also tells the host this shader reads such
        // buffers.)
        let mut index = index;
        if let Some(stride) = self.slot_stride(src)?
            && let Some(element) = index
            && !typed
            && used.is_none()
        {
            let at = self.s.m.bin(c, Bin::Mul, element, stride);
            off = self.s.m.bin(c, Bin::Add, at, off);
            index = None;
        }
        // A structured buffer by its used-resource entry's stride: raw
        // bytes too. The host makes its buffer views raw (a raw read
        // through a structured view gives zeros on NVIDIA), and one kind
        // of view can then serve every shader that reads the buffer.
        if let (Some(element), Some(u)) = (index, used)
            && !typed
            && u.kind != ResourceType::Cbv
            && u.stride >= 4
            && u.stride.is_multiple_of(4)
        {
            let k = self.s.m.i32(u.stride);
            let at = self.s.m.bin(c, Bin::Mul, element, k);
            off = self.s.m.bin(c, Bin::Add, at, off);
            index = None;
        }
        if used.is_some_and(|u| u.kind == ResourceType::Cbv) && !typed {
            // A constant buffer read per lane (a dynamically indexed array).
            // shortcut: the console's CBV descriptor is taken to have a 16-byte
            // stride, as the vector loads of whole rows suggest.
            let raw = self.constants_raw(site, src)?;
            let h = match raw {
                Some(h) => h,
                None => self.handle(site, src, false, (13, 0x1_0000))?,
            };
            let byte = match index {
                Some(i) => {
                    let sixteen = self.s.m.i32(16);
                    let rows = self.s.m.bin(c, Bin::Mul, i, sixteen);
                    self.s.m.bin(c, Bin::Add, rows, off)
                }
                None => off,
            };
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            for k in 0..count {
                let four = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, byte, four);
                let x = match raw {
                    Some(_) => self.raw_dwords(h, a, 1)[0],
                    None => self.cbuffer_dword(h, a),
                };
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        if self.root_address(src).is_some() {
            // Title memory by address: the byte in the host copy from here on.
            if let Some(element) = index {
                if used.is_none_or(|u| u.stride != 1) {
                    return Err("an indexed access through a root view by address");
                }
                off = self.s.m.bin(c, Bin::Add, element, off);
                index = None;
            }
            if let Some((_, byte)) = self.root_memory(src, off, guard, store || atomic.is_some()) {
                off = byte;
            }
        }
        let c = self.cur;
        let uav = store || atomic.is_some() || used.is_some_and(|u| u.kind == ResourceType::Uav);
        let mut u = if uav { 0x1000 } else { 0 };
        if uav && !typed && self.entry(src).is_some_and(|e| self.coherent.contains(&e)) {
            u |= 0x4000;
        }
        let iu = self.s.m.undef(self.s.t.i32);
        if let Some((bits, signed)) = narrow {
            // Below a dword: raw buffers are read and written in dwords on
            // 4 bytes, so the dword that holds the value is read and
            // shifted, or changed with two atomic operations that touch
            // only the value's own bits (other lanes may write the rest of
            // the dword at the same time). A 16-bit value at a dword's
            // last byte continues in the next dword.
            let byte = match index {
                None => off,
                Some(index) if used.is_some_and(|u| u.stride == 1) => {
                    self.s.m.bin(c, Bin::Add, index, off)
                }
                Some(_) => return Err("a byte or 16-bit buffer access by index"),
            };
            let h = self.handle(site, src, false, (11 | u, 0))?;
            let k3 = self.s.m.i32(3);
            let within = self.s.m.bin(c, Bin::And, byte, k3);
            let aligned = self.s.m.bin(c, Bin::Sub, byte, within);
            let shift = self.s.m.bin(c, Bin::Shl, within, k3);
            let four = self.s.m.i32(4);
            let next = self.s.m.bin(c, Bin::Add, aligned, four);
            let ones = self.s.m.i32(if bits == 8 { 0xff } else { 0xffff });
            let zero = self.s.m.i32(0);
            let k8 = self.s.m.i32(8);
            let straddles = self.s.m.cmp(c, pred::EQ, within, k3);
            if !store {
                let mask = self.s.m.i8(1);
                let align = self.s.m.i32(4);
                let dword = |l: &mut Self, at: V| {
                    let r = l.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        l.s.t.res_ret,
                        139,
                        &[h, at, iu, mask, align],
                    );
                    let f = l.s.m.extract(c, r, 0);
                    l.as_int(f)
                };
                let low = dword(self, aligned);
                let mut x = self.s.m.bin(c, Bin::LShr, low, shift);
                if bits == 16 {
                    let high = dword(self, next);
                    let high = self.s.m.bin(c, Bin::Shl, high, k8);
                    let high = self.s.m.select(c, straddles, high, zero);
                    x = self.s.m.bin(c, Bin::Or, x, high);
                }
                x = self.s.m.bin(c, Bin::And, x, ones);
                if signed {
                    let up = self.s.m.i32(32 - bits);
                    x = self.s.m.bin(c, Bin::Shl, x, up);
                    x = self.s.m.bin(c, Bin::AShr, x, up);
                }
                let Some(Operand::Vector { reg, .. }) = *data else {
                    return Err("a buffer result");
                };
                self.write(inst, Reg::V(reg), x, guard);
                return Ok(());
            }
            let d = data.as_ref().ok_or("buffer store data")?;
            let v = self.vgprs(inst, d, 1)?[0];
            let v = self.s.m.bin(c, Bin::And, v, ones);
            let all = self.s.m.i32(u32::MAX);
            let clear = self.s.m.bin(c, Bin::Shl, ones, shift);
            let keep = self.s.m.bin(c, Bin::Xor, clear, all);
            let set = self.s.m.bin(c, Bin::Shl, v, shift);
            let keep_next = self.s.m.i32(0xffff_ff00);
            let set_next = self.s.m.bin(c, Bin::LShr, v, k8);
            let (and, or) = (self.s.m.i32(1), self.s.m.i32(2));
            let i32t = self.s.t.i32;
            let at = self.begin_guarded(guard);
            for (op, x) in [(and, keep), (or, set)] {
                let args = [h, op, aligned, iu, iu, x];
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
            }
            if bits == 16 {
                let second = self.begin_guarded(Some(straddles));
                for (op, x) in [(and, keep_next), (or, set_next)] {
                    let args = [h, op, next, iu, iu, x];
                    self.s
                        .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args);
                }
                self.end_guarded(second, &[]);
            }
            self.end_guarded(at, &[]);
            return Ok(());
        }
        if let Some(op) = atomic {
            let d = data.as_ref().ok_or("buffer atomic data")?;
            let vals = self.vgprs(inst, d, if op == u32::MAX { 2 } else { 1 })?;
            let (h, coords) = match index {
                // A raw buffer (stride 1) read by index: the index is the byte.
                Some(index) if used.is_some_and(|u| u.stride == 1) => {
                    let byte = self.s.m.bin(c, Bin::Add, index, off);
                    (self.handle(site, src, false, (11 | u, 0))?, [byte, iu, iu])
                }
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    if stride < 4 || !stride.is_multiple_of(4) {
                        return Err("a structured buffer of unknown stride");
                    }
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    (h, [index, off, iu])
                }
                None => (self.handle(site, src, false, (11 | u, 0))?, [off, iu, iu]),
            };
            let Operand::Vector { reg, .. } = *d else {
                return Err("buffer atomic data");
            };
            let returns = matches!(inst.fields, Fields::Buffer { cache, .. } if cache.glc);
            let old = match inst.read(Reg::V(reg)) {
                Some(v) if returns => self.i32_of(v)?,
                _ => self.s.m.i32(0),
            };
            let at = self.begin_guarded(guard);
            let i32t = self.s.t.i32;
            let r = if op == u32::MAX {
                // cmpswap: data is (new, compare).
                let mut args = vec![h];
                args.extend(coords);
                args.extend([vals[1], vals[0]]);
                self.s.op(
                    self.cur,
                    "atomicCompareExchange.i32",
                    Attr::NoUnwind,
                    i32t,
                    79,
                    &args,
                )
            } else {
                let mut args = vec![h, self.s.m.i32(op)];
                args.extend(coords);
                args.push(vals[0]);
                self.s
                    .op(self.cur, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &args)
            };
            let r = self.end_guarded(at, &[(r, old)])[0];
            if returns {
                self.write(inst, Reg::V(reg), r, None);
            }
            return Ok(());
        }
        if store {
            let regs = if d16 { count.div_ceil(2) } else { count };
            let mut vals =
                self.vgprs(inst, data.as_ref().ok_or("buffer store data")?, regs as u8)?;
            if d16 {
                // d16 data is taken as f16, widened for the 32-bit float
                // store; through a view of an integer format it is a 16-bit
                // integer, zero- or sign-extended (VIctory 4243a96a: the face
                // network keeps its halves in UINT views; its output was
                // noise). Both `ResourceHeap` (`SLOT_STRIDES`) and `Table`
                // (`SLOT_TABLE_STRIDES`, dynamic table snapshots included)
                // populate the view-kind bits, while the handle stays a float
                // one (`0x409`), whose raw bits RTX and WARP pass bit-exact
                // without extra CFG blocks or touching non-d16 shaders.
                let integer = if typed { self.integer_view(src)? } else { None };
                let mut wide = Vec::new();
                for k in 0..count {
                    let mut x = vals[(k / 2) as usize];
                    if k % 2 == 1 {
                        let k16 = self.s.m.i32(16);
                        x = self.s.m.bin(c, Bin::LShr, x, k16);
                    }
                    let f32t = self.s.t.f32;
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x]);
                    let mut w = self.as_int(f);
                    if let Some((int, signed)) = integer {
                        let (k16, low) = (self.s.m.i32(16), self.s.m.i32(0xFFFF));
                        let zero_ext = self.s.m.bin(c, Bin::And, x, low);
                        let up = self.s.m.bin(c, Bin::Shl, x, k16);
                        let sign_ext = self.s.m.bin(c, Bin::AShr, up, k16);
                        let n = self.s.m.select(c, signed, sign_ext, zero_ext);
                        w = self.s.m.select(c, int, n, w);
                    }
                    wide.push(w);
                }
                vals = wide;
            }
            let mut args = Vec::new();
            let (op, name) = if typed {
                // shortcut: typed stores are taken to write all four channels
                // of a float format.
                if count != 4 || *offen || *offset != 0 {
                    return Err("a typed buffer store of some channels or at an offset");
                }
                args.push(self.handle(site, src, false, (10 | u, 0x409))?);
                args.extend([index.unwrap_or(self.s.m.i32(0)), iu]);
                for x in vals {
                    args.push(self.as_float(x));
                }
                (69, "bufferStore.f32")
            } else {
                match index {
                    // A raw buffer (stride 1) written by index: the index is the byte.
                    Some(index) if used.is_some_and(|u| u.stride == 1) => {
                        let byte = self.s.m.bin(c, Bin::Add, index, off);
                        args.push(self.handle(site, src, false, (11 | u, 0))?);
                        args.extend([byte, iu]);
                    }
                    Some(index) => {
                        let stride = used.map_or(0, |u| u.stride);
                        if stride < 4 || !stride.is_multiple_of(4) {
                            return Err("a structured buffer of unknown stride");
                        }
                        args.push(self.handle(site, src, false, (12 | u, stride))?);
                        args.extend([index, off]);
                    }
                    None => {
                        args.push(self.handle(site, src, false, (11 | u, 0))?);
                        args.extend([off, iu]);
                    }
                }
                args.extend(&vals);
                args.resize(3 + 4, iu);
                (140, "rawBufferStore.i32")
            };
            args.push(self.s.m.i8(if typed { 0xF } else { (1u8 << count) - 1 }));
            if !typed {
                args.push(self.s.m.i32(4));
            }
            let at = self.begin_guarded(guard);
            let void = self.s.t.void;
            self.s.op(self.cur, name, Attr::NoUnwind, void, op, &args);
            self.end_guarded(at, &[]);
            return Ok(());
        }
        if let Some(f) = *format {
            if index.is_some() {
                return Err("a typed-format load by index");
            }
            let h = self.handle(site, src, false, (11 | u, 0))?;
            let Some(Operand::Vector { reg, .. }) = *data else {
                return Err("a buffer result");
            };
            let mut xs = self.format_load(h, off, f, count)?;
            if d16 {
                xs = self.d16_channels(f, xs)?;
            }
            for (k, x) in xs.into_iter().enumerate() {
                self.write(inst, Reg::V(reg + k as u8), x, guard);
            }
            return Ok(());
        }
        let r = if typed {
            if *offen || matches!(off, V::Inst(_)) || *offset != 0 {
                return Err("a typed buffer load at an offset");
            }
            if uav {
                self.s.flags |= flags::TYPED_UAV_LOADS;
            }
            let h = self.handle(site, src, false, (10 | u, 0x409))?;
            let index = index.unwrap_or(self.s.m.i32(0));
            self.s.op(
                c,
                "bufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                68,
                &[h, index, iu],
            )
        } else {
            let mask = self.s.m.i8((1u8 << count) - 1);
            let align = self.s.m.i32(4);
            match index {
                // A raw buffer (stride 1) read by index: the index is the byte.
                Some(index) if used.is_some_and(|u| u.stride == 1) => {
                    let byte = self.s.m.bin(c, Bin::Add, index, off);
                    let h = self.handle(site, src, false, (11 | u, 0))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, byte, iu, mask, align],
                    )
                }
                Some(index) => {
                    let stride = used.map_or(0, |u| u.stride);
                    if stride < 4 || !stride.is_multiple_of(4) {
                        return Err("a structured buffer of unknown stride");
                    }
                    let h = self.handle(site, src, false, (12 | u, stride))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, index, off, mask, align],
                    )
                }
                None => {
                    let h = self.handle(site, src, false, (11 | u, 0))?;
                    self.s.op(
                        c,
                        "rawBufferLoad.f32",
                        Attr::ReadOnly,
                        self.s.t.res_ret,
                        139,
                        &[h, off, iu, mask, align],
                    )
                }
            }
        };
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a buffer result");
        };
        if d16 {
            // Two f16 channels per VGPR (an odd last high half 0); the low
            // 16 bits of an integer view's (see the stores above).
            let integer = if typed { self.integer_view(src)? } else { None };
            let k16 = self.s.m.i32(16);
            let i32t = self.s.t.i32;
            for k in 0..count.div_ceil(2) {
                let mut x = None;
                for ch in (2 * k..(2 * k + 2).min(count)).rev() {
                    let f = self.s.m.extract(c, r, ch);
                    let mut h = self
                        .s
                        .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f]);
                    if let Some((int, _)) = integer {
                        let bits = self.as_int(f);
                        let low = self.s.m.i32(0xFFFF);
                        let low = self.s.m.bin(c, Bin::And, bits, low);
                        h = self.s.m.select(c, int, low, h);
                    }
                    x = Some(match x {
                        Some(hi) => {
                            let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                            self.s.m.bin(c, Bin::Or, h, hi)
                        }
                        None => h,
                    });
                }
                self.write(inst, Reg::V(reg + k as u8), x.expect("a channel"), guard);
            }
            return Ok(());
        }
        for k in 0..count {
            let x = self.s.m.extract(c, r, k);
            let x = self.as_int(x);
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }

    pub(super) fn export(&mut self, b: usize, i: usize) -> Result<(), Why> {
        let e = *self
            .exports
            .get(&(b, i))
            .ok_or("an export I/O raising did not see")?;
        if e.compressed {
            return Err("a compressed export");
        }
        let guard = self.guard(b, i)?;
        let mesh = self.s.mesh.is_some();
        let c = self.cur;
        if let Some(g) = guard
            && !mesh
        {
            if !(e.done && self.s.pixel) {
                return Err("a conditional export");
            }
            let t = self.s.m.bool(true);
            let kill = self.s.m.bin(c, Bin::Xor, g, t);
            self.s
                .op(c, "discard", Attr::NoUnwind, self.s.t.void, 82, &[kill]);
        }
        if mesh && e.target == Target::Primitive {
            return self.emit_indices(e, guard);
        }
        let param_row = |usage: Usage, row: u8| match usage {
            // The 2020 compiler leaves a mesh program's first entry zeroed
            // (usage "position"): its row is the one no other entry names.
            Usage::Other(0) if mesh => self.unnamed_row(),
            _ => Some(row),
        };
        // (output element, channel) per exported channel; per-primitive
        // outputs are elements of `prims`.
        let mut stores = Vec::new();
        let primitive = matches!(e.target, Target::PrimitiveParam { .. });
        let outputs = if primitive {
            &self.s.prims
        } else {
            &self.s.outputs
        };
        let by_kind = |kind: u8| outputs.iter().position(|o| o.kind == kind);
        // 16-bit vertex outputs: the halves each dword carries, as (output
        // element, channel, high half) per channel.
        let halves = match e.target {
            Target::Param { usage, row } => {
                param_row(usage, row).and_then(|r| self.half_outputs.get(&r).map(|&h| (r, h)))
            }
            _ => None,
        };
        if let Some((row, (pack2, _))) = halves {
            let packs: Vec<(u8, Option<u8>)> = self
                .half_outputs
                .iter()
                .filter(|(_, h)| h.0)
                .map(|(&r, h)| (r, h.1))
                .collect();
            for ch in 0..4u8 {
                let Some(value) = e.values[usize::from(ch)] else {
                    continue;
                };
                for high in [false, true] {
                    let h = 2 * ch + u8::from(high);
                    let src = if pack2 {
                        packed_half(outputs, &packs, row, h)
                    } else {
                        (!high).then_some((row, ch))
                    };
                    let found = src.and_then(|(r, c)| {
                        outputs.iter().position(|o| {
                            o.row == Some(u32::from(r)) && c >= o.col && c < o.col + o.cols
                        })
                    });
                    if let Some(id) = found {
                        let col = src.expect("found").1 - outputs[id].col;
                        stores.push((id, col, value, Some(high)));
                    }
                }
            }
        }
        for ch in 0..4u8 {
            let Some(value) = e.values[usize::from(ch)] else {
                continue;
            };
            if halves.is_some() {
                break;
            }
            let found = match e.target {
                Target::Color(n) => outputs
                    .iter()
                    .position(|o| o.kind == 16 && o.row == Some(u32::from(n)))
                    .map(|id| (id, ch)),
                Target::Position(0) => by_kind(3).map(|id| (id, ch)),
                Target::Depth => match ch {
                    0 => by_kind(17)
                        .or(by_kind(18))
                        .or(by_kind(19))
                        .map(|id| (id, 0)),
                    1 => by_kind(20).map(|id| (id, 0)),
                    2 => by_kind(14).map(|id| (id, 0)),
                    _ => None,
                },
                Target::Param { usage, row } | Target::PrimitiveParam { usage, row } => {
                    let row = param_row(usage, row);
                    outputs
                        .iter()
                        .position(|o| {
                            row.is_some_and(|r| o.row == Some(u32::from(r)))
                                && ch >= o.col
                                && ch < o.col + o.cols
                        })
                        .map(|id| (id, ch - outputs[id].col))
                }
                Target::Null | Target::Unread => None,
                Target::Position(n @ 1..=2) => {
                    // shortcut: without the misc vector (point size, render
                    // target or viewport index) pos1 and pos2 carry the clip
                    // then cull distances, in semantic order.
                    if outputs.iter().any(|o| matches!(o.kind, 4 | 5)) {
                        return Err("render-target or viewport index exports");
                    }
                    let mut dists: Vec<usize> = (0..outputs.len())
                        .filter(|&i| matches!(outputs[i].kind, 6 | 7))
                        .collect();
                    dists.sort_by_key(|&i| (outputs[i].kind, outputs[i].index));
                    let flat: Vec<(usize, u8)> = dists
                        .iter()
                        .flat_map(|&i| (0..outputs[i].cols).map(move |c| (i, c)))
                        .collect();
                    // Channels past the distances the signature declares are
                    // ones the clipper does not enable (`PA_CL_VS_OUT_CNTL`).
                    let k = usize::from(n - 1) * 4 + usize::from(ch);
                    flat.get(k).copied()
                }
                Target::Position(_) => return Err("clip, cull or point-size exports"),
                Target::Primitive => return Err("a primitive export"),
            };
            if let Some((id, col)) = found {
                stores.push((id, col, value, None));
            }
        }
        fn elements<'l>(l: &'l mut Lower, primitive: bool) -> &'l mut Vec<SigElement> {
            if primitive {
                &mut l.s.prims
            } else {
                &mut l.s.outputs
            }
        }
        if stores
            .iter()
            .any(|s| s.3.is_none() && elements(self, primitive)[s.0].half)
        {
            return Err("a 16-bit output the stage data does not mark");
        }
        // A mesh lane writes its own vertex or primitive, where it exports.
        let at = if mesh {
            let flat = self.mesh_thread();
            let limit = if let Some(m) = self.s.mesh {
                if primitive {
                    m.max_primitives
                } else {
                    m.max_vertices
                }
            } else {
                0
            };
            let threads = self.s.threads.map(|t| t[0]).unwrap_or(u32::MAX);
            let mesh_guard = if limit > 0 && threads > limit {
                let lim = self.s.m.i32(limit);
                let in_bounds = self.s.m.cmp(self.cur, pred::IULT, flat, lim);
                match guard {
                    Some(g) => Some(self.s.m.bin(self.cur, Bin::And, g, in_bounds)),
                    None => Some(in_bounds),
                }
            } else {
                guard
            };
            self.begin_guarded(mesh_guard)
        } else {
            None
        };
        let (name, opcode) = match (mesh, primitive) {
            (false, _) => ("storeOutput", 5),
            (true, false) => ("storeVertexOutput", 171),
            (true, true) => ("storePrimitiveOutput", 172),
        };
        for (id, col, value, half) in stores {
            let mut x = self.i32_of(value)?;
            if let Some(high) = half {
                // An f16 half, declared a 32-bit float on the host.
                let c = self.cur;
                if high {
                    let k16 = self.s.m.i32(16);
                    x = self.s.m.bin(c, Bin::LShr, x, k16);
                }
                let f32t = self.s.t.f32;
                let f = self
                    .s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[x]);
                x = self.as_int(f);
            }
            let el = &mut elements(self, primitive)[id];
            el.usage |= 1 << col;
            let (suffix, x) = match el.comp {
                9 if half.is_some() => (".f32", self.as_float(x)),
                9 => {
                    let f = match self.floats.get(&value) {
                        Some(&f) => f,
                        None => self.as_float(x),
                    };
                    (".f32", f)
                }
                _ => (".i32", x),
            };
            let mut args = vec![
                self.s.m.i32(id as u32),
                self.s.m.i32(0),
                self.s.m.i8(col),
                x,
            ];
            if mesh {
                args.push(self.mesh_thread());
            }
            let void = self.s.t.void;
            self.s.op(
                self.cur,
                &format!("{name}{suffix}"),
                Attr::NoUnwind,
                void,
                opcode,
                &args,
            );
        }
        self.end_guarded(at, &[]);
        Ok(())
    }

    /// Mesh: `exp prim` as the lane's primitive. Each corner takes ten
    /// bits: a 9-bit vertex index and, above it, its edge flag (bits 9, 19
    /// and 29), which only the console's edge-flag rasterisation reads and
    /// which is no part of the index. Bit 31 culls it (all indices 0: no
    /// area).
    fn emit_indices(&mut self, e: &Export, guard: Option<V>) -> Result<(), Why> {
        let v = e.values[0].ok_or("a primitive export without its indices")?;
        let x = self.i32_of(v)?;
        let flat = self.mesh_thread();
        let guard = if let Some(mesh) = self.s.mesh {
            let threads = self.s.threads.map(|t| t[0]).unwrap_or(u32::MAX);
            if threads > mesh.max_primitives {
                let p = self.s.m.i32(mesh.max_primitives);
                let in_bounds = self.s.m.cmp(self.cur, pred::IULT, flat, p);
                match guard {
                    Some(g) => Some(self.s.m.bin(self.cur, Bin::And, g, in_bounds)),
                    None => Some(in_bounds),
                }
            } else {
                guard
            }
        } else {
            guard
        };
        let at = self.begin_guarded(guard);
        let c = self.cur;
        let z = self.s.m.i32(0);
        let mut culled = self.s.m.cmp(c, pred::SLT, x, z);
        // A primitive that names a vertex past the declared count, or one
        // no lane writes (lane n writes vertex n alone), is culled too: the
        // host would read a vertex nothing set, at any position.
        let written = self.s.mesh.map(|m| {
            m.max_vertices
                .min(self.s.threads.map_or(u32::MAX, |t| t[0]))
        });
        let mut indices = [z; 3];
        for (k, slot) in indices.iter_mut().enumerate() {
            let (shift, mask) = (self.s.m.i32(10 * k as u32), self.s.m.i32(0x1FF));
            let i = self.s.m.bin(c, Bin::LShr, x, shift);
            let i = self.s.m.bin(c, Bin::And, i, mask);
            if let Some(n) = written {
                let n = self.s.m.i32(n);
                let past = self.s.m.cmp(c, pred::IUGE, i, n);
                culled = self.s.m.bin(c, Bin::Or, culled, past);
            }
            *slot = i;
        }
        let mut args = vec![flat];
        for i in indices {
            args.push(self.s.m.select(c, culled, z, i));
        }
        let void = self.s.t.void;
        self.s
            .op(c, "emitIndices", Attr::NoUnwind, void, 169, &args);
        self.end_guarded(at, &[]);
        Ok(())
    }

    /// The per-vertex arbitrary row no mesh output entry names but one
    /// (the 2020 compiler's zeroed entry).
    fn unnamed_row(&self) -> Option<u8> {
        let StageIo::Ms(ms) = &self.x.native.io else {
            return None;
        };
        let named = |r: u32| {
            ms.outputs
                .iter()
                .any(|o| o.param & 0x80 == 0 && o.usage != Usage::Other(0) && u32::from(o.row) == r)
        };
        let free: std::collections::BTreeSet<u32> = self
            .s
            .outputs
            .iter()
            .filter(|o| o.kind == 0)
            .filter_map(|o| o.row)
            .filter(|&r| !named(r))
            .collect();
        match free.into_iter().collect::<Vec<_>>()[..] {
            [r] => u8::try_from(r).ok(),
            _ => None,
        }
    }

    pub(super) fn exit(&mut self, b: usize) -> Result<(), Stop> {
        let offset = self.x.p.blocks[b]
            .insts
            .last()
            .map_or(self.x.p.blocks[b].offset, |i| i.machine.offset);
        let term = match self.x.st.exits[b] {
            LaneExit::Jump(t) if self.counters.contains_key(&t) && self.rpo[t] <= self.rpo[b] => {
                let ok = self.count_iteration(t, pred::IULT);
                let abort = self.abort_block();
                Term::CondBr(ok, self.block[t].expect("reachable"), abort)
            }
            LaneExit::Jump(t) => {
                self.enter_loop(b, t, self.cur);
                Term::Br(self.block[t].expect("reachable"))
            }
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                // A negated condition branches the other way round.
                let (cond, taken, not_taken) = match self.x.st.bools.node(when) {
                    _ if self.x.st.wide.contains(&b) => {
                        let c = self
                            .wave_wide(b, when)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                    Some((atom, Bool::TRUE, Bool::FALSE)) => {
                        let a = self
                            .atom(atom, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (a, not_taken, taken)
                    }
                    _ => {
                        let c = self
                            .boolean(when, self.cur)
                            .map_err(|w| unsupported(offset, w))?;
                        (c, taken, not_taken)
                    }
                };
                // A back edge is taken only below the iteration limit.
                let back =
                    |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
                // (No `xor` of a compare: a loop exit testing one hung the
                // AMD driver.)
                let cond = if back(self, taken) {
                    let ok = self.count_iteration(taken, pred::IULT);
                    self.s.m.bin(self.cur, Bin::And, cond, ok)
                } else if back(self, not_taken) {
                    let stop = self.count_iteration(not_taken, pred::IUGE);
                    self.s.m.bin(self.cur, Bin::Or, cond, stop)
                } else {
                    cond
                };
                // Critical edges get a block of their own, so loops have
                // dedicated exits and preheaders as DXC emits them.
                let (t, f) = (self.edge(b, taken), self.edge(b, not_taken));
                for (to, via) in [(taken, t), (not_taken, f)] {
                    if !back(self, to) {
                        // From an edge block, or from here.
                        let from = if Some(via) == self.block[to] {
                            self.cur
                        } else {
                            via
                        };
                        self.enter_loop(b, to, from);
                    }
                }
                Term::CondBr(cond, t, f)
            }
            LaneExit::End => {
                self.flush_meet_totals();
                if self.s.pixel && !self.exported[b] && !self.has_done_export(b) {
                    // A lane leaving without its final export was killed.
                    let t = self.s.m.bool(true);
                    self.s
                        .op(self.cur, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
                }
                Term::Ret
            }
            LaneExit::Indirect => return Err(unsupported(offset, "an indirect jump")),
        };
        let _ = Exit::End;
        self.s.m.terminate(self.cur, term);
        Ok(())
    }

    /// A forward edge into loop header `to` (from DXIL block `via`) starts
    /// its count at 0.
    fn enter_loop(&mut self, _from: usize, to: usize, via: usize) {
        if let Some((_, incoming)) = self.counters.get_mut(&to) {
            let z = self.s.m.i32(0);
            incoming.push((z, via));
        }
    }

    /// A back edge to header `to` (a back edge keeps its block, no
    /// critical-edge block): the count goes up; the result compares it with
    /// [`LOOP_LIMIT`] by `p` (below: go on; at or above: stop).
    fn count_iteration(&mut self, to: usize, p: u8) -> V {
        let c = self.cur;
        let (phi, _) = self.counters[&to];
        let one = self.s.m.i32(1);
        let next = self.s.m.bin(c, Bin::Add, phi, one);
        self.s.m.tag(phi, Tag::Counter);
        self.s.m.tag(next, Tag::Counter);
        // Per stage unless the switch names one: a loop that never meets its
        // exit (a count read from a buffer the host has not got yet, say)
        // runs to the limit in every lane. A million iterations a pixel
        // hung the GPU for seconds a draw until Windows reset it
        // (PS_Textured_Eight, 2026-11-24, run 12).
        // shortcut: a pixel loop past 128 iterations, a vertex or mesh loop
        // past 256 or a compute loop past 4,096 is cut short. (They were
        // 1,024, 4,096 and 65,536: the opening cutscene then lost the card
        // 14-25 frames in, in a minimap or shadow mesh draw; with every
        // loop cut at 64 it ran 90 frames. Raise them again once shaders
        // stop reading data that is not there yet.)
        let limit = match super::super::loop_limit() {
            LOOP_LIMIT if self.s.pixel => 1 << 7,
            LOOP_LIMIT if self.s.mesh.is_some() || self.s.threads.is_none() => 1 << 8,
            LOOP_LIMIT => 1 << 12,
            named => named,
        };
        let limit = self.s.m.i32(limit);
        self.counters
            .get_mut(&to)
            .expect("header")
            .1
            .push((next, c));
        let over = self.s.m.cmp(c, p, next, limit);
        self.s.m.tag(over, Tag::Counter);
        over
    }

    /// The block a lane stopped by [`LOOP_LIMIT`] leaves through.
    pub(super) fn abort_block(&mut self) -> usize {
        if let Some(a) = self.abort {
            return a;
        }
        let a = self.s.m.new_block();
        if self.s.pixel {
            let t = self.s.m.bool(true);
            self.s
                .op(a, "discard", Attr::NoUnwind, self.s.t.void, 82, &[t]);
        }
        self.s.m.terminate(a, Term::Ret);
        self.abort = Some(a);
        a
    }

    /// The DXIL block a branch from IR block `from` to `to` goes to.
    pub(super) fn edge(&mut self, from: usize, to: usize) -> usize {
        let target = self.block[to].expect("reachable");
        let preds = self.x.p.blocks[to]
            .preds
            .iter()
            .filter(|&&q| self.block[q].is_some())
            .count();
        // Back edges stay as they are: the exiting block is the latch.
        if preds < 2 || self.rpo[to] <= self.rpo[from] {
            return target;
        }
        let e = self.s.m.new_block();
        self.s.m.terminate(e, Term::Br(target));
        self.edges.insert((from, to), e);
        e
    }

    pub(super) fn fill_phis(&mut self) -> Result<(), Stop> {
        let p = self.x.p;
        // Lane-mask joins first (what an edge brings may ask for more joins,
        // of masks and of numbers): each edge's mask as the structure
        // tracked it, a constant or a scalar-built one included.
        let st = self.x.st;
        while let Some((v, b, i)) = self.pending_masks.pop() {
            let phi = &p.blocks[b].phis[i];
            let offset = p.blocks[b].offset;
            let args = st
                .phi_masks
                .get(&phi.value)
                .ok_or_else(|| unsupported(offset, "a lane-mask join of an untracked mask"))?;
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let x = self
                    .boolean(args[n], qb)
                    .map_err(|w| unsupported(offset, w))?;
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        // A join that only another join reads is read wherever that one is. Left
        // unmarked, a value without a DXIL form coming into it became zero without a
        // word (the looked-up tint of the material resolve programs, whose size
        // computation had no value, was zero in every lane that way).
        // shortcut: only with `VICTORY_STRICT_JOINS` set (the offline pack
        // listing sets it to count them): 40 programs that translate today
        // stop by name with it (vehicle paint pixel shaders at "the first
        // half of an interpolation", foliage mesh shaders); until those are
        // lowered they keep drawing with that zero.
        static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let strict = *STRICT.get_or_init(|| std::env::var_os("VICTORY_STRICT_JOINS").is_some());
        let mut work: Vec<Value> = if strict {
            self.read_phis.iter().copied().collect()
        } else {
            Vec::new()
        };
        while let Some(v) = work.pop() {
            let Some(&(b, i)) = self.phi_at.get(&v) else {
                continue;
            };
            for &a in &p.blocks[b].phis[i].args {
                if self.phi_at.contains_key(&a) && self.read_phis.insert(a) {
                    work.push(a);
                }
            }
        }
        for (v, b, i) in std::mem::take(&mut self.data_phis) {
            let phi = &p.blocks[b].phis[i];
            let bit = phi.reg == Reg::Scc;
            // A join of numbers with dead registers (`dead`): the paths that bring those
            // never read the result, so they bring zero. Not for a join that is itself a
            // lane mask (a ballot gives that one as data).
            let st = self.x.st;
            let numbers = !bit
                && !st.masks.contains_key(&phi.value)
                && !st.highs.contains_key(&phi.value)
                && phi
                    .args
                    .iter()
                    .any(|a| matches!(self.vals.get(a), Some(Val::I32(_))));
            let mut incoming = Vec::new();
            for (n, &q) in p.blocks[b].preds.iter().enumerate() {
                let Some(qb) = self.tail[q] else { continue };
                let qb = self.edges.get(&(q, b)).copied().unwrap_or(qb);
                let arg = phi.args[n];
                if self.read_phis.contains(&phi.value) && !self.vals.contains_key(&arg) {
                    let _ = self.lazy_barycentric(arg);
                    // A root parameter nothing read before the join (user
                    // data is loaded on first use, in the entry block).
                    if !self.vals.contains_key(&arg)
                        && let Some(&(param, dword)) = self.params.get(&arg)
                    {
                        let val = self.user(param, dword);
                        self.vals.insert(arg, val);
                    }
                }
                let x = match (self.vals.get(&arg).copied(), bit) {
                    (Some(Val::I32(x)), false) | (Some(Val::Bit(x)), true) => x,
                    (Some(Val::Missing(_)) | None, false)
                        if numbers && self.dead(arg, &mut Vec::new()) =>
                    {
                        self.s.m.i32(0)
                    }
                    // A lane mask joined with numbers (a traversal that keeps its lanes' masks on
                    // a stack with its node numbers): the mask comes as the number it is, the
                    // ballot of its lanes where it is written.
                    (Some(Val::Missing(_)) | None, false)
                        if self.read_phis.contains(&phi.value)
                            && self.mask_data == MaskData::Console
                            && (st.masks.contains_key(&arg) || st.highs.contains_key(&arg)) =>
                    {
                        match self.mask_dword(arg) {
                            Ok(x) => x,
                            Err(why) => return Err(Stop::Retry(phi.value, why)),
                        }
                    }
                    // SCC of a mask operation ("some lane is left") joined with a scalar test:
                    // the compiler branches around code that sets SCC and tests SCC after it,
                    // the skip bringing "no lane" along. As data it is "some lane of the wave
                    // has the bit" - right only if the wave took that skip together, as the
                    // console does: a lane that skipped alone would bring "some lane" where the
                    // console has the skipped code's SCC. So the branch must be one the wave
                    // takes together; else its region is named for `emit`'s retry to widen it.
                    (Some(Val::Missing(_)) | None, true)
                        if self.read_phis.contains(&phi.value)
                            && self.mask_data == MaskData::Console
                            && st.scc_some.contains_key(&arg) =>
                    {
                        let per_lane = !st.wide.contains(&q)
                            && matches!(st.exits[q], LaneExit::Branch { when, .. }
                                if !st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))));
                        if per_lane {
                            for &other in p.blocks[b].preds.iter().filter(|&&o| o != q) {
                                need(other);
                            }
                            return Err(Stop::Retry(phi.value, LANE_MASK));
                        }
                        match self.scc_some(arg) {
                            Ok(x) => x,
                            Err(why) => return Err(Stop::Retry(phi.value, why)),
                        }
                    }
                    (Some(Val::Bit(_)), false) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "an SCC joined with SGPR values"));
                    }
                    (Some(Val::Missing(why)), _) if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, why));
                    }
                    _ if self.read_phis.contains(&phi.value) => {
                        return Err(Stop::Retry(phi.value, "a join of mismatched values"));
                    }
                    _ if bit => self.s.m.bool(false),
                    _ => self.s.m.i32(0),
                };
                incoming.push((x, qb));
            }
            self.s.m.set_phi(v, incoming);
        }
        for (phi, incoming) in std::mem::take(&mut self.counters).into_values() {
            self.s.m.set_phi(phi, incoming);
        }
        Ok(())
    }
}
