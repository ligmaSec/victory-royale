#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2), UAV(u3)"
cbuffer Params : register(b0) {
    uint records;
    uint src_stride;
    uint grid_x;
    uint vb_src_bias;
    uint vb_src_stride;
    uint vb_dst_stride;
    uint elem_count;
    uint pad0;
};
ByteAddressBuffer elems : register(t0);
RWByteAddressBuffer src_args : register(u0);
RWByteAddressBuffer dst_args : register(u1);
RWByteAddressBuffer src_vb : register(u2);
RWByteAddressBuffer dst_vb : register(u3);

uint load_u8(uint pos) {
    uint w = src_vb.Load(pos & ~3u);
    return (w >> ((pos & 3u) * 8u)) & 0xffu;
}

uint load_u16(uint pos) {
    uint rem = pos & 3u;
    uint w0 = src_vb.Load(pos & ~3u);
    if (rem <= 2u) {
        return (w0 >> (rem * 8u)) & 0xffffu;
    }
    uint w1 = src_vb.Load((pos & ~3u) + 4u);
    return ((w0 >> 24u) | (w1 << 8u)) & 0xffffu;
}

uint load_u32(uint pos) {
    uint rem = pos & 3u;
    uint w0 = src_vb.Load(pos & ~3u);
    if (rem == 0u) {
        return w0;
    }
    uint w1 = src_vb.Load((pos & ~3u) + 4u);
    return (w0 >> (rem * 8u)) | (w1 << ((4u - rem) * 8u));
}

void store_u8(uint pos, uint val) {
    uint aligned = pos & ~3u;
    uint shift = (pos & 3u) * 8u;
    uint w = dst_vb.Load(aligned);
    w = (w & ~(0xffu << shift)) | ((val & 0xffu) << shift);
    dst_vb.Store(aligned, w);
}

void store_u16(uint pos, uint val) {
    uint rem = pos & 3u;
    if (rem <= 2u) {
        uint aligned = pos & ~3u;
        uint shift = rem * 8u;
        uint w = dst_vb.Load(aligned);
        w = (w & ~(0xffffu << shift)) | ((val & 0xffffu) << shift);
        dst_vb.Store(aligned, w);
    } else {
        store_u8(pos, val & 0xffu);
        store_u8(pos + 1u, (val >> 8u) & 0xffu);
    }
}

void store_u32(uint pos, uint val) {
    uint rem = pos & 3u;
    if (rem == 0u) {
        dst_vb.Store(pos, val);
    } else {
        uint aligned = pos & ~3u;
        uint shift = rem * 8u;
        uint inv = (4u - rem) * 8u;
        uint mask = (1u << shift) - 1u;
        uint w0 = dst_vb.Load(aligned);
        w0 = (w0 & mask) | (val << shift);
        dst_vb.Store(aligned, w0);
        uint w1 = dst_vb.Load(aligned + 4u);
        w1 = (w1 & ~mask) | (val >> inv);
        dst_vb.Store(aligned + 4u, w1);
    }
}

void copy_vertex(uint src_off, uint dst_off) {
    uint full = vb_src_stride & ~3u;
    for (uint b = 0u; b < full; b += 4u) {
        dst_vb.Store(dst_off + b, load_u32(src_off + b));
    }
    uint rem = vb_src_stride & 3u;
    if (rem != 0u) {
        uint w = 0u;
        for (uint k = 0u; k < rem; k++) {
            w |= load_u8(src_off + full + k) << (k * 8u);
        }
        dst_vb.Store(dst_off + full, w);
    }
}

float half_to_f32(uint h) {
    uint sgn = (h >> 15u) << 31u;
    uint ex = (h >> 10u) & 0x1fu;
    uint man = h & 0x3ffu;
    if (ex == 0u) {
        if (man == 0u) return asfloat(sgn);
        float v = (float)man * (1.0f / 16777216.0f);
        return sgn != 0u ? -v : v;
    }
    if (ex == 31u) {
        return asfloat(sgn | 0x7f800000u | (man << 13u));
    }
    return asfloat(sgn | ((ex + 112u) << 23u) | (man << 13u));
}

// The same bits as `primitive.rs`'s `f32_to_half`: NaN kept, subnormals,
// rounding to nearest with ties to even.
uint f32_to_half(float v) {
    uint bits = asuint(v);
    uint sgn = (bits >> 16u) & 0x8000u;
    int ex = (int)((bits >> 23u) & 0xffu) - 127 + 15;
    uint man = bits & 0x7fffffu;
    if ((bits & 0x7fffffffu) > 0x7f800000u) return sgn | 0x7e00u | (man >> 13u);
    if (ex >= 31) return sgn | 0x7c00u;
    if (ex <= 0) {
        if (ex < -10) return sgn;
        uint sig = 0x800000u | man;
        uint shift = (uint)(14 - ex);
        uint halfway = 1u << (shift - 1u);
        uint q = sig >> shift;
        uint r = sig & ((1u << shift) - 1u);
        bool up = r > halfway || (r == halfway && (q & 1u) != 0u);
        return sgn | (q + (up ? 1u : 0u));
    }
    // Added, not or-ed: a rounding carry out of the mantissa raises the
    // exponent.
    return sgn | (((uint)ex << 10u) + ((man + 0xfffu + ((man >> 13u) & 1u)) >> 13u));
}

float read_comp(uint kind, uint pos) {
    if (kind == 0u) return asfloat(load_u32(pos));
    if (kind == 1u) return half_to_f32(load_u16(pos));
    if (kind == 2u) return (float)load_u16(pos) / 65535.0f;
    return (float)load_u8(pos) / 255.0f;
}

void write_comp(uint kind, uint pos, float v) {
    if (kind == 0u) {
        store_u32(pos, asuint(v));
    } else if (kind == 1u) {
        store_u16(pos, f32_to_half(v));
    } else if (kind == 2u) {
        store_u16(pos, (uint)(saturate(v) * 65535.0f + 0.5f));
    } else {
        store_u8(pos, (uint)(saturate(v) * 255.0f + 0.5f));
    }
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {
    uint r = gid.y * grid_x + gid.x;
    if (r >= records) return;
    uint out_verts = dst_args.Load(r * 16u);
    uint rects = out_verts / 6u;
    if (rects == 0u) return;
    uint dst_start = dst_args.Load(r * 16u + 8u);
    uint src_start = src_args.Load(r * src_stride + 8u);

    for (uint rect = gtid.x; rect < rects; rect += 64u) {
        uint v0_src = vb_src_bias + (src_start + rect * 3u) * vb_src_stride;
        uint v1_src = v0_src + vb_src_stride;
        uint v2_src = v1_src + vb_src_stride;
        uint d = (dst_start + rect * 6u) * vb_dst_stride;

        copy_vertex(v0_src, d);
        copy_vertex(v1_src, d + vb_dst_stride);
        copy_vertex(v2_src, d + vb_dst_stride * 2u);
        copy_vertex(v0_src, d + vb_dst_stride * 3u);
        copy_vertex(v2_src, d + vb_dst_stride * 4u);
        uint v3_dst = d + vb_dst_stride * 5u;
        copy_vertex(v0_src, v3_dst);

        for (uint e = 0u; e < elem_count; e++) {
            uint2 desc = elems.Load2(e * 8u);
            uint offset = desc.x;
            uint kind = desc.y & 0xffu;
            uint comp_count = desc.y >> 8u;
            uint comp_bytes = kind == 0u ? 4u : (kind == 3u ? 1u : 2u);
            for (uint c = 0u; c < comp_count; c++) {
                uint at = offset + c * comp_bytes;
                float a = read_comp(kind, v0_src + at);
                float b = read_comp(kind, v1_src + at);
                float f = read_comp(kind, v2_src + at);
                write_comp(kind, v3_dst + at, a + f - b);
            }
        }
    }
}
