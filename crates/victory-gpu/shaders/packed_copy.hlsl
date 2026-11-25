#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), UAV(u0)"
cbuffer Copy : register(b0) { uint width; uint src_pitch; uint dst_pitch; uint mode; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);

// Match resource::unpack_16 and resource::pack_16.
uint unpack(uint packed) {
    uint4 rgba;
    if ((mode & 255u) == 86u) {
        rgba = uint4(packed >> 10, packed >> 5, packed, 0) & 31u;
        rgba = (rgba << 3) | (rgba >> 2);
        rgba.a = ((packed >> 15) & 1u) * 255u;
    } else {
        rgba = (uint4(packed >> 8, packed >> 4, packed, packed >> 12) & 15u) * 17u;
    }
    return rgba.r | (rgba.g << 8) | (rgba.b << 16) | (rgba.a << 24);
}

uint pack(uint word) {
    uint4 rgba = uint4(word, word >> 8, word >> 16, word >> 24) & 255u;
    if ((mode & 255u) == 86u) {
        rgba = (rgba * uint4(31, 31, 31, 1) + 127u) / 255u;
        return rgba.b | (rgba.g << 5) | (rgba.r << 10) | (rgba.a << 15);
    }
    rgba = (rgba * 15u + 127u) / 255u;
    return rgba.b | (rgba.g << 4) | (rgba.r << 8) | (rgba.a << 12);
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = 2u * id.x;
    if (x >= width) return;
    uint src_row = id.y * src_pitch;
    uint dst_row = id.y * dst_pitch;
    if (mode & 256u) {
        uint pair = src.Load(src_row + 2u * x);
        dst.Store(dst_row + 4u * x, unpack(pair & 65535u));
        if (x + 1u < width) dst.Store(dst_row + 4u * x + 4u, unpack(pair >> 16));
    } else {
        uint pair = pack(src.Load(src_row + 4u * x));
        if (x + 1u < width) pair |= pack(src.Load(src_row + 4u * x + 4u)) << 16;
        // A single thread owns each dword, including the padding on odd widths.
        dst.Store(dst_row + 2u * x, pair);
    }
}
