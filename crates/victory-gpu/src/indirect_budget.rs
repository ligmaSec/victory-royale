//! CPU model of the saturating per-list indirect work budget, independent
//! of the graphics API. In the stop-after-rejection mode, a record reserves
//! `min(cost, remaining)`; the record is
//! rejected (runs empty) when the budget was already spent or the cost does
//! not fit in what remains. The counter never exceeds [`BUDGET`], so rejected
//! records cannot wrap the 32-bit word and reopen the budget.

/// Vertex instances or threads one list may run (2^27); matches `BUDGET` in
/// the shader.
pub(crate) const BUDGET: u32 = 1 << 27;

/// The budget word the shader updates.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budget {
    pub(crate) used: u32,
}

impl Budget {
    /// Applies one record's `cost`; returns whether it runs (`true`) or is
    /// zeroed (`false`).
    pub(crate) fn admit(&mut self, cost: u32) -> bool {
        if self.used >= BUDGET {
            return false;
        }
        let remaining = BUDGET - self.used;
        self.used += cost.min(remaining);
        cost <= remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget as the shader had it before saturation: an unconditional
    /// 32-bit add.
    fn wrapping_admit(used: &mut u32, cost: u32) -> bool {
        let before = *used;
        *used = before.wrapping_add(cost);
        !(before >= BUDGET || before.wrapping_add(cost) > BUDGET)
    }

    fn accepted_work(costs: impl Iterator<Item = u32>) -> (u64, Budget) {
        let mut b = Budget::default();
        let mut work = 0u64;
        for c in costs {
            if b.admit(c) {
                work += u64::from(c);
            }
        }
        (work, b)
    }

    #[test]
    fn rejected_records_cannot_wrap_the_counter() {
        let cost = 1u32 << 26;
        let (work, b) = accepted_work(std::iter::repeat_n(cost, 66));
        assert_eq!(work, u64::from(BUDGET));
        assert_eq!(b.used, BUDGET);

        // The unsaturated word returns to zero after 64 records and then
        // admits the budget a second time.
        let mut used = 0u32;
        let mut old_work = 0u64;
        for _ in 0..66 {
            if wrapping_admit(&mut used, cost) {
                old_work += u64::from(cost);
            }
        }
        assert_eq!(old_work, 2 * u64::from(BUDGET));
    }

    #[test]
    fn boundaries() {
        let mut b = Budget::default();
        assert!(b.admit(BUDGET - 1));
        assert!(b.admit(1));
        assert_eq!(b.used, BUDGET);
        assert!(!b.admit(1));
        assert!(
            !b.admit(0),
            "an exhausted budget rejects even empty records"
        );
        assert_eq!(b.used, BUDGET);

        let mut b = Budget::default();
        assert!(b.admit(BUDGET));
        assert!(!b.admit(u32::MAX));
        assert_eq!(b.used, BUDGET);

        let mut b = Budget { used: BUDGET - 4 };
        assert!(!b.admit(5), "an oversized record is rejected");
        assert_eq!(b.used, BUDGET, "and saturates the counter");
    }

    #[test]
    fn zero_cost_records_do_not_consume() {
        let mut b = Budget::default();
        for _ in 0..1000 {
            assert!(b.admit(0));
        }
        assert_eq!(b.used, 0);
    }

    #[test]
    fn randomized_sequences_respect_the_budget() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..512 {
            let mut b = Budget::default();
            let mut work = 0u64;
            for _ in 0..100 {
                let r = next();
                let cost = match r % 4 {
                    0 => 0,
                    1 => (r >> 8) as u32 % 64,
                    2 => (r >> 8) as u32 % (1 << 26),
                    _ => 1 << 26,
                };
                let before = b.used;
                let ran = b.admit(cost);
                assert!(b.used <= BUDGET && b.used >= before);
                if ran {
                    assert!(before < BUDGET);
                    work += u64::from(cost);
                }
                assert!(work <= u64::from(BUDGET));
            }
            assert!(u64::from(b.used) >= work);
        }
    }
}
