//! Lowers console addresses, descriptors and scalar-memory accesses to host views.

use super::*;

impl<'a> Lower<'a> {
    /// The kind of a view no instruction uses (in this program, or through
    /// another load of its address), by what the code reads of the
    /// descriptor the load `inst` at `site` writes to the SGPRs from `code`:
    /// `Some(true)` for a 2D image, read only for its size, `Some(false)`
    /// for a structured buffer, read only for its record count.
    ///
    /// - Eight dwords are an image's. `GetDimensions` of a 2D one reads the
    ///   width (dwords 1 and 2), the height (dword 2, bits 14-29, with one
    ///   `s_bfe_u32`) and the base level or the last (dword 3); of an array,
    ///   a cube or a volume also dword 4.
    /// - Four dwords at a table entry the used resources list as an SRV or
    ///   UAV are a buffer's. A stride above 4 bytes there is an element's
    ///   (a typed or raw view has at most 4): a test for a null view reads
    ///   the record count, nothing else.
    pub(super) fn sized_view(&self, inst: &Inst, site: Site, code: u8, count: u32) -> Option<bool> {
        let dword = |k: u8| inst.write(Reg::S(code + k));
        let read = |v: Value| {
            self.x.p.blocks.iter().any(|b| {
                b.insts.iter().any(|i| i.reads.iter().any(|r| r.1 == v))
                    || b.phis.iter().any(|p| p.args.contains(&v))
            })
        };
        let only = |allowed: &[u8]| {
            (0..count as u8).all(|k| allowed.contains(&k) || !dword(k).is_some_and(read))
        };
        match count {
            8 => {
                const HEIGHT: u32 = 16 << 16 | 14;
                let v = dword(2)?;
                let height = self.x.p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
                    i.name() == "s_bfe_u32"
                        && i.reads.iter().any(|r| r.1 == v)
                        && matches!(&i.fields, Fields::Sop { src, .. }
                            if matches!(src.get(1), Some(Operand::Literal(HEIGHT))))
                });
                (height && only(&[1, 2, 3])).then_some(true)
            }
            4 => {
                let used = self.used_kind(&Source::Loaded {
                    load: site,
                    first: 0,
                })?;
                let a = self.scalar.get(&site)?;
                let buffer = used.dwords == 4
                    && a.dynamic.is_none()
                    && used.table_offset_dwords == a.offset / 4
                    && matches!(used.kind, ResourceType::Srv | ResourceType::Uav)
                    && used.stride > 4
                    && used.stride & 3 == 0;
                (buffer && only(&[2])).then_some(false)
            }
            _ => None,
        }
    }

    /// The title address (low dword, bits from 32 up) of the buffer view
    /// whose descriptor the scalar load at `site` reads from the start of
    /// a table entry, as the host tells it when it binds (`shader.rs`
    /// `USER_VIEW_ADDRESS`); none for anything else. (The console's
    /// descriptor holds more in the second dword, the stride from bit 16
    /// up; code that takes the address masks those away.)
    pub(super) fn view_address(&mut self, site: Site) -> Option<(V, V)> {
        use crate::emit::shader::{SLOT_STRIDES_COUNT, SLOT_VIEW_ADDRESSES, USER_VIEW_ADDRESS};
        let a = self.scalar.get(&site).copied()?;
        let rp = *self.x.native.layout.get(a.param)?;
        if a.offset % 32 != 0 {
            return None;
        }
        if rp.root == RootType::Table && a.dynamic.is_none() {
            let dword = a.offset / 4;
            let used = self
                .x
                .native
                .used
                .iter()
                .filter(|u| {
                    u.root == RootType::Table
                        && u.user_data == rp.user_data
                        && u.table_offset_dwords <= dword
                })
                .max_by_key(|u| u.table_offset_dwords)?;
            if used.dwords != 4 || !matches!(used.kind, ResourceType::Srv | ResourceType::Uav) {
                return None;
            }
            let lo = self.host_word(RootType::Other(USER_VIEW_ADDRESS), rp.slot, dword);
            let hi = self.host_word(RootType::Other(USER_VIEW_ADDRESS + 1), rp.slot, dword);
            return Some((lo, hi));
        }
        if !matches!(rp.root, RootType::Table | RootType::ResourceHeap) {
            return None;
        }
        let slot = self.heap_index(site, 0, false).ok()?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let last = self.s.m.i32(SLOT_STRIDES_COUNT - 1);
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let three = self.s.m.i32(3);
        let byte = self.s.m.bin(c, Bin::Shl, slot, three);
        let h = self.s.root_srv_handle(SLOT_VIEW_ADDRESSES);
        let iu = self.s.m.undef(i32t);
        let mask = self.s.m.i8(3);
        let align = self.s.m.i32(4);
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, byte, iu, mask, align],
        );
        let f0 = self.s.m.extract(c, r, 0);
        let f1 = self.s.m.extract(c, r, 1);
        let lo = self.as_int(f0);
        let hi = self.as_int(f1);
        Some((lo, hi))
    }

    /// `lo`, `hi` plus the instruction's byte offset `off`, with the carry.
    pub(super) fn address_plus(&mut self, lo: V, hi: V, off: i32) -> (V, V) {
        if off == 0 {
            return (lo, hi);
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let k = self.s.m.i32(off as u32);
        let sum = self.s.m.bin(c, Bin::Add, lo, k);
        // Up: a carry when the sum wrapped below `lo`. Down: a borrow when
        // it wrapped above.
        let wrapped = self
            .s
            .m
            .cmp(c, if off > 0 { pred::IULT } else { pred::IUGT }, sum, lo);
        let one = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let hi = self
            .s
            .m
            .bin(c, if off > 0 { Bin::Add } else { Bin::Sub }, hi, one);
        (sum, hi)
    }

    /// Title memory at the 40-bit address `lo`, `hi` (`shader.rs`
    /// `SLOT_MEMORY`): the handle of the raw view of the host copy holding
    /// it and the byte in that view. A page without a copy reads zeros
    /// (and drops writes) through the null view at the first slot, and its
    /// number is left for the host, for lanes that run (`guard`). Goes on in
    /// another block.
    /// With `store`, a page that is not to be written ([`crate::emit::shader::MEMORY_READ_ONLY`])
    /// is the null view.
    pub(super) fn memory(&mut self, lo: V, hi: V, guard: Option<V>, store: bool) -> (V, V) {
        use crate::emit::shader::{
            MEMORY_FAULTS, MEMORY_READ_ONLY, SLOT_FAULTS, SLOT_MEMORY, USER_MEMORY_BASE,
        };
        let c = self.cur;
        let (i32t, res) = (self.s.t.i32, self.s.t.res_ret);
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        let (k16, k8) = (k(self, 16), k(self, 0xFF));
        let low = self.s.m.bin(c, Bin::LShr, lo, k16);
        let high = self.s.m.bin(c, Bin::And, hi, k8);
        let high = self.s.m.bin(c, Bin::Shl, high, k16);
        let page = self.s.m.bin(c, Bin::Or, low, high);
        // The eight high address bits and sixteen page bits from `lo`
        // already fit the full 24-bit page directory.
        let k3 = k(self, 3);
        let at = self.s.m.bin(c, Bin::Shl, page, k3);
        let directory = self.s.root_srv_handle(SLOT_MEMORY);
        let (iu, both, align) = (self.s.m.undef(i32t), self.s.m.i8(3), k(self, 4));
        let entry = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            res,
            139,
            &[directory, at, iu, both, align],
        );
        let (copy, start) = (self.s.m.extract(c, entry, 0), self.s.m.extract(c, entry, 1));
        let (number, start) = (self.as_int(copy), self.as_int(start));
        let mask = k(self, !MEMORY_READ_ONLY);
        let copy = self.s.m.bin(c, Bin::And, number, mask);
        // A page without a copy: its number, at a place by the number. In a
        // branch: the notes lie in the PC's memory, and a store there with
        // every node of every ray would be most of what a trace does.
        let zero = k(self, 0);
        let mut missing = self.s.m.cmp(c, pred::EQ, copy, zero);
        if let Some(g) = guard {
            missing = self.s.m.bin(c, Bin::And, missing, g);
        }
        let places = k(self, MEMORY_FAULTS - 1);
        let faults = self.s.root_uav_handle(SLOT_FAULTS);
        let noting = self.begin_guarded(Some(missing));
        let c = self.cur;
        let place = self.s.m.bin(c, Bin::URem, page, places);
        let one = k(self, 1);
        let place = self.s.m.bin(c, Bin::Add, place, one);
        let k2 = k(self, 2);
        let place = self.s.m.bin(c, Bin::Shl, place, k2);
        let void = self.s.t.void;
        let mask = self.s.m.i8(1);
        self.s.op(
            c,
            "rawBufferStore.i32",
            Attr::NoUnwind,
            void,
            140,
            &[faults, place, iu, page, iu, iu, iu, mask, align],
        );
        self.end_guarded(noting, &[]);
        let c = self.cur;
        self.page_there = Some(self.s.m.cmp(c, pred::NE, copy, zero));
        let copy = if store {
            let closed = self.s.m.cmp(c, pred::SLT, number, zero);
            self.s.m.select(c, closed, zero, copy)
        } else {
            copy
        };
        let base = self.host_word(RootType::Other(USER_MEMORY_BASE), 0, 0);
        let slot = self.s.m.bin(c, Bin::Add, base, copy);
        // The builder's threads read what others of the dispatch wrote (a
        // refit climbs from children to parents): globally coherent.
        let coherent = if crate::spill::is_spilled(self.x.native) {
            0x4000
        } else {
            0
        };
        let h = self
            .s
            .heap_handle(c, slot, false, true, (11 | 0x1000 | coherent, 0));
        let k64k = k(self, 0xFFFF);
        let within = self.s.m.bin(c, Bin::And, lo, k64k);
        let byte = self.s.m.bin(c, Bin::Add, start, within);
        (h, byte)
    }

    /// The title address in the root view `src` is built around, in a program
    /// with a spilled user-data table (`shader.rs` `USER_ROOT_ADDRESS`).
    pub(super) fn root_address(&mut self, src: &Source) -> Option<(V, V)> {
        use crate::emit::shader::USER_ROOT_ADDRESS;
        let Source::Built { address, .. } = src else {
            return None;
        };
        let rp = *self.x.native.layout.get(address.param)?;
        if rp.root != RootType::Descriptor || !crate::spill::is_spilled(self.x.native) {
            return None;
        }
        let used = self
            .x
            .native
            .used
            .iter()
            .find(|u| u.user_data == rp.user_data)?;
        if !matches!(used.kind, ResourceType::Srv | ResourceType::Uav) {
            return None;
        }
        let lo = self.host_word(RootType::Other(USER_ROOT_ADDRESS), rp.slot, 0);
        let hi = self.host_word(RootType::Other(USER_ROOT_ADDRESS + 1), rp.slot, 0);
        Some((lo, hi))
    }

    /// For an access through such a root view: the raw view of the host copy
    /// and the byte in it of the view's byte `off` (signed: an offset below
    /// the address stays below it). [`Lower::handle`] gives the same view for
    /// the rest of the instruction. None for any other source. Goes on in
    /// another block.
    /// shortcut: an access of several dwords is taken not to cross the end
    /// of a host copy (the builder's records lie on their size).
    pub(super) fn root_memory(
        &mut self,
        src: &Source,
        off: V,
        guard: Option<V>,
        store: bool,
    ) -> Option<(V, V)> {
        let (lo, hi) = self.root_address(src)?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let sum = self.s.m.bin(c, Bin::Add, lo, off);
        let wrapped = self.s.m.cmp(c, pred::IULT, sum, lo);
        let carry = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let k31 = self.s.m.i32(31);
        let sign = self.s.m.bin(c, Bin::AShr, off, k31);
        let hi = self.s.m.bin(c, Bin::Add, hi, carry);
        let mut hi = self.s.m.bin(c, Bin::Add, hi, sign);
        let mut sum = sum;
        if crate::emit::builder_bounds() {
            // No structure is near 64 MiB (the greatest seen has 14). One
            // whose memory was written over says it has gigabytes, and the
            // builder's copy of it went through all other structures' memory
            // after it and, past 2 GiB, before it. Nothing is there instead.
            let (below, all, zero) = (
                self.s.m.i32(16 << 20),
                self.s.m.i32(80 << 20),
                self.s.m.i32(0),
            );
            let at = self.s.m.bin(c, Bin::Add, off, below);
            let near = self.s.m.cmp(c, pred::IULT, at, all);
            sum = self.s.m.select(c, near, sum, zero);
            hi = self.s.m.select(c, near, hi, zero);
        }
        let (h, byte) = self.memory(sum, hi, guard, store);
        self.root_copy = Some(h);
        Some((h, byte))
    }

    /// `global_load_*`, `global_store_*` and `global_atomic_*` on title
    /// memory by address (`resource::MEMORY`): the address is the VGPR
    /// pair, each lane its own.
    pub(super) fn memory_flat(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            offset,
            lds,
            cache,
        } = &inst.fields
        else {
            unreachable!()
        };
        if *lds || !matches!(saddr, None | Some(Operand::Scalar { code: NULL, .. })) {
            return Err("a global address with a scalar base");
        }
        let Some(Operand::Vector { reg: areg, .. }) = *addr else {
            return Err("a global address not in VGPRs");
        };
        let word = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst.read(Reg::V(areg + k)).ok_or("a global address")?;
            l.i32_of(v)
        };
        let (lo, hi) = (word(self, 0)?, word(self, 1)?);
        let (lo, hi) = self.address_plus(lo, hi, i32::from(*offset));
        let guard = self.guard(b, i)?;
        let (i32t, res, void) = (self.s.t.i32, self.s.t.res_ret, self.s.t.void);
        let iu = self.s.m.undef(i32t);
        let align = self.s.m.i32(4);
        let name = inst.name();
        let count = |rest: &str| match rest {
            "dword" => Ok(1u8),
            "dwordx2" => Ok(2),
            "dwordx3" => Ok(3),
            "dwordx4" => Ok(4),
            _ => Err(name),
        };
        if let Some(rest) = name.strip_prefix("global_load_") {
            let n = count(rest)?;
            let Some(Operand::Vector { reg, .. }) = *dst else {
                return Err("a global load result");
            };
            let (h, byte) = self.memory(lo, hi, guard, false);
            let c = self.cur;
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                res,
                139,
                &[h, byte, iu, mask, align],
            );
            for k in 0..n {
                let x = self.s.m.extract(c, r, u32::from(k));
                let x = self.as_int(x);
                self.write(inst, Reg::V(reg + k), x, guard);
            }
            return Ok(());
        }
        if let Some(rest) = name.strip_prefix("global_store_") {
            let n = count(rest)?;
            let d = data.as_ref().ok_or("global store data")?;
            let vals = self.vgprs(inst, d, n)?;
            let (h, byte) = self.memory(lo, hi, guard, true);
            let mut args = vec![h, byte, iu];
            args.extend(&vals);
            args.resize(3 + 4, iu);
            args.push(self.s.m.i8((1u8 << n) - 1));
            args.push(align);
            let at = self.begin_guarded(guard);
            self.s.op(
                self.cur,
                "rawBufferStore.i32",
                Attr::NoUnwind,
                void,
                140,
                &args,
            );
            self.end_guarded(at, &[]);
            return Ok(());
        }
        let Some(op) = name.strip_prefix("global_atomic_") else {
            return Err(name);
        };
        let d = data.as_ref().ok_or("global atomic data")?;
        let v = self.vgprs(inst, d, 1)?[0];
        // With `glc` the value from before comes back, in the result
        // register the instruction names.
        let back = match (*dst, cache.glc) {
            (Some(Operand::Vector { reg, .. }), true) => Some(reg),
            _ => None,
        };
        let old = match back.and_then(|reg| inst.read(Reg::V(reg))) {
            Some(x) => self.i32_of(x)?,
            None => self.s.m.i32(0),
        };
        let (h, byte) = self.memory(lo, hi, guard, true);
        let c = self.cur;
        // DXIL's atomic operations: add 0, and 1, or 2, xor 3, signed
        // minimum 4 and maximum 5, unsigned 6 and 7, exchange 8.
        let simple = match op {
            "add" => Some((0, v)),
            "sub" => {
                let zero = self.s.m.i32(0);
                Some((0, self.s.m.bin(c, Bin::Sub, zero, v)))
            }
            "and" => Some((1, v)),
            "or" => Some((2, v)),
            "xor" => Some((3, v)),
            "smin" => Some((4, v)),
            "smax" => Some((5, v)),
            "umin" => Some((6, v)),
            "umax" => Some((7, v)),
            "swap" => Some((8, v)),
            "fmin" | "fmax" => None,
            _ => return Err(name),
        };
        let at = self.begin_guarded(guard);
        let was = match simple {
            Some((code, x)) => {
                let code = self.s.m.i32(code);
                self.s.op(
                    self.cur,
                    "atomicBinOp.i32",
                    Attr::NoUnwind,
                    i32t,
                    78,
                    &[h, code, byte, iu, iu, x],
                )
            }
            None => {
                // A float minimum or maximum has no atomic operation on
                // the host: the dword is exchanged for the smaller (larger)
                // of itself and the value until no other thread got in
                // between. Each failed pass means another one's succeeded;
                // the limit only keeps a broken card from hanging.
                let head = self.cur;
                let mask = self.s.m.i8(1);
                let first = self.s.op(
                    head,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    res,
                    139,
                    &[h, byte, iu, mask, align],
                );
                let first = self.s.m.extract(head, first, 0);
                let first = self.as_int(first);
                let (pass, done) = (self.s.m.new_block(), self.s.m.new_block());
                self.s.m.terminate(head, Term::Br(pass));
                self.cur = pass;
                let seen = self.s.m.phi(pass, i32t);
                let n = self.s.m.phi(pass, i32t);
                let (f32t, fv) = (self.s.t.f32, self.as_float(v));
                let fseen = self.as_float(seen);
                // `dx.op.binary`: FMax 35, FMin 36.
                let pick = if op == "fmin" { 36 } else { 35 };
                let wanted =
                    self.s
                        .op(pass, "binary.f32", Attr::ReadNone, f32t, pick, &[fseen, fv]);
                let wanted = self.as_int(wanted);
                let found = self.s.op(
                    pass,
                    "atomicCompareExchange.i32",
                    Attr::NoUnwind,
                    i32t,
                    79,
                    &[h, byte, iu, iu, seen, wanted],
                );
                let one = self.s.m.i32(1);
                let n1 = self.s.m.bin(pass, Bin::Add, n, one);
                let lost = self.s.m.cmp(pass, pred::NE, found, seen);
                let limit = self.s.m.i32(1 << 16);
                let more = self.s.m.cmp(pass, pred::IULT, n1, limit);
                let again = self.s.m.bin(pass, Bin::And, lost, more);
                self.s.m.terminate(pass, Term::CondBr(again, pass, done));
                let zero = self.s.m.i32(0);
                self.s.m.set_phi(seen, vec![(first, head), (found, pass)]);
                self.s.m.set_phi(n, vec![(zero, head), (n1, pass)]);
                self.cur = done;
                found
            }
        };
        let was = self.end_guarded(at, &[(was, old)])[0];
        if let Some(reg) = back {
            self.write(inst, Reg::V(reg), was, None);
        }
        Ok(())
    }

    /// `s_load_dword*` from title memory by address (`resource::MEMORY`):
    /// the SGPR pair plus the instruction's offsets.
    pub(super) fn memory_smem(&mut self, inst: &Inst) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            base: Operand::Scalar { code: base, .. },
            offset,
            soffset,
            ..
        } = inst.fields
        else {
            return Err("a scalar load from memory");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u8,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar memory access by address"),
        };
        let word = |l: &mut Self, k: u8| -> Result<V, Why> {
            let v = inst
                .read(Reg::S(base + k))
                .ok_or("an address not in SGPRs")?;
            l.i32_of(v)
        };
        let (mut lo, mut hi) = (word(self, 0)?, word(self, 1)?);
        let c = self.cur;
        let (i32t, res) = (self.s.t.i32, self.s.t.res_ret);
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            // An offset register counts up from the address.
            let so = self.operand(inst, &soffset)?;
            let sum = self.s.m.bin(c, Bin::Add, lo, so);
            let carry = self.s.m.cmp(c, pred::IULT, sum, lo);
            let carry = self.s.m.cast(c, Cast::ZExt, carry, i32t);
            hi = self.s.m.bin(c, Bin::Add, hi, carry);
            lo = sum;
        }
        let (iu, align) = (self.s.m.undef(i32t), self.s.m.i32(4));
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4);
            let (lo, hi) = self.address_plus(lo, hi, offset + 4 * i32::from(first));
            let (h, byte) = self.memory(lo, hi, None, false);
            let c = self.cur;
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                res,
                139,
                &[h, byte, iu, mask, align],
            );
            for k in 0..n {
                let x = self.s.m.extract(c, r, u32::from(k));
                let x = self.as_int(x);
                self.write(inst, Reg::S(code + first + k), x, None);
            }
        }
        Ok(())
    }

    /// Whether the dwords of one console load are read by one call for up
    /// to four of them (`bitcode::opt::WIDE` of `VICTORY_SHADER_OPT`), not
    /// by a call each.
    pub(super) fn wide_loads() -> bool {
        crate::emit::shader_opt() & crate::emit::bitcode::opt::WIDE != 0
    }

    /// `count` dwords from byte `addr` on of the raw view `h`, as integers,
    /// read in the current block by one call for every four of them
    /// ([`Self::wide_loads`]). They are the dwords of ONE console load,
    /// which the console reads as one (address plus 4, 8, ... without
    /// wrapping at 32 bits): the same bytes as a call a dword reads. Loads
    /// of different console instructions are never joined: the program
    /// computes their offsets in 32-bit registers, and `x - 4` and `x` at
    /// `x = 0` are one dword out of the view and its first one, where one
    /// call from `x - 4` on would read both out of it.
    pub(super) fn raw_dwords(&mut self, h: V, addr: V, count: u32) -> Vec<V> {
        let c = self.cur;
        let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let mut out = Vec::with_capacity(count as usize);
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4);
            let off = self.s.m.i32(4 * first);
            let a = self.s.m.bin(c, Bin::Add, addr, off);
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, a, u, mask, al],
            );
            for k in 0..n {
                let x = self.s.m.extract(c, r, k);
                out.push(self.as_int(x));
            }
        }
        out
    }

    pub(super) fn smem(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        let Fields::Smem {
            data,
            offset,
            soffset,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        self.root_copy = None;
        let name = inst.name();
        if let Some(a) = self.scalar.get(&site).copied().filter(|a| a.param == CODE) {
            return self.code_smem(inst, a);
        }
        if self.scalar.get(&site).is_some_and(|a| a.param == MEMORY) {
            return self.memory_smem(inst);
        }
        let Some(rest) = name.strip_prefix("s_buffer_load_dword") else {
            return self.descriptor_data(inst, site);
        };
        let count = match rest {
            "" => 1,
            "x2" => 2,
            "x4" => 4,
            "x8" => 8,
            "x16" => 16,
            _ => return Err("a scalar buffer load"),
        };
        let Some(Operand::Scalar { code, .. }) = *data else {
            return Err("a scalar buffer load");
        };
        let src = self
            .descriptors
            .get(&(site, false))
            .copied()
            .ok_or("an untracked descriptor")?;
        let kind = self.used_kind(src);
        let c = self.cur;
        let (base, extra) = self.built_offset(src)?;
        let offset = &((*offset as u32).wrapping_add(base) as i32);
        let mut addr = self.s.m.i32(*offset as u32);
        if let Some(x) = extra {
            addr = self.s.m.bin(c, Bin::Add, addr, x);
        }
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(inst, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        if matches!(src, Source::Built { address, .. } if address.param == CODE) {
            // A descriptor built around a table in the code.
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let x = self.code_dword(a)?;
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        // A buffer the program picks from the view heap by an index it
        // computes (`ResourceHeap`) has no used-resource entry, and the
        // heap holds a buffer view there, not a constant buffer: read as
        // raw bytes, as a table's SRV is below. Read as a constant buffer
        // (the host's validation: "Descriptor Type in Heap: SRV, Register
        // Type: CBV", heap index 17) the loads gave nothing:
        // the title's meshlet culling (BatchCullMeshlets,
        // BatchCullPreSkinnedMeshlets, BatchCullBatchCSMeshlets) reads its
        // batch records, mesh headers and instance transforms this way, so
        // it passed no meshlet and every indirect mesh draw ran empty.
        // The host's table of strides by heap slot tells a buffer view
        // (not 0) from anything else, which is still read as a constant
        // buffer; only the load that fits the slot runs.
        let picked = match kind {
            None => self.slot_stride(src, false)?,
            Some(_) => None,
        };
        if let Some(stride) = picked {
            let zero = self.s.m.i32(0);
            let is_buffer = self.s.m.cmp(self.cur, pred::NE, stride, zero);
            let at = self.begin_guarded(Some(is_buffer));
            let h = self.handle(site, src, false, (11, 0))?;
            let mut raw = Vec::new();
            let wide = Self::wide_loads();
            if wide {
                let dwords = self.raw_dwords(h, addr, count);
                raw.extend(dwords.into_iter().map(|x| (x, zero)));
            }
            for k in 0..if wide { 0 } else { count } {
                let b = self.cur;
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(b, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    b,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(b, r, 0);
                let x = self.as_int(x);
                raw.push((x, zero));
            }
            let raw = self.end_guarded(at, &raw);
            let other = self.s.m.cmp(self.cur, pred::EQ, stride, zero);
            let at = self.begin_guarded(Some(other));
            // (Renderer port 5: the twin of a constant buffer's slot is a
            // raw view of its bytes, read as one.)
            let as_raw =
                crate::emit::shader::constants_raw() && crate::emit::shader::raw_views() != 0;
            let h = if as_raw {
                self.handle(site, src, false, (11, 0))?
            } else {
                self.handle(site, src, false, (13, 0x1_0000))?
            };
            let mut both = Vec::new();
            let dwords = if as_raw {
                self.raw_dwords(h, addr, raw.len() as u32)
            } else {
                Vec::new()
            };
            for (k, &r) in raw.iter().enumerate() {
                let b = self.cur;
                let x = match dwords.get(k) {
                    Some(&x) => x,
                    None => {
                        let off = self.s.m.i32(4 * k as u32);
                        let a = self.s.m.bin(b, Bin::Add, addr, off);
                        self.cbuffer_dword(h, a)
                    }
                };
                both.push((x, r));
            }
            let both = self.end_guarded(at, &both);
            for (k, x) in both.into_iter().enumerate() {
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        if matches!(kind, Some(u) if u.kind == ResourceType::Srv) {
            // The size in a structure's header (its second dword), which a
            // copy of it loops over.
            let size = *offset == 4
                && count == 1
                && extra.is_none()
                && matches!(soffset, Operand::Scalar { code: NULL, .. })
                && crate::emit::builder_bounds();
            let mut root = false;
            let h = match self.root_memory(src, addr, None, false) {
                Some((h, byte)) => {
                    addr = byte;
                    root = true;
                    h
                }
                None => self.handle(site, src, false, (11, 0))?,
            };
            if Self::wide_loads() {
                for (k, x) in self.raw_dwords(h, addr, count).into_iter().enumerate() {
                    self.write(inst, Reg::S(code + k as u8), x, None);
                }
                return Ok(());
            }
            let c = self.cur;
            for k in 0..count {
                let off = self.s.m.i32(4 * k);
                let a = self.s.m.bin(c, Bin::Add, addr, off);
                let mask = self.s.m.i8(1);
                let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
                let r = self.s.op(
                    c,
                    "rawBufferLoad.f32",
                    Attr::ReadOnly,
                    self.s.t.res_ret,
                    139,
                    &[h, a, u, mask, al],
                );
                let x = self.s.m.extract(c, r, 0);
                let mut x = self.as_int(x);
                if size && root {
                    let (most, zero) = (self.s.m.i32(1 << 20), self.s.m.i32(0));
                    let sane = self.s.m.cmp(c, pred::IULT, x, most);
                    x = self.s.m.select(c, sane, x, zero);
                }
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        if let Some(h) = self.constants_raw(site, src)? {
            for (k, x) in self.raw_dwords(h, addr, count).into_iter().enumerate() {
                self.write(inst, Reg::S(code + k as u8), x, None);
            }
            return Ok(());
        }
        let h = self.handle(site, src, false, (13, 0x1_0000))?;
        let constant = match addr {
            V::Const(_)
                if matches!(soffset, Operand::Scalar { code: NULL, .. }) && extra.is_none() =>
            {
                Some(*offset as u32)
            }
            _ => None,
        };
        let mut rows: HashMap<u32, V> = HashMap::new();
        for k in 0..count {
            let x = match constant {
                Some(base) => {
                    let byte = base + 4 * k;
                    let row = match rows.get(&(byte >> 4)) {
                        Some(&r) => r,
                        None => {
                            let ri = self.s.m.i32(byte >> 4);
                            let r = self.s.op(
                                c,
                                "cbufferLoadLegacy.i32",
                                Attr::ReadOnly,
                                self.s.t.cbuf_ret,
                                59,
                                &[h, ri],
                            );
                            rows.insert(byte >> 4, r);
                            r
                        }
                    };
                    self.s.m.extract(c, row, (byte >> 2) & 3)
                }
                None => {
                    let off = self.s.m.i32(4 * k);
                    let a = self.s.m.bin(c, Bin::Add, addr, off);
                    self.cbuffer_dword(h, a)
                }
            };
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// A descriptor load (`s_load_dword*` from a table or heap) whose dwords
    /// the code also reads as numbers: the console compiler computes sizes
    /// from descriptor bits (an image's width, height, depth, levels; a
    /// buffer's record count) instead of querying them. Those fields are
    /// rebuilt from the host view's dimensions in the GFX10 layout (image
    /// dword 1 bits 30-31 and dword 2 bits 0-13 width - 1, dword 2 bits
    /// 14-29 height - 1, dword 3 identity swizzle, last level at 16, type at
    /// 28, dword 4 depth or layers - 1; buffer dword 2 the record count).
    // shortcut: the base level reads 0 (the host view starts at its own
    // level 0, so sizes the code shifts by it come out right) and the other
    // dwords (addresses, formats, tiling) have no host value: reading them
    // stops the program.
    pub(super) fn descriptor_data(&mut self, inst: &Inst, site: Site) -> Result<(), Why> {
        const OTHER: Why = "a descriptor's address or format read as data";
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar memory read as data");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar memory read as data"),
        };
        // How the program uses the descriptor tells the view's kind: an image or buffer
        // instruction before a scalar buffer load, which reads any buffer.
        let x = self.x;
        let uses = |of: Site| {
            x.res
                .descriptors
                .iter()
                .filter(|d| d.role == Role::Resource)
                .filter_map(move |d| {
                    d.source.leaves().into_iter().find_map(|l| match *l {
                        Source::Loaded { load, first } if load == of => Some((d.site, first)),
                        _ => None,
                    })
                })
        };
        let by_scalar_load =
            |u: &(Site, u8)| matches!(x.p.blocks[u.0.0].insts[u.0.1].fields, Fields::Smem { .. });
        let use_of = |of: Site| {
            uses(of)
                .find(|u| !by_scalar_load(u))
                .or_else(|| uses(of).next())
        };
        // The console compiler loads a descriptor again wherever it needs it, and for a
        // size it reads the dwords of a load that nothing uses as a descriptor (a texture
        // picked from the heap by an index held per lane: one load for its width, one for
        // its base level, one for each read of the image). Another load of as many dwords
        // from the same address reads the same descriptor, so what uses that one tells
        // this one's kind. The handle is still made from this load's own operands, which
        // hold here.
        let address = self.scalar.get(&site).copied();
        let twin = || {
            let a = address?;
            let same = |s: Site| x.p.blocks[s.0].insts[s.1].name() == inst.name();
            x.res
                .scalar
                .iter()
                .filter(|(s, b)| *s != site && *b == a && same(*s))
                .find_map(|(s, _)| use_of(*s))
        };
        let use_at = use_of(site).or_else(twin);
        // A view whose kind no use tells, but what is read of it does.
        let sized = match use_at {
            Some(_) => None,
            None => self.sized_view(inst, site, code, count),
        };
        // A descriptor nothing uses as one, whose size the code reads all the same (a
        // texture picked from the heap by an index in a constant buffer, its width and
        // height turned into a texel size): the kind from its dwords and its used-resource
        // entry. Only an image whose layers or depth (dword 4) are not read: those are no 2D
        // texture's; and no buffer `sized_view` did not take (a stride of 4 or less may be a
        // typed view's component count, and the address is `view_address`'s below).
        let layers = inst
            .write(Reg::S(code + 4))
            .is_some_and(|v| self.read_as_number(v));
        let unused = if use_at.is_none() && sized.is_none() && count == 8 && !layers {
            self.unused_descriptor_kind(inst, site, code, count)
        } else {
            None
        };
        let (first, props, image, uav, used) = if let Some(kind) = unused {
            kind
        } else {
            let Some((at, first)) = use_at.or(sized.map(|_| (site, 0))) else {
                // A descriptor kept per lane in VGPRs (`Resources::lane_copied`): the code only
                // copies its dwords and compares them, lane against first lane, and every load
                // that reaches those lanes reads one address. Each dword stands in as its place
                // in the table, so the lanes compare equal as they do on the console.
                if self.x.res.lane_copied.contains(&site)
                    && let Some(a) = self.scalar.get(&site).copied()
                {
                    for k in 0..count {
                        let x = self.s.m.i32(a.offset / 4 + k);
                        self.write(inst, Reg::S(code + k as u8), x, None);
                    }
                    return Ok(());
                }
                // A sampler's descriptor: no program reads its dwords as numbers, so where one
                // meets numbers at a join the register is dead on that path (`dead`), as a
                // descriptor's address or format is.
                let sampler = x.res.descriptors.iter().any(|d| {
                    d.role == Role::Sampler
                        && d.source
                            .leaves()
                            .into_iter()
                            .any(|l| matches!(*l, Source::Loaded { load, .. } if load == site))
                });
                if sampler {
                    self.stale.extend(inst.writes.iter().map(|w| w.1));
                    return Err("a sampler's descriptor read as data");
                }
                // A buffer view the code only takes the address of, to walk
                // the memory behind it (a ray-tracing structure): the address
                // from the host, the other dwords have no value.
                if count <= 4
                    && let Some((lo, hi)) = self.view_address(site)
                {
                    for k in 0..count {
                        let reg = Reg::S(code + k as u8);
                        match k {
                            0 => self.write(inst, reg, lo, None),
                            1 => self.write(inst, reg, hi, None),
                            _ => {
                                if let Some(v) = inst.write(reg) {
                                    self.vals.insert(v, Val::Missing(OTHER));
                                    self.stale.insert(v);
                                }
                            }
                        }
                    }
                    return Ok(());
                }
                if std::env::var_os("VICTORY_DESC_WHY").is_some() {
                    eprintln!(
                        "DESC-WHY {:#x} {} scalar={:?} lane_copied={}",
                        inst.machine.offset,
                        inst.name(),
                        address,
                        self.x.res.lane_copied.contains(&site)
                    );
                }
                return Err("a descriptor read only as data");
            };
            let src = Source::Loaded { load: site, first };
            let used = self.used_kind(&src);
            let ui: &Inst = &self.x.p.blocks[at.0].insts[at.1];
            let un = ui.name();
            let uav = un.contains("store")
                || un.contains("atomic")
                || used.is_some_and(|u| u.kind == ResourceType::Uav);
            let u = if uav { 0x1000 } else { 0 };
            let (props, image) = match &ui.fields {
                _ if sized.is_some_and(|image| image) => ((2 | u, 0x409), Some(2)),
                _ if sized.is_some() => ((11 | u, 0), None),
                // A ray-tracing structure in a buffer of the host: read raw.
                Fields::Image { .. } if un.starts_with("image_bvh") => ((11, 0), None),
                Fields::Image { dim, .. } => {
                    let kind = match dim {
                        0 => 1,
                        1 => 2,
                        2 => 4,
                        4 => 6,
                        5 => 7,
                        _ => return Err("an image dimension (cube or MSAA)"),
                    };
                    ((kind | u, 0x409), Some(kind))
                }
                Fields::Buffer { idxen, format, .. } => {
                    let stride = used.map_or(0, |u| u.stride);
                    let counted = inst
                        .write(Reg::S(code + first + 2))
                        .is_some_and(|v| self.read_as_number(v));
                    let props = if un.starts_with("buffer_load_format")
                        || un.starts_with("buffer_store_format")
                    {
                        (10 | u, 0x409)
                    } else if !counted
                        && *idxen
                        && format.is_none()
                        && stride >= 4
                        && stride & 3 == 0
                    {
                        // Nothing reads the count: the query is cleaned up.
                        (12 | u, stride)
                    } else {
                        // Structured ones too: the host's views of them are raw
                        // (see `buffer`), which count bytes, and a size asked
                        // for as a structured view's is that count on AMD.
                        (11 | u, 0)
                    };
                    if used.is_some_and(|u| u.kind == ResourceType::Cbv) {
                        self.stale.extend(inst.writes.iter().map(|w| w.1));
                        return Err("a constant buffer's descriptor read as data");
                    }
                    (props, None)
                }
                // Read by scalar buffer loads alone: the raw view `smem` reads it through.
                Fields::Smem { .. } if used.is_some_and(|u| u.kind == ResourceType::Srv) => {
                    ((11, 0), None)
                }
                // A constant buffer: nothing of its descriptor is a number.
                Fields::Smem { .. } => {
                    self.stale.extend(inst.writes.iter().map(|w| w.1));
                    return Err("a constant buffer's descriptor read as data");
                }
                _ => return Err("a scalar memory read as data"),
            };
            (first, props, image, uav, used)
        };
        let src = Source::Loaded { load: site, first };
        let h = self.handle(site, &src, false, props)?;
        let c = self.cur;
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let mip = if image.is_some() && !uav {
            self.s.m.i32(0)
        } else {
            self.s.m.undef(i32t)
        };
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, mip]);
        let field = |l: &mut Self, k: u32| l.s.m.extract(c, d, k);
        let words: Vec<(u32, V)> = match image {
            Some(kind) => {
                let one = self.s.m.i32(1);
                let w = field(self, 0);
                let (height, depth) = match kind {
                    1 => (one, one),
                    2 => (field(self, 1), one),
                    6 => (one, field(self, 1)),
                    _ => (field(self, 1), field(self, 2)),
                };
                let levels = if uav { one } else { field(self, 3) };
                let less = |l: &mut Self, x: V| l.s.m.bin(c, Bin::Sub, x, one);
                let (w, height, depth, levels) = (
                    less(self, w),
                    less(self, height),
                    less(self, depth),
                    less(self, levels),
                );
                let k = |l: &mut Self, v: u32| l.s.m.i32(v);
                let (k30, k2, k14, k16, mask) = (
                    k(self, 30),
                    k(self, 2),
                    k(self, 14),
                    k(self, 16),
                    k(self, 0x3FFF),
                );
                let d1 = self.s.m.bin(c, Bin::Shl, w, k30);
                let hi = self.s.m.bin(c, Bin::LShr, w, k2);
                let hi = self.s.m.bin(c, Bin::And, hi, mask);
                let ht = self.s.m.bin(c, Bin::Shl, height, k14);
                let d2 = self.s.m.bin(c, Bin::Or, hi, ht);
                let ty = [0, 8, 9, 0, 10, 0, 12, 13][kind as usize];
                let base = k(self, 0xFAC | ty << 28);
                let last = self.s.m.bin(c, Bin::Shl, levels, k16);
                let d3 = self.s.m.bin(c, Bin::Or, base, last);
                vec![(1, d1), (2, d2), (3, d3), (4, depth)]
            }
            None => {
                // A raw view counts bytes; the descriptor's record count is in elements.
                let n = field(self, 0);
                let stride = used.map_or(0, |u| u.stride);
                let counted = inst
                    .write(Reg::S(code + first + 2))
                    .is_some_and(|v| self.read_as_number(v));
                let n = if props.0 & 0xFF == 11 && stride > 1 {
                    let k = self.s.m.i32(stride);
                    self.s.m.bin(c, Bin::UDiv, n, k)
                } else if props.0 & 0xFF == 11
                    && counted
                    && let Some(stride) = self.slot_stride(&src, false)?
                {
                    // Picked from the heap: the host's table has the stride.
                    let (i32t, one) = (self.s.t.i32, self.s.m.i32(1));
                    let k = self
                        .s
                        .op(c, "binary.i32", Attr::ReadNone, i32t, 39, &[stride, one]);
                    self.s.m.bin(c, Bin::UDiv, n, k)
                } else {
                    n
                };
                let mut words = vec![(2, n)];
                // Its address too, where the code computes with it and the
                // host can tell it.
                let computed = (0..2u8).any(|k| {
                    inst.write(Reg::S(code + first + k))
                        .is_some_and(|v| self.computed_with(v, 6))
                });
                // (A descriptor nothing uses as one gets it whenever the host can tell it, as
                // a buffer whose address alone is read does.)
                if first == 0
                    && (computed || unused.is_some())
                    && let Some((lo, hi)) = self.view_address(site)
                {
                    words.extend([(0, lo), (1, hi)]);
                }
                words
            }
        };
        for k in 0..count {
            let rel = k.wrapping_sub(u32::from(first));
            match words.iter().find(|w| w.0 == rel) {
                Some(&(_, x)) => self.write(inst, Reg::S(code + k as u8), x, None),
                None => {
                    if let Some(v) = inst.write(Reg::S(code + k as u8)) {
                        self.vals.insert(v, Val::Missing(OTHER));
                        self.stale.insert(v);
                    }
                }
            }
        }
        Ok(())
    }

    /// What [`Self::descriptor_data`] makes of a descriptor load that nothing uses as a
    /// descriptor but whose dwords the code reads: the first dword, the handle properties,
    /// the image kind (none for a buffer), whether it is an unordered view, and its
    /// used-resource entry. An image's eight dwords read as a 2D texture's, a buffer's four
    /// as a raw view's where the code reads its record count (dword 2); none for anything
    /// else (a constant buffer, a sampler, a buffer whose address alone is read).
    // shortcut: an image whose kind nothing tells is taken for a 2D texture (the title's
    // reads are texel sizes of 2D textures: puddle masks, shadow masks); a host view of
    // another kind would answer its own sizes.
    #[allow(clippy::type_complexity)]
    pub(super) fn unused_descriptor_kind(
        &self,
        inst: &Inst,
        site: Site,
        code: u8,
        count: u32,
    ) -> Option<(u8, (u32, u32), Option<u32>, bool, Option<&'a UsedResource>)> {
        // Kept per lane and compared (`Resources::lane_copied`), or a sampler's: the cases
        // `descriptor_data` handles on their own.
        if self.x.res.lane_copied.contains(&site)
            || self.x.res.descriptors.iter().any(|d| {
                d.role == Role::Sampler
                    && d.source
                        .leaves()
                        .into_iter()
                        .any(|l| matches!(*l, Source::Loaded { load, .. } if load == site))
            })
        {
            return None;
        }
        let used = self.used_kind(&Source::Loaded {
            load: site,
            first: 0,
        });
        if used.is_some_and(|u| !matches!(u.kind, ResourceType::Srv | ResourceType::Uav)) {
            return None;
        }
        let read = |k: u32| {
            inst.write(Reg::S(code + k as u8)).is_some_and(|v| {
                self.x
                    .p
                    .blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .any(|i| i.reads.iter().any(|r| r.1 == v))
            })
        };
        let uav = used.is_some_and(|u| u.kind == ResourceType::Uav);
        let u = if uav { 0x1000 } else { 0 };
        match count {
            // width (dwords 1-2), height (2), levels (3), depth (4)
            8 if (1..5).any(read) && used.is_none_or(|u| u.dwords == 8) => {
                Some((0, (2 | u, 0x409), Some(2), uav, used))
            }
            // the record count
            4 if read(2) && used.is_none_or(|u| u.dwords == 4) => {
                Some((0, (11 | u, 0), None, uav, used))
            }
            _ => None,
        }
    }

    /// `s_load_dword*` from the program's constant table.
    pub(super) fn code_smem(&mut self, inst: &Inst, a: Address) -> Result<(), Why> {
        let Fields::Smem {
            data: Some(Operand::Scalar { code, .. }),
            ..
        } = inst.fields
        else {
            return Err("a scalar load");
        };
        let count = match inst.name().strip_prefix("s_load_dword") {
            Some("") => 1u32,
            Some("x2") => 2,
            Some("x4") => 4,
            Some("x8") => 8,
            Some("x16") => 16,
            _ => return Err("a scalar load from the code"),
        };
        let c = self.cur;
        let mut addr = self.s.m.i32(a.offset);
        if let Some(value) = a.dynamic {
            let mut d = self.i32_of(value)?;
            // A term held per lane reached the address through
            // `v_readfirstlane_b32`: the first lane's.
            let per_lane = self.def_block.get(&value).is_some_and(|&b| {
                let writes = |i: &Inst| {
                    i.writes
                        .iter()
                        .any(|w| w.1 == value && matches!(w.0, Reg::V(_)))
                };
                self.x.p.blocks[b].insts.iter().any(writes)
            });
            if per_lane {
                self.s.flags |= flags::WAVE_OPS;
                let i32t = self.s.t.i32;
                let first = self
                    .s
                    .op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[d]);
                d = if self.s.pixel && crate::emit::helper_own() {
                    let helper = self.helper_lane();
                    self.s.m.select(c, helper, d, first)
                } else {
                    first
                };
            }
            addr = self.s.m.bin(c, Bin::Add, addr, d);
        }
        for k in 0..count {
            let off = self.s.m.i32(4 * k);
            let at = self.s.m.bin(c, Bin::Add, addr, off);
            let x = self.code_dword(at)?;
            self.write(inst, Reg::S(code + k as u8), x, None);
        }
        Ok(())
    }

    /// Renderer port 5 (`VICTORY_PORT_CBV=1`, [`crate::emit::shader::constants_raw`]):
    /// the handle a constant buffer `src` is read through as raw bytes, or
    /// `None` to read it as a constant buffer, as before.
    ///
    /// The console takes constant buffers on 16 bytes, the host on 256: the
    /// layer copied each one the title's tables name into upload memory of
    /// its own at every submission (some 23,000 a frame of the city, out of
    /// the title's graphics memory, which renderer port 1 has the card read
    /// in place), and each root one not on 256 bytes (64 KiB each). A raw
    /// view starts on 16 bytes and a root SRV anywhere on 4, so read so the
    /// card can take the title's bytes where they are:
    /// - a table's (or one picked from the view heap): through the raw twin
    ///   of its slot ([`crate::emit::shader::raw_views`]), which the host makes a
    ///   raw view of the same bytes; a twin's reads end where the view does,
    ///   as a constant buffer's do;
    /// - a root one: as the root SRV `t<slot>` in [`crate::emit::shader::ROOT_CBV_SPACE`],
    ///   which the host binds at the title's address like any root SRV.
    ///
    /// Anything else (a descriptor built otherwise, or chosen between kinds of
    /// parameter) stays a constant buffer.
    pub(super) fn constants_raw(&mut self, site: Site, src: &Source) -> Result<Option<V>, Why> {
        if !crate::emit::shader::constants_raw() {
            return Ok(None);
        }
        if let Source::Built { address, .. } = src {
            if self.root_copy.is_some() {
                return Ok(None);
            }
            let Some(&rp) = self.x.native.layout.get(address.param) else {
                return Ok(None);
            };
            let kind = self
                .x
                .native
                .used
                .iter()
                .find(|u| u.user_data == rp.user_data)
                .map(|u| u.kind);
            if rp.root == RootType::Descriptor && matches!(kind, Some(ResourceType::Cbv) | None) {
                return Ok(Some(self.s.root_srv_handle(rp.slot)));
            }
            return Ok(None);
        }
        if crate::emit::shader::raw_views() == 0 {
            return Ok(None);
        }
        let x = self.x;
        let scalar = &self.scalar;
        let through_heap = src.leaves().iter().all(|leaf| match leaf {
            Source::Loaded { load, .. } | Source::Fetched { load, .. } => scalar
                .get(load)
                .and_then(|a| x.native.layout.get(a.param))
                .is_some_and(|p| matches!(p.root, RootType::Table | RootType::ResourceHeap)),
            _ => false,
        });
        if !through_heap {
            return Ok(None);
        }
        // (Raw, so `raw_twin` gives the twin's index.)
        Ok(Some(self.handle(site, src, false, (11, 0))?))
    }

    /// The dword at a computed byte address of a constant buffer.
    pub(super) fn cbuffer_dword(&mut self, h: V, a: V) -> V {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let ri = self.s.m.bin(c, Bin::LShr, a, four);
        let r = self.s.op(
            c,
            "cbufferLoadLegacy.i32",
            Attr::ReadOnly,
            self.s.t.cbuf_ret,
            59,
            &[h, ri],
        );
        let two = self.s.m.i32(2);
        let three = self.s.m.i32(3);
        let comp = self.s.m.bin(c, Bin::LShr, a, two);
        let comp = self.s.m.bin(c, Bin::And, comp, three);
        let mut x = self.s.m.extract(c, r, 0);
        for j in 1..4 {
            let e = self.s.m.extract(c, r, j);
            let jc = self.s.m.i32(j);
            let is = self.s.m.cmp(c, pred::EQ, comp, jc);
            x = self.s.m.select(c, is, e, x);
        }
        x
    }

    /// `channels` channels of the element at byte `off` of raw buffer `h`,
    /// converted from the GFX10 buffer format `format` as the hardware's
    /// typed loads do (channels past the format's read 0, 0, 0, 1).
    // shortcut: the descriptor's channel swizzle is taken as the identity
    // (it is for raw views and the descriptors code builds), elements of 4
    // bytes or more as dword-aligned.
    pub(super) fn format_load(
        &mut self,
        h: V,
        off: V,
        format: u8,
        channels: u32,
    ) -> Result<Vec<V>, Why> {
        let (bits, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let total: u32 = bits.iter().sum();
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let load = |l: &mut Self, at: V, n: u32| -> Vec<V> {
            let mask = l.s.m.i8((1u8 << n) - 1);
            let r = l.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                l.s.t.res_ret,
                139,
                &[h, at, u, mask, align],
            );
            (0..n)
                .map(|k| {
                    let x = l.s.m.extract(c, r, k);
                    l.as_int(x)
                })
                .collect()
        };
        let words = if total < 32 {
            // A byte or half inside the dword that holds it.
            let (not3, three) = (self.s.m.i32(!3), self.s.m.i32(3));
            let at = self.s.m.bin(c, Bin::And, off, not3);
            let w = load(self, at, 1)[0];
            let byte = self.s.m.bin(c, Bin::And, off, three);
            let sh = self.s.m.bin(c, Bin::Shl, byte, three);
            vec![self.s.m.bin(c, Bin::LShr, w, sh)]
        } else {
            load(self, off, total / 32)
        };
        let float = |l: &mut Self, f: V| l.as_int(f);
        let mut out = Vec::new();
        let mut at = 0;
        for &w in bits {
            let word = words[(at / 32) as usize];
            let field = if w == 32 {
                word
            } else {
                let sh = self.s.m.i32(at % 32);
                let t = self.s.m.bin(c, Bin::LShr, word, sh);
                let m = self.s.m.i32((1 << w) - 1);
                self.s.m.bin(c, Bin::And, t, m)
            };
            at += w;
            // Sign-extended from the field's top bit.
            let signed = |l: &mut Self| {
                if w == 32 {
                    return field;
                }
                let k = l.s.m.i32(32 - w);
                let t = l.s.m.bin(c, Bin::Shl, field, k);
                l.s.m.bin(c, Bin::AShr, t, k)
            };
            let f32t = self.s.t.f32;
            // A normalized format's ends are exactly 1.0 and -1.0. The host
            // may divide through a reciprocal and land just below 1.0: RDNA
            // 4 loaded 16_16_UNORM's 65535 as the half 0x3BFF, not 0x3C00.
            let x = match numeric {
                Numeric::Unorm => {
                    let top = (1u64 << w) - 1;
                    let q = if w <= 16 {
                        let q = self.norm_quotient(field, w);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    let (high, one) = (self.s.m.i32(top as u32), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::EQ, field, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    float(self, q)
                }
                Numeric::Snorm => {
                    let s = signed(self);
                    let top = (1u32 << (w - 1)) - 1;
                    let q = if w <= 17 {
                        // The magnitude's quotient, signed (codes at and
                        // below -top are -1.0 below).
                        let (zero, sign) = (self.s.m.i32(0), self.s.m.i32(0x8000_0000));
                        let neg = self.s.m.cmp(c, pred::SLT, s, zero);
                        let minus = self.s.m.bin(c, Bin::Sub, zero, s);
                        let m = self.s.m.select(c, neg, minus, s);
                        let q = self.norm_quotient(m, w - 1);
                        let sign = self.s.m.select(c, neg, sign, zero);
                        let q = self.s.m.bin(c, Bin::Or, q, sign);
                        self.as_float(q)
                    } else {
                        let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                        let max = self.s.m.f32((top as f32).to_bits());
                        self.s.m.bin(c, Bin::SDiv, f, max)
                    };
                    // The lowest value is -1.0 too, as is the one above it.
                    let (high, one) = (self.s.m.i32(top), self.s.m.f32(1.0f32.to_bits()));
                    let at_top = self.s.m.cmp(c, pred::SGE, s, high);
                    let q = self.s.m.select(c, at_top, one, q);
                    let low = self.s.m.i32(top.wrapping_neg());
                    let minus_one = self.s.m.f32((-1.0f32).to_bits());
                    let at_bottom = self.s.m.cmp(c, pred::SLE, s, low);
                    let q = self.s.m.select(c, at_bottom, minus_one, q);
                    float(self, q)
                }
                Numeric::Uscaled => {
                    let f = self.s.m.cast(c, Cast::UiToFp, field, f32t);
                    float(self, f)
                }
                Numeric::Sscaled => {
                    let s = signed(self);
                    let f = self.s.m.cast(c, Cast::SiToFp, s, f32t);
                    float(self, f)
                }
                Numeric::Uint => field,
                Numeric::Sint => signed(self),
                Numeric::Float if w == 32 => field,
                Numeric::Float => {
                    let h = match half_shift(w) {
                        0 => field,
                        k => {
                            let sh = self.s.m.i32(k);
                            self.s.m.bin(c, Bin::Shl, field, sh)
                        }
                    };
                    let f = self
                        .s
                        .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[h]);
                    float(self, f)
                }
            };
            out.push(x);
        }
        let one = match numeric {
            Numeric::Uint | Numeric::Sint => 1,
            _ => 1.0f32.to_bits(),
        };
        for k in out.len() as u32..channels {
            out.push(self.s.m.i32(if k == 3 { one } else { 0 }));
        }
        out.truncate(channels as usize);
        Ok(out)
    }

    /// The bits of the float nearest to `a / d`, `d = 2^w - 1`, for codes
    /// `0 <= a < d` and `w <= 16`: what the console's buffer unit gives for
    /// a normalized code. The host's division is not exact (the RX 6700 XT
    /// returns 8-bit codes one ulp high and some 16-bit codes one ulp low),
    /// and titles truncate `x * d` back to the code (a material index in a
    /// vertex colour's alpha), which a quotient one ulp low turns into the
    /// code below.
    ///
    /// `Q = floor(a * 2^32 / d) = a * K + floor(a * r / d)` with `K =
    /// floor(2^32 / d)` and `r = 2^32 mod d` (no division when `r` is 1:
    /// widths dividing 32). `a / d` is never exact for `0 < a < d` (`d`
    /// odd, `r` coprime to it), so `Q` rounds to 24 bits up exactly when
    /// its highest dropped bit is set, and the result is that integer (at
    /// most 2^24, so its conversion is exact on any host) scaled by a power
    /// of two; `Q` itself when it fits, the quotient's further bits then
    /// lying below half an ulp. That holds for every code of every width but
    /// 11 and 12 (checked for all codes of widths 2 to 16), which take
    /// [`Self::exact_norm`]. The host's conversion of a larger integer is
    /// not relied on: WARP rounds those above 2^31 otherwise.
    pub(super) fn norm_quotient(&mut self, a: V, w: u32) -> V {
        if matches!(w, 11 | 12) {
            return self.exact_norm(a, w);
        }
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, kv) = (k(self, 1), k(self, kk));
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        if r != 1 {
            let (rv, dv) = (k(self, r), k(self, d));
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
        }
        // The bits to drop (1 to 8) from the exponent of `q >> 8`, which
        // converts exactly (fewer than 25 bits); none when `q` fits.
        let k8 = k(self, 8);
        let top = self.s.m.bin(c, Bin::LShr, q, k8);
        let top = self.s.m.cast(c, Cast::UiToFp, top, self.s.t.f32);
        let top = self.as_int(top);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::LShr, top, k23);
        let bias = k(self, 127 + 15);
        let shift = self.s.m.bin(c, Bin::Sub, e, bias);
        let (zero, b24) = (k(self, 0), k(self, 1 << 24));
        let fits = self.s.m.cmp(c, pred::IULT, q, b24);
        let shift = self.s.m.select(c, fits, zero, shift);
        // Rounded up when the highest dropped bit is set: `a / d` is never
        // exact, so a dropped half is more than half, never a tie.
        let up = self.s.m.bin(c, Bin::Shl, q, one);
        let up = self.s.m.bin(c, Bin::LShr, up, shift);
        let up = self.s.m.bin(c, Bin::And, up, one);
        let m = self.s.m.bin(c, Bin::LShr, q, shift);
        let m = self.s.m.bin(c, Bin::Add, m, up);
        // At most 2^24: exact as a float, and so its scale by 2^(shift - 32).
        let f = self.s.m.cast(c, Cast::UiToFp, m, self.s.t.f32);
        let b95 = k(self, 127 - 32);
        let scale = self.s.m.bin(c, Bin::Add, shift, b95);
        let scale = self.s.m.bin(c, Bin::Shl, scale, k23);
        let scale = self.as_float(scale);
        let f = self.s.m.bin(c, Bin::Mul, f, scale);
        self.as_int(f)
    }

    /// [`Self::norm_quotient`] in integers only, for any width up to 16: `Q`
    /// as there, 8 more bits when that leaves fewer than 25 (codes of wide
    /// formats near 0), then rounded to 24 bits to nearest even with the
    /// remainder as sticky bit, and assembled as float bits.
    pub(super) fn exact_norm(&mut self, a: V, w: u32) -> V {
        let c = self.cur;
        let d = (1u64 << w) - 1;
        let (kk, r) = ((1u64 << 32) / d, (1u64 << 32) % d);
        let k = |l: &mut Self, v: u64| l.s.m.i32(v as u32);
        let (one, zero, dv, k8) = (k(self, 1), k(self, 0), k(self, d), k(self, 8));
        let kv = k(self, kk);
        let mut q = self.s.m.bin(c, Bin::Mul, a, kv);
        let mut rem = a;
        if r != 1 {
            let rv = k(self, r);
            let t = self.s.m.bin(c, Bin::Mul, a, rv);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            q = self.s.m.bin(c, Bin::Add, q, more);
            rem = self.s.m.bin(c, Bin::URem, t, dv);
        }
        // The exponent's bias less the 32 (or 40) fraction bits of `q`; the
        // shift below is added.
        let mut bias = k(self, 127 + 23 - 32);
        if kk < 1 << 24 {
            // Fewer than 25 bits for the smallest codes: 8 more.
            let t = self.s.m.bin(c, Bin::Shl, rem, k8);
            let more = self.s.m.bin(c, Bin::UDiv, t, dv);
            let rest = self.s.m.bin(c, Bin::URem, t, dv);
            let wide = self.s.m.bin(c, Bin::Shl, q, k8);
            let wide = self.s.m.bin(c, Bin::Or, wide, more);
            let limit = k(self, 1 << 24);
            let small = self.s.m.cmp(c, pred::IULT, q, limit);
            q = self.s.m.select(c, small, wide, q);
            rem = self.s.m.select(c, small, rest, rem);
            let b40 = k(self, 127 + 23 - 40);
            bias = self.s.m.select(c, small, b40, bias);
        }
        // Bits to drop: the bit length less 24, 1 to 8.
        let mut shift = one;
        for bit in 25..32 {
            let at = k(self, 1 << bit);
            let ge = self.s.m.cmp(c, pred::IUGE, q, at);
            let add = self.s.m.select(c, ge, one, zero);
            shift = self.s.m.bin(c, Bin::Add, shift, add);
        }
        let mant = self.s.m.bin(c, Bin::LShr, q, shift);
        let below = self.s.m.bin(c, Bin::Sub, shift, one);
        let half = self.s.m.bin(c, Bin::LShr, q, below);
        let half = self.s.m.bin(c, Bin::And, half, one);
        let low = self.s.m.bin(c, Bin::Shl, one, below);
        let low = self.s.m.bin(c, Bin::Sub, low, one);
        let low = self.s.m.bin(c, Bin::And, q, low);
        let rest = self.s.m.bin(c, Bin::Or, low, rem);
        let sticky = self.s.m.cmp(c, pred::NE, rest, zero);
        let odd = self.s.m.bin(c, Bin::And, mant, one);
        let tie = self.s.m.select(c, sticky, one, odd);
        let up = self.s.m.bin(c, Bin::And, half, tie);
        let mant = self.s.m.bin(c, Bin::Add, mant, up);
        // Rounding up to 2^24 moves to the next exponent.
        let k24 = k(self, 24);
        let carry = self.s.m.bin(c, Bin::LShr, mant, k24);
        let mant = self.s.m.bin(c, Bin::LShr, mant, carry);
        let e = self.s.m.bin(c, Bin::Add, bias, shift);
        let e = self.s.m.bin(c, Bin::Add, e, carry);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::Shl, e, k23);
        let fraction = k(self, 0x7F_FFFF);
        let m = self.s.m.bin(c, Bin::And, mant, fraction);
        let bits = self.s.m.bin(c, Bin::Or, e, m);
        let is_zero = self.s.m.cmp(c, pred::EQ, a, zero);
        self.s.m.select(c, is_zero, zero, bits)
    }

    /// Channels of buffer format `format`, as [`Self::format_load`] gives
    /// them, as d16 data: 16 bits each, two per dword, low half first (an
    /// odd last high half 0). Integer formats keep their low 16 bits; the
    /// others become f16.
    pub(super) fn d16_channels(&mut self, format: u8, channels: Vec<V>) -> Result<Vec<V>, Why> {
        let (_, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let mut halves = Vec::new();
        for x in channels {
            halves.push(if matches!(numeric, Numeric::Uint | Numeric::Sint) {
                let m = self.s.m.i32(0xFFFF);
                self.s.m.bin(c, Bin::And, x, m)
            } else {
                let (f, i32t) = (self.as_float(x), self.s.t.i32);
                self.s
                    .op(c, "legacyF32ToF16", Attr::ReadNone, i32t, 130, &[f])
            });
        }
        let mut out = Vec::new();
        for pair in halves.chunks(2) {
            out.push(match *pair {
                [lo, hi] => {
                    let k16 = self.s.m.i32(16);
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                }
                [lo] => lo,
                _ => unreachable!("chunks of one or two"),
            });
        }
        Ok(out)
    }

    /// The byte offset a buffer descriptor built around a root descriptor adds
    /// (constant and computed parts); none for other descriptors.
    pub(super) fn built_offset(&mut self, src: &Source) -> Result<(u32, Option<V>), Why> {
        match src {
            Source::Built { address, .. } => {
                let mut x = match address.dynamic {
                    Some(v) => Some(self.i32_of(v)?),
                    None => None,
                };
                if let Some(bias) = self.root_bias(address.param) {
                    x = Some(match x {
                        Some(d) => self.s.m.bin(self.cur, Bin::Add, d, bias),
                        None => bias,
                    });
                }
                Ok((address.offset, x))
            }
            _ => Ok((0, None)),
        }
    }

    /// For a root view the host binds as a raw view that may begin before
    /// the title's address (`shader.rs` `USER_VIEW_BIAS`): the bytes to add
    /// to offsets through it. Every root UAV (root SRVs keep the views they
    /// have).
    pub(super) fn root_bias(&mut self, param: usize) -> Option<V> {
        use crate::emit::shader::USER_VIEW_BIAS;
        let rp = *self.x.native.layout.get(param)?;
        // (Title memory by address where the user-data table is spilled:
        // [`Lower::root_memory`].)
        if rp.root != RootType::Descriptor || crate::spill::is_spilled(self.x.native) {
            return None;
        }
        let kind = self
            .x
            .native
            .used
            .iter()
            .find(|u| u.user_data == rp.user_data)
            .map(|u| u.kind);
        (kind == Some(ResourceType::Uav))
            .then(|| self.host_word(RootType::Other(USER_VIEW_BIAS), rp.slot, 0))
    }

    /// The used-resource entry a descriptor loaded from a table falls in.
    pub(super) fn used_kind(&self, src: &Source) -> Option<&'a UsedResource> {
        if let Source::Phi { args, .. } | Source::Select { args, .. } | Source::Lanes { args, .. } =
            src
        {
            return self.used_kind(args.first()?);
        }
        if let Source::LanePhi { args, .. } = src {
            return args.iter().flatten().find_map(|a| self.used_kind(a));
        }
        if let Source::Built { address, .. } = src {
            let rp = self.x.native.layout.get(address.param)?;
            return self
                .x
                .native
                .used
                .iter()
                .find(|u| u.root == RootType::Descriptor && u.user_data == rp.user_data);
        }
        let (Source::Loaded { load, first } | Source::Fetched { load, first }) = src else {
            return None;
        };
        let a = self.scalar.get(load)?;
        let rp = self.x.native.layout.get(a.param)?;
        if rp.root != RootType::Table {
            return None;
        }
        let dword = (a.offset / 4) + u32::from(*first);
        self.x
            .native
            .used
            .iter()
            .filter(|u| {
                u.root == RootType::Table
                    && u.user_data == rp.user_data
                    && u.table_offset_dwords <= dword
            })
            .max_by_key(|u| u.table_offset_dwords)
    }

    /// A handle for the descriptor `src` read at `site`.
    pub(super) fn handle(
        &mut self,
        site: Site,
        src: &Source,
        sampler: bool,
        props: (u32, u32),
    ) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => {
                let key = (self.cur, *load, *first, sampler, props);
                if let Some(&h) = self.handles.get(&key) {
                    return Ok(h);
                }
                let index = self.heap_index(*load, *first, sampler)?;
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                // A waterfall loop's load at the first lane's offset reads
                // each lane's own entry (see `resource::Waterfall`).
                let per_lane = self.x.res.waterfall.loads.contains(load);
                let h = self
                    .s
                    .heap_handle_at(self.cur, index, sampler, per_lane, props, bounded);
                self.handles.insert(key, h);
                Ok(h)
            }
            Source::Constant(words) if sampler => {
                let n = self.register_sampler(words, props.0 & 0x8000 != 0)?;
                Ok(self.s.sampler_handle(self.cur, n))
            }
            Source::Constant(_) => Err("a constant resource descriptor"),
            Source::Built { .. } if self.root_copy.is_some() => self.root_copy.ok_or("no copy"),
            Source::Built { address, .. } => {
                let rp = *self
                    .x
                    .native
                    .layout
                    .get(address.param)
                    .ok_or("a descriptor built around a table in the code")?;
                let kind = self
                    .x
                    .native
                    .used
                    .iter()
                    .find(|u| u.user_data == rp.user_data)
                    .map(|u| u.kind);
                match (rp.root, kind, props.0) {
                    (RootType::Descriptor, Some(ResourceType::Cbv) | None, 13) => {
                        Ok(self.s.root_cbv_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 11) => {
                        Ok(self.s.root_srv_handle(rp.slot))
                    }
                    // A root UAV: a raw buffer whether the access reads
                    // (11) or writes (11 with the UAV bit). Indexed
                    // accesses reach here as raw ones at index times the
                    // entry's stride (`buffer`).
                    (RootType::Descriptor, Some(ResourceType::Uav), 11 | 0x100b) => {
                        Ok(self.s.root_uav_handle(rp.slot))
                    }
                    (RootType::Descriptor, Some(ResourceType::Srv), 0x100b) => {
                        Err("a write through a root SRV")
                    }
                    _ => Err("a typed root view"),
                }
            }
            Source::Phi { block, args } => {
                let _ = site;
                let phi = self.descriptor_phi(*block, args, sampler, 8)?;
                let (index, bounded) = self.raw_twin(src, phi, sampler, props);
                Ok(self
                    .s
                    .heap_handle_at(self.cur, index, sampler, false, props, bounded))
            }
            Source::Select { scc, args } => {
                // The heap index of each choice, picked by SCC (uniform).
                let [
                    Source::Loaded { load: a, first: fa },
                    Source::Loaded { load: b, first: fb },
                ] = args[..]
                else {
                    return Err("a descriptor chosen by a condition");
                };
                let cond = match self.get(*scc)? {
                    Val::Bit(x) => x,
                    _ => return Err("SCC without a value"),
                };
                let set = self.heap_index(a, fa, sampler)?;
                let clear = self.heap_index(b, fb, sampler)?;
                let index = self.s.m.select(self.cur, cond, set, clear);
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                Ok(self
                    .s
                    .heap_handle_at(self.cur, index, sampler, false, props, bounded))
            }
            Source::Fetched { .. } | Source::Lanes { .. } | Source::LanePhi { .. } => {
                // A descriptor each lane has its own of (a waterfall loop
                // reads it from the first active lane): each lane's heap
                // index, non-uniform.
                let index = self.lane_heap_index(src, sampler)?;
                let (index, bounded) = self.raw_twin(src, index, sampler, props);
                Ok(self
                    .s
                    .heap_handle_at(self.cur, index, sampler, true, props, bounded))
            }
        }
    }

    /// The heap index to read descriptor `src` at, `index` being the title's:
    /// for a buffer read as raw bytes, through a table or picked from the
    /// heap itself (or picked and read as a constant buffer), the host's raw
    /// twin of that slot ([`crate::emit::shader::raw_views`]), whose reads end where
    /// the view does. Everything else (textures, typed buffers, tables'
    /// constant buffers) stays in place. And whether it is a twin's (inside
    /// the host's heap).
    ///
    /// Tables too: a view that does not begin on 16 bytes cannot be raw, and
    /// Bindless raw access uses the corresponding slot in the host’s raw view range.
    pub(super) fn raw_twin(
        &mut self,
        src: &Source,
        index: V,
        sampler: bool,
        props: (u32, u32),
    ) -> (V, bool) {
        // Raw and constant buffers (DXIL resource kinds 11 and 13).
        const RAW: u32 = 11;
        const CONSTANT: u32 = 13;
        let first = crate::emit::shader::raw_views();
        let picked = |leaf: &&Source| {
            let (Source::Loaded { load, .. } | Source::Fetched { load, .. }) = leaf else {
                return false;
            };
            self.scalar
                .get(load)
                .and_then(|a| self.x.native.layout.get(a.param))
                .is_some_and(|p| {
                    p.root == RootType::ResourceHeap
                        || (p.root == RootType::Table && props.0 & 0xFF == RAW)
                })
        };
        if first == 0
            || sampler
            || !matches!(props.0 & 0xFF, RAW | CONSTANT)
            || !src.leaves().iter().all(picked)
        {
            return (index, false);
        }
        self.raw_twin_index(index, first)
    }

    pub(super) fn raw_twin_index(&mut self, index: V, first: u32) -> (V, bool) {
        if first == 0 {
            return (index, false);
        }
        let c = self.cur;
        let (last, first) = (self.s.m.i32(first - 1), self.s.m.i32(first));
        let i32t = self.s.t.i32;
        // (As `dx.op.binary` UMin, see `heap_handle`.)
        let index = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[index, last]);
        self.s.m.tag(index, Tag::Clamp);
        (self.s.m.bin(c, Bin::Or, index, first), true)
    }

    /// The heap index a descriptor phi picks. Each predecessor computes its
    /// heap index at its end and the join picks one with a phi; a
    /// predecessor whose descriptor is itself chosen by control flow (a
    /// phi of an earlier join, which dominates it) gives that join's phi,
    /// at most `depth` joins deep.
    pub(super) fn descriptor_phi(
        &mut self,
        block: usize,
        args: &[Source],
        sampler: bool,
        depth: u32,
    ) -> Result<V, Why> {
        const WHY: Why = "a descriptor chosen by control flow";
        let p = self.x.p;
        let mut incoming = Vec::new();
        for (&q, arg) in p.blocks[block].preds.iter().zip(args) {
            if self.block[q].is_none() {
                continue;
            }
            let tail = self.tail[q].ok_or(WHY)?;
            let at = self.edges.get(&(q, block)).copied().unwrap_or(tail);
            let index = match arg {
                Source::Loaded { load, first } => {
                    let here = std::mem::replace(&mut self.cur, at);
                    let index = self.heap_index(*load, *first, sampler);
                    self.cur = here;
                    index?
                }
                Source::Phi {
                    block: inner,
                    args: inner_args,
                } if depth > 0 => self.descriptor_phi(*inner, inner_args, sampler, depth - 1)?,
                _ => return Err(WHY),
            };
            incoming.push((index, at));
        }
        let join = self.block[block].ok_or(WHY)?;
        let phi = self.s.m.phi(join, self.s.t.i32);
        self.s.m.set_phi(phi, incoming);
        Ok(phi)
    }

    /// Registers an immediate sampler constant and returns its slot index: a
    /// constant used with and without comparison takes one slot for each.
    pub(super) fn register_sampler(&mut self, words: &[u32], cmp: bool) -> Result<u32, Why> {
        let words: [u32; 4] = words
            .get(..4)
            .and_then(|w| w.try_into().ok())
            .ok_or("a sampler constant")?;
        let slot = self
            .samplers
            .iter()
            .enumerate()
            .position(|(n, w)| *w == words && (self.s.cmp_samplers >> n & 1 != 0) == cmp);
        let n = match slot {
            Some(n) => n,
            None => {
                let n = self.samplers.len();
                if n >= u64::BITS as usize {
                    return Err("more than 64 immediate sampler slots");
                }
                self.samplers.push(words);
                self.s.cmp_samplers |= u64::from(cmp) << n;
                n
            }
        };
        Ok(n as u32)
    }

    /// Resolves a control-flow phi of immediate sampler constants into either a
    /// single static sampler register (`One`) or a uniform `i32` phi index over
    /// `regs` (`Many`) for branch dispatch at the sample site.
    pub(super) fn sampler_phi_choice(
        &mut self,
        block: usize,
        args: &[Source],
        cmp: bool,
        depth: u32,
    ) -> Result<(Option<V>, Vec<u32>), Why> {
        const WHY: Why = "a descriptor chosen by control flow";
        let mut distinct: Vec<[u32; 4]> = Vec::new();
        self.collect_constant_samplers(block, args, depth, &mut distinct)?;
        if distinct.is_empty() {
            return Err(WHY);
        }
        let mut regs = Vec::with_capacity(distinct.len());
        for w in &distinct {
            regs.push(self.register_sampler(w, cmp)?);
        }
        if regs.len() == 1 {
            return Ok((None, regs));
        }
        let idx = self.sampler_index_phi(block, args, 0, &distinct, depth)?;
        Ok((Some(idx), regs))
    }

    pub(super) fn collect_constant_samplers(
        &self,
        block: usize,
        args: &[Source],
        depth: u32,
        distinct: &mut Vec<[u32; 4]>,
    ) -> Result<(), Why> {
        const WHY: Why = "a descriptor chosen by control flow";
        let p = self.x.p;
        for (&q, arg) in p.blocks[block].preds.iter().zip(args) {
            if self.block[q].is_none() {
                continue;
            }
            match arg {
                Source::Constant(w) if w.len() >= 4 => {
                    let k: [u32; 4] = w[..4].try_into().unwrap();
                    if !distinct.contains(&k) {
                        distinct.push(k);
                    }
                }
                Source::Phi {
                    block: inner,
                    args: inner_args,
                } if depth > 0 => {
                    self.collect_constant_samplers(*inner, inner_args, depth - 1, distinct)?;
                }
                _ => return Err(WHY),
            }
        }
        Ok(())
    }

    pub(super) fn sampler_index_phi(
        &mut self,
        block: usize,
        args: &[Source],
        lower: u32,
        distinct: &[[u32; 4]],
        depth: u32,
    ) -> Result<V, Why> {
        const WHY: Why = "a descriptor chosen by control flow";
        let p = self.x.p;
        let mut incoming = Vec::new();
        for (&q, arg) in p.blocks[block].preds.iter().zip(args) {
            if self.block[q].is_none() {
                continue;
            }
            let tail = self.tail[q].ok_or(WHY)?;
            let at = self.edges.get(&(q, block)).copied().unwrap_or(tail);
            let idx = match arg {
                Source::Constant(w) if w.len() >= 4 => {
                    let k: [u32; 4] = w[..4].try_into().unwrap();
                    let pos = distinct.iter().position(|d| *d == k).ok_or(WHY)? as u32;
                    self.s.m.i32(lower + pos)
                }
                Source::Phi {
                    block: inner,
                    args: inner_args,
                } if depth > 0 => {
                    self.sampler_index_phi(*inner, inner_args, lower, distinct, depth - 1)?
                }
                _ => return Err(WHY),
            };
            incoming.push((idx, at));
        }
        let join = self.block[block].ok_or(WHY)?;
        let phi = self.s.m.phi(join, self.s.t.i32);
        self.s.m.set_phi(phi, incoming);
        Ok(phi)
    }

    /// Byte stride from the heap metadata, including table entries when requested.
    pub(super) fn slot_stride(
        &mut self,
        src: &Source,
        allow_table: bool,
    ) -> Result<Option<V>, Why> {
        let word = self.slot_word(src, allow_table)?;
        Ok(word.map(|word| {
            let bits = self.s.m.i32(crate::emit::shader::VIEW_STRIDE_BITS);
            self.s.m.bin(self.cur, Bin::And, word, bits)
        }))
    }

    /// Whether the typed buffer view `src` names is one of an integer format,
    /// and whether a signed one (`ResourceHeap` or `Table`).
    pub(super) fn integer_view(&mut self, src: &Source) -> Result<Option<(V, V)>, Why> {
        let Some(word) = self.slot_word(src, true)? else {
            return Ok(None);
        };
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let [int, signed] = [
            crate::emit::shader::VIEW_INTEGER,
            crate::emit::shader::VIEW_SIGNED,
        ]
        .map(|bit| {
            let bit = self.s.m.i32(bit);
            let x = self.s.m.bin(c, Bin::And, word, bit);
            self.s.m.cmp(c, pred::NE, x, zero)
        });
        Ok(Some((int, signed)))
    }

    /// The host's word for the buffer view `src` names: its stride and
    /// format kind ([`crate::emit::shader::SLOT_STRIDES`] for `ResourceHeap`,
    /// [`crate::emit::shader::SLOT_TABLE_STRIDES`] for `Table` when `allow_table`).
    pub(super) fn slot_word(&mut self, src: &Source, allow_table: bool) -> Result<Option<V>, Why> {
        let Source::Loaded { load, first } = src else {
            return Ok(None);
        };
        let srv_slot = match self
            .scalar
            .get(load)
            .and_then(|a| self.x.native.layout.get(a.param))
            .map(|p| p.root)
        {
            Some(RootType::ResourceHeap) => crate::emit::shader::SLOT_STRIDES,
            Some(RootType::Table) if allow_table => crate::emit::shader::SLOT_TABLE_STRIDES,
            _ => return Ok(None),
        };
        let key = (self.cur, *load, *first);
        if let Some(&v) = self.strides.get(&key) {
            return Ok(Some(v));
        }
        let c = self.cur;
        let slot = self.heap_index(*load, *first, false)?;
        // Inside the table whatever the index (lanes that do not run
        // compute one too): a root view has no bounds of its own.
        let last = self.s.m.i32(crate::emit::shader::SLOT_STRIDES_COUNT - 1);
        let i32t = self.s.t.i32;
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let two = self.s.m.i32(2);
        let byte = self.s.m.bin(c, Bin::Shl, slot, two);
        let h = self.s.root_srv_handle(srv_slot);
        let iu = self.s.m.undef(self.s.t.i32);
        let mask = self.s.m.i8(1);
        let align = self.s.m.i32(4);
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, byte, iu, mask, align],
        );
        let f = self.s.m.extract(c, r, 0);
        let v = self.as_int(f);
        self.strides.insert(key, v);
        Ok(Some(v))
    }

    /// The heap index of a descriptor that may differ between lanes,
    /// computed in the current block (or, chosen by control flow, at the
    /// join).
    pub(super) fn lane_heap_index(&mut self, src: &Source, sampler: bool) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => self.heap_index(*load, *first, sampler),
            Source::Fetched { load, first } => self.fetched_index(*load, *first, sampler),
            Source::Lanes { mask, args } | Source::Select { scc: mask, args } => {
                let [a, b] = &args[..] else {
                    return Err("a descriptor chosen per lane");
                };
                let cond = if matches!(src, Source::Lanes { .. }) {
                    let &m = self
                        .x
                        .st
                        .masks
                        .get(mask)
                        .ok_or("a descriptor chosen per lane by an untracked mask")?;
                    self.boolean(m, self.cur)?
                } else {
                    match self.get(*mask)? {
                        Val::Bit(x) => x,
                        _ => return Err("SCC without a value"),
                    }
                };
                let set = self.lane_heap_index(a, sampler)?;
                let clear = self.lane_heap_index(b, sampler)?;
                Ok(self.s.m.select(self.cur, cond, set, clear))
            }
            Source::LanePhi { block, args } => {
                // Each predecessor computes its heap index at its end (what
                // it reads is there: the dwords are, on that edge); the join
                // picks the one of the edge the lane came by. An edge no
                // lane reading it takes gives index 0.
                let join =
                    self.block[*block].ok_or("a per-lane descriptor chosen by control flow")?;
                let mut incoming = Vec::new();
                for (&q, arg) in self.x.p.blocks[*block].preds.iter().zip(args) {
                    if self.block[q].is_none() {
                        continue;
                    }
                    let tail =
                        self.tail[q].ok_or("a per-lane descriptor joined from a later block")?;
                    let at = self.edges.get(&(q, *block)).copied().unwrap_or(tail);
                    let index = match arg {
                        Some(arg) => {
                            let here = std::mem::replace(&mut self.cur, at);
                            let index = self.lane_heap_index(arg, sampler);
                            self.cur = here;
                            index?
                        }
                        None => self.s.m.i32(0),
                    };
                    incoming.push((index, at));
                }
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s.m.set_phi(phi, incoming);
                Ok(phi)
            }
            _ => Err("a per-lane descriptor from neither a heap load nor a choice of them"),
        }
    }

    /// The heap index of the descriptor at dword `first` of what the global
    /// load at `load` reads, in each lane: its address (base plus offsets)
    /// over the descriptor size.
    pub(super) fn fetched_index(&mut self, load: Site, first: u8, sampler: bool) -> Result<V, Why> {
        const WHY: Why = "a per-lane descriptor load";
        let a = *self
            .scalar
            .get(&load)
            .ok_or("an untracked descriptor load")?;
        match self.x.native.layout.get(a.param).map(|p| p.root) {
            Some(RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap) => {}
            _ => return Err("a descriptor outside the heaps"),
        }
        let li: &Inst = &self.x.p.blocks[load.0].insts[load.1];
        let Fields::Flat {
            addr: Some(Operand::Vector { reg, .. }),
            saddr,
            offset,
            ..
        } = li.fields
        else {
            return Err(WHY);
        };
        let c = self.cur;
        // The low dword of the address: SGPR base plus VGPR offset, or the
        // VGPR pair's low half (heap addresses fit in 32 bits).
        let mut at = self.i32_of(li.read(Reg::V(reg)).ok_or(WHY)?)?;
        if let Some(Operand::Scalar { code, .. }) = saddr
            && code != NULL
        {
            let base = self.i32_of(li.read(Reg::S(code)).ok_or(WHY)?)?;
            at = self.s.m.bin(c, Bin::Add, base, at);
        }
        let k = self
            .s
            .m
            .i32((offset as i32 as u32).wrapping_add(4 * u32::from(first)));
        at = self.s.m.bin(c, Bin::Add, at, k);
        let shift = self.s.m.i32(if sampler { 4 } else { 5 });
        Ok(self.s.m.bin(c, Bin::LShr, at, shift))
    }

    /// The heap index of the descriptor at dword `first` of what the scalar
    /// load at `load` reads, computed in the current block.
    pub(super) fn heap_index(&mut self, load: Site, first: u8, sampler: bool) -> Result<V, Why> {
        let a = *self
            .scalar
            .get(&load)
            .ok_or("an untracked descriptor load")?;
        match self.x.native.layout.get(a.param).map(|p| p.root) {
            Some(RootType::Table | RootType::ResourceHeap | RootType::SamplerHeap) => {}
            _ => return Err("a descriptor outside the heaps"),
        }
        let li: &Inst = &self.x.p.blocks[load.0].insts[load.1];
        let Fields::Smem {
            base,
            offset,
            soffset,
            ..
        } = &li.fields
        else {
            return Err("a descriptor load");
        };
        let Operand::Scalar { code, .. } = *base else {
            return Err("a descriptor load");
        };
        let c = self.cur;
        let base = li.read(Reg::S(code)).ok_or("a descriptor load")?;
        let mut addr = self.i32_of(base)?;
        let k = self
            .s
            .m
            .i32((*offset as u32).wrapping_add(4 * u32::from(first)));
        addr = self.s.m.bin(c, Bin::Add, addr, k);
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(li, soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        let shift = self.s.m.i32(if sampler { 4 } else { 5 });
        Ok(self.s.m.bin(c, Bin::LShr, addr, shift))
    }
}
