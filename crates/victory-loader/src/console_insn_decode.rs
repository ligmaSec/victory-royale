//! Recognises only the x86-64 instructions the console can use but the host
//! may decline. All decoding stays inside a bounded instruction window.
//!
//! Adapted from cool_fox's console decoder, based on XCL (GPL-2.0-only).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Instruction {
    Clwb,
    Clzero {
        address32: bool,
    },
    Rdpru,
    Monitorx,
    Mwaitx,
    Rdpid(u8),
    Extrq {
        dst: u8,
        src: Option<u8>,
        len: u8,
        idx: u8,
    },
    Insertq {
        dst: u8,
        src: u8,
        register_fields: bool,
        len: u8,
        idx: u8,
    },
}

fn low_mask(len: u8) -> u64 {
    u64::MAX >> ((64 - u32::from(len & 63)) % 64)
}

pub(super) fn extrq(value: u64, len: u8, idx: u8) -> u64 {
    (value >> (idx & 63)) & low_mask(len)
}

pub(super) fn insertq(dst: u64, src: u64, len: u8, idx: u8) -> u64 {
    let field = low_mask(len);
    let idx = idx & 63;
    (dst & !(field << idx)) | ((src & field) << idx)
}

/// ModRM, optional SIB and displacement. The opcode field is already checked.
fn operand_len(bytes: &[u8], modrm: u8) -> Option<usize> {
    let (mode, rm) = (modrm >> 6, modrm & 7);
    let (base, start) = if rm == 4 {
        (*bytes.get(1)? & 7, 2)
    } else {
        (rm, 1)
    };
    let len = start
        + match mode {
            0 if base == 5 => 4,
            0 => 0,
            1 => 1,
            _ => 4,
        };
    (len <= bytes.len()).then_some(len)
}

pub(super) fn decode(code: &[u8]) -> Option<(Instruction, usize)> {
    use Instruction as Op;

    let mut code = &code[..code.len().min(15)];
    let window = code.len();
    let (mut operand_size, mut address32, mut repeat, mut rex) = (false, false, 0, 0);
    loop {
        match *code.first()? {
            0x66 => operand_size = true,
            0x67 => address32 = true,
            prefix @ (0xf2 | 0xf3) => repeat = prefix,
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 => {}
            prefix @ 0x40..=0x4f => {
                rex = prefix;
                code = &code[1..];
                continue;
            }
            _ => break,
        }
        // A legacy prefix after REX cancels the effect of REX.
        rex = 0;
        code = &code[1..];
    }
    let prefixes = window - code.len();
    let &[0x0f, opcode, modrm, ..] = code else {
        return None;
    };
    let (mode, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    let (op, len) = match opcode {
        0x01 if repeat == 0 && !operand_size => match modrm {
            0xfc => (Op::Clzero { address32 }, 3),
            0xfd if prefixes == 0 => (Op::Rdpru, 3),
            0xfa if prefixes == 0 => (Op::Monitorx, 3),
            0xfb if prefixes == 0 => (Op::Mwaitx, 3),
            _ => return None,
        },
        0xae if operand_size && repeat == 0 && reg == 6 && mode != 3 => {
            (Op::Clwb, 2 + operand_len(&code[2..], modrm)?)
        }
        0xc7 if repeat == 0xf3 && reg == 7 && mode == 3 => (Op::Rdpid(rm | (rex & 1) << 3), 3),
        0x78 | 0x79 if mode == 3 && (repeat == 0xf2 || (repeat == 0 && operand_size)) => {
            let (reg_xmm, rm_xmm) = (reg | (rex & 4) << 1, rm | (rex & 1) << 3);
            let immediate = opcode == 0x78;
            let (len, idx) = if immediate {
                (*code.get(3)?, *code.get(4)?)
            } else {
                (0, 0)
            };
            let op = if repeat == 0xf2 {
                Op::Insertq {
                    dst: reg_xmm,
                    src: rm_xmm,
                    register_fields: !immediate,
                    len,
                    idx,
                }
            } else if immediate {
                if reg != 0 {
                    return None;
                }
                Op::Extrq {
                    dst: rm_xmm,
                    src: None,
                    len,
                    idx,
                }
            } else {
                Op::Extrq {
                    dst: reg_xmm,
                    src: Some(rm_xmm),
                    len,
                    idx,
                }
            };
            (op, if immediate { 5 } else { 3 })
        }
        _ => return None,
    };
    Some((op, prefixes + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_field_insertion_and_extraction_round_trip() {
        for (value, len, idx) in [(0x1234_u64, 12, 20), (u64::MAX, 63, 1), (0x55, 0, 0)] {
            assert_eq!(
                extrq(insertq(0, value, len, idx), len, idx),
                value & low_mask(len)
            );
        }
        assert_eq!(extrq(0x8000_0000_0000_0000, 1, 63), 1);
        assert_eq!(insertq(u64::MAX, 0xab, 8, 8), 0xffff_ffff_ffff_abff);
        assert_eq!(insertq(0, 0x1ff, 4, 60), 0xf000_0000_0000_0000);
    }

    #[test]
    fn prefixes_select_the_instruction_and_last_repeat_wins() {
        let insert = Instruction::Insertq {
            dst: 0,
            src: 1,
            register_fields: false,
            len: 16,
            idx: 8,
        };
        for prefixes in [
            [0x66, 0xf2],
            [0xf2, 0x66],
            [0xf3, 0xf2],
            [0x67, 0xf2],
            [0x45, 0xf2],
        ] {
            let [a, b] = prefixes;
            assert_eq!(decode(&[a, b, 0x0f, 0x78, 0xc1, 16, 8]), Some((insert, 7)));
        }
        assert_eq!(decode(&[0xf2, 0xf3, 0x0f, 0x78, 0xc1, 16, 8]), None);
        assert_eq!(decode(&[0xf2, 0x0f, 0x78, 0xc1, 16]), None);
        assert_eq!(decode(&[0x0f, 0x78, 0xc1, 16, 8]), None); // VMREAD
        assert_eq!(decode(&[0xf0, 0x66, 0x0f, 0xae, 0x30]), None); // LOCK CLWB
    }

    #[test]
    fn decodes_only_complete_legal_encodings() {
        assert_eq!(
            decode(&[0x67, 0x0f, 0x01, 0xfc]),
            Some((Instruction::Clzero { address32: true }, 4))
        );
        assert_eq!(
            decode(&[0xf3, 0x41, 0x0f, 0xc7, 0xf9]),
            Some((Instruction::Rdpid(9), 5))
        );
        assert_eq!(
            decode(&[0x66, 0x0f, 0xae, 0x74, 0x24, 8]),
            Some((Instruction::Clwb, 6))
        );
        assert_eq!(decode(&[0x66, 0x0f, 0xae, 0x74, 0x24]), None);
        assert_eq!(decode(&[0x0f, 0x0b]), None); // UD2 is a deliberate trap.
        let mut overlong = [0x66; 16];
        overlong[13..].copy_from_slice(&[0x0f, 0xae, 0x30]);
        assert_eq!(decode(&overlong), None);
        assert_eq!(decode(&overlong[1..]), Some((Instruction::Clwb, 15)));
    }
}
