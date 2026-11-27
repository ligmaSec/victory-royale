//! Synthetic BVH encodings verify the address, node descriptor and result widths.

const BVH32: [u32; 2] = [0xF198_8F01, 0x0003_1001];
const BVH64: [u32; 2] = [0xF19C_8F01, 0x0003_1001];

#[test]
fn decoded_operands_match_the_ray_query_layout() {
    use crate::gfx10::{Fields, Operand, decode};
    for (words, addr, name) in [
        (BVH32, 11, "image_bvh_intersect_ray"),
        (BVH64, 12, "image_bvh64_intersect_ray"),
    ] {
        let code: Vec<u8> = words
            .iter()
            .chain(&[0xBF81_0000])
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let inst = &decode(&code).unwrap()[0];
        assert_eq!(inst.name(), Some(name));
        let Fields::Image {
            data,
            addr: a,
            rsrc,
            ..
        } = inst.fields(true)
        else {
            panic!("not an image instruction");
        };
        assert_eq!(data, Operand::Vector { reg: 16, dwords: 4 });
        assert_eq!(
            a,
            [Operand::Vector {
                reg: 1,
                dwords: addr
            }]
        );
        assert_eq!(
            rsrc,
            Operand::Scalar {
                code: 12,
                dwords: 4
            }
        );
    }
}
