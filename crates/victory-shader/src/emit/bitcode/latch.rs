//! One back edge per loop header, as DXC's loops have.
//!
//! A console loop can go back to its header from more than one place: the
//! ray-traversal loops of the title's lighting programs test "go on" in two
//! blocks, each a conditional branch to the header or out through its own
//! exit block to the same join. The lowering keeps both back edges, and
//! dxil-spirv as bundled with Proton (114e664e) assumes DXIL has one back
//! edge per block: it keeps the second in the header's forward
//! predecessors, and its dominator computation then recurses until the
//! stack runs out (newer dxil-spirv converts the same programs).
//!
//! Here the latches of such a loop branch to one new latch instead, which
//! makes the same test on a join of their conditions and goes back to the
//! header or out through one exit block to the same join. The values the
//! header and the join receive along the old edges are joined in the new
//! latch. The exiting block stays the latch (the AMD driver hangs on
//! divergent loops whose exit is elsewhere, see the writer), and every lane
//! takes the same edges as before: back to the header, or on to the join.

use super::{Module, Op, Term, V, successors};

/// A loop header's latches folded into one: what the rewrite needs.
struct Merge {
    header: usize,
    /// (latch, its condition, its exit block); the header is on the same
    /// side of every latch's branch.
    latches: Vec<(usize, V, usize)>,
    /// The block every exit block goes on to.
    join: usize,
    /// Whether a true condition goes back to the header.
    back_on_true: bool,
}

impl Module {
    /// Rewrites loops with more than one back edge into loops with one, where
    /// the latches have the shape described in the module docs; other loops
    /// are left as they are.
    pub(crate) fn one_latch_per_loop(&mut self) {
        // One at a time: a merge changes what the next loop out sees.
        while let Some(merge) = self.mergeable_loops().into_iter().next() {
            self.merge_latches(&merge);
        }
    }

    /// The back edges of `main`, by header, in the order a depth-first walk
    /// from the entry along successor order meets them (as dxil-spirv
    /// classifies them: an edge to a block still on the walk's stack).
    fn back_edges(&self) -> Vec<(usize, Vec<usize>)> {
        let n = self.blocks.len();
        let mut by_header: Vec<(usize, Vec<usize>)> = Vec::new();
        if n == 0 {
            return by_header;
        }
        let (mut seen, mut on_stack) = (vec![false; n], vec![false; n]);
        let mut stack = vec![(0usize, 0usize)];
        seen[0] = true;
        on_stack[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            top.1 += 1;
            let Some(&s) = successors(self.blocks[b].term).get(k) else {
                on_stack[b] = false;
                stack.pop();
                continue;
            };
            if on_stack[s] {
                match by_header.iter_mut().find(|(h, _)| *h == s) {
                    Some((_, latches)) if !latches.contains(&b) => latches.push(b),
                    Some(_) => {}
                    None => by_header.push((s, vec![b])),
                }
                continue;
            }
            if !seen[s] {
                seen[s] = true;
                on_stack[s] = true;
                stack.push((s, 0));
            }
        }
        by_header
    }

    /// The loops with several latches that [`Module::merge_latches`] can fold.
    fn mergeable_loops(&self) -> Vec<Merge> {
        let mut preds = vec![0usize; self.blocks.len()];
        for b in &self.blocks {
            for &s in successors(b.term).iter() {
                preds[s] += 1;
            }
        }
        self.back_edges()
            .into_iter()
            .filter(|(_, latches)| latches.len() > 1)
            .filter_map(|(header, latches)| self.merge_of(header, &latches, &preds))
            .collect()
    }

    /// Each latch a conditional branch with the header on one side and, on
    /// the other, an exit block of its own (no joins, no instructions, one
    /// way in) that goes to a join shared by all of them.
    fn merge_of(&self, header: usize, latches: &[usize], preds: &[usize]) -> Option<Merge> {
        let mut found = Vec::new();
        let (mut join, mut side) = (None, None);
        for &l in latches {
            let Some(Term::CondBr(c, t, f)) = self.blocks[l].term else {
                return None;
            };
            let (back_on_true, exit) = match (t == header, f == header) {
                (true, false) => (true, f),
                (false, true) => (false, t),
                _ => return None,
            };
            let e = &self.blocks[exit];
            let Some(Term::Br(j)) = e.term else {
                return None;
            };
            let dedicated = e.phis.is_empty() && e.body.is_empty() && preds[exit] == 1;
            if !dedicated || j == header || *join.get_or_insert(j) != j {
                return None;
            }
            if *side.get_or_insert(back_on_true) != back_on_true {
                return None;
            }
            found.push((l, c, exit));
        }
        Some(Merge {
            header,
            latches: found,
            join: join?,
            back_on_true: side?,
        })
    }

    /// Joins `incoming` (one value per latch, along the latch's old edge) in
    /// the new latch `l`. One value from every latch is defined above all of
    /// them, so above `l` too: no join.
    fn join_in(&mut self, l: usize, incoming: Vec<(V, usize)>) -> V {
        let first = incoming[0].0;
        if incoming.iter().all(|x| x.0 == first) {
            return first;
        }
        let phi = self.phi(l, self.type_of(incoming[0].0));
        self.set_phi(phi, incoming);
        phi
    }

    fn merge_latches(&mut self, m: &Merge) {
        let latch = self.new_block();
        let exit = self.new_block();
        let from_latch = |from: usize| m.latches.iter().any(|&(l, ..)| l == from);
        let from_exit = |from: usize| m.latches.iter().any(|&(_, _, e)| e == from);

        // What the header and the join receive along the old edges.
        for (target, old) in [(m.header, false), (m.join, true)] {
            for n in 0..self.blocks[target].phis.len() {
                let p = self.blocks[target].phis[n];
                let Op::Phi(incoming) = &self.insts[p as usize].op else {
                    continue;
                };
                let moved: Vec<(V, usize)> = m
                    .latches
                    .iter()
                    .filter_map(|&(l, _, e)| {
                        let via = if old { e } else { l };
                        incoming.iter().find(|x| x.1 == via).map(|x| (x.0, l))
                    })
                    .collect();
                let mut kept: Vec<(V, usize)> = incoming
                    .iter()
                    .copied()
                    .filter(|x| !if old { from_exit(x.1) } else { from_latch(x.1) })
                    .collect();
                if moved.is_empty() {
                    continue;
                }
                let joined = self.join_in(latch, moved);
                kept.push((joined, if old { exit } else { latch }));
                self.set_phi(V::Inst(p), kept);
            }
        }

        let cond = self.join_in(latch, m.latches.iter().map(|&(l, c, _)| (c, l)).collect());
        for &(l, _, e) in &m.latches {
            self.terminate(l, Term::Br(latch));
            // Unreachable now; the writer leaves it out.
            self.blocks[e].term = Some(Term::Ret);
        }
        let term = if m.back_on_true {
            Term::CondBr(cond, m.header, exit)
        } else {
            Term::CondBr(cond, exit, m.header)
        };
        self.terminate(latch, term);
        self.terminate(exit, Term::Br(m.join));
    }
}

#[cfg(test)]
mod tests {
    use super::super::opt::tests::run;
    use super::super::{Attr, Bin, Module, Term, V, pred};

    /// Which way each latch of [`two_latches`] leaves.
    #[derive(Clone, Copy)]
    struct Shape {
        /// A true condition goes back to the header (else it leaves).
        back_on_true: bool,
        /// The second latch leaves to a join of its own.
        apart: bool,
    }

    /// A loop whose header goes to one of two latches; each stores its count
    /// and either goes round again or leaves through its own exit block, to
    /// one join (or, `apart`, to two), which stores the count it left with.
    /// The bound is read from the inputs.
    fn two_latches(shape: Shape) -> (Module, usize) {
        let mut m = Module::default();
        let i32t = m.int(32);
        let void = m.void();
        let fty = m.func_ty(i32t, vec![i32t, i32t]);
        m.ptr(fty);
        let input = m.function(
            "dx.op.cbufferLoadLegacy.i32",
            fty,
            Some(Attr::ReadOnly),
            false,
        );
        let store = m.function("dx.op.bufferStore.i32", fty, Some(Attr::NoUnwind), false);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        m.function("main", main_ty, None, true);
        let [entry, head, l1, l2, e1, e2, j1, j2] = [(); 8].map(|_| m.new_block());
        let (zero, one, two, fifteen) = (m.i32(0), m.i32(1), m.i32(2), m.i32(15));
        let raw = m.call(entry, input, vec![zero, zero]);
        let bound = m.bin(entry, Bin::And, raw, fifteen);
        m.terminate(entry, Term::Br(head));

        let i = m.phi(head, i32t);
        let n = m.bin(head, Bin::Add, i, one);
        let odd = m.bin(head, Bin::And, n, one);
        let even = m.cmp(head, pred::EQ, odd, zero);
        m.terminate(head, Term::CondBr(even, l1, l2));

        let latch = |m: &mut Module, l: usize, v: V, tag: V, exit: usize| {
            let _ = m.call(l, store, vec![tag, v]);
            let p = if shape.back_on_true {
                pred::IULT
            } else {
                pred::IUGE
            };
            let c = m.cmp(l, p, v, bound);
            let (t, f) = if shape.back_on_true {
                (head, exit)
            } else {
                (exit, head)
            };
            m.terminate(l, Term::CondBr(c, t, f));
        };
        let n2 = m.bin(l2, Bin::Add, n, two);
        latch(&mut m, l1, n, one, e1);
        latch(&mut m, l2, n2, two, e2);
        m.set_phi(i, vec![(zero, entry), (n, l1), (n2, l2)]);

        let second = if shape.apart { j2 } else { j1 };
        m.terminate(e1, Term::Br(j1));
        m.terminate(e2, Term::Br(second));
        let three = m.i32(3);
        if shape.apart {
            for (j, v) in [(j1, n), (j2, n2)] {
                let _ = m.call(j, store, vec![three, v]);
                m.terminate(j, Term::Ret);
            }
        } else {
            let r = m.phi(j1, i32t);
            m.set_phi(r, vec![(n, e1), (n2, e2)]);
            let _ = m.call(j1, store, vec![three, r]);
            m.terminate(j1, Term::Ret);
            m.terminate(j2, Term::Ret);
        }
        (m, head)
    }

    fn latches_of(m: &Module, header: usize) -> usize {
        m.back_edges()
            .into_iter()
            .find(|(h, _)| *h == header)
            .map_or(0, |(_, l)| l.len())
    }

    #[test]
    fn a_loop_with_two_latches_to_one_join_gets_one_latch_and_stores_the_same() {
        for back_on_true in [false, true] {
            let shape = Shape {
                back_on_true,
                apart: false,
            };
            let (before, head) = two_latches(shape);
            assert_eq!(latches_of(&before, head), 2);
            let (mut after, _) = two_latches(shape);
            after.one_latch_per_loop();
            assert_eq!(latches_of(&after, head), 1);
            assert!(after.back_edges().iter().all(|(_, l)| l.len() == 1));
            // The latch is the exiting block: a conditional branch.
            let (_, latches) = after.back_edges().remove(0);
            assert!(matches!(
                after.blocks[latches[0]].term,
                Some(Term::CondBr(..))
            ));
            for bound in 0..16u32 {
                let trace = run(&before, &[bound]);
                assert_eq!(trace.last().map(|t| t.0.as_str()), Some("ret"));
                assert_eq!(run(&after, &[bound]), trace, "bound {bound}");
            }
            after.write();
        }
    }

    #[test]
    fn a_loop_whose_latches_leave_to_different_joins_is_left_as_it_is() {
        let shape = Shape {
            back_on_true: false,
            apart: true,
        };
        let (before, head) = two_latches(shape);
        let (mut after, _) = two_latches(shape);
        after.one_latch_per_loop();
        assert_eq!(latches_of(&after, head), 2);
        assert_eq!(after.write(), before.write());
    }
}
