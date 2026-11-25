#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0), UAV(u1), UAV(u2), UAV(u3)"
cbuffer Params : register(b0) {
    uint records;
    uint src_stride;
    uint has_count;
    uint max_src_verts;
    uint vert_capacity;
    uint need_verts;
    uint pad0;
    uint pad1;
};
RWByteAddressBuffer src_args : register(u0);
RWByteAddressBuffer count_buf : register(u1);
RWByteAddressBuffer alloc : register(u2);
RWByteAddressBuffer dst_args : register(u3);

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint r = id.x;
    if (r >= records) return;
    uint dst_at = r * 16u;
    if (has_count != 0u) {
        uint live = count_buf.Load(0u);
        if (r >= live) {
            dst_args.Store4(dst_at, uint4(0u, 0u, 0u, 0u));
            return;
        }
    }
    uint4 rec = src_args.Load4(r * src_stride);
    // The clamp's bounds again: 2^24 vertices, 2^26 vertex instances.
    rec.x = min(rec.x, 1u << 24);
    rec.y = min(rec.y, (1u << 26) / max(rec.x, 1u));
    uint rects = rec.x / 3u;
    uint src_verts = rects * 3u;
    uint out_verts = rects * 6u;
    uint out_start = 0u;
    if (rects == 0u || rec.y == 0u || rec.z > max_src_verts || src_verts > max_src_verts - rec.z) {
        out_verts = 0u;
    } else if (need_verts != 0u) {
        if (out_verts > vert_capacity) {
            out_verts = 0u;
        } else {
            uint before;
            alloc.InterlockedAdd(0u, out_verts, before);
            if (before >= vert_capacity || out_verts > vert_capacity - before) {
                out_verts = 0u;
            } else {
                out_start = before;
            }
        }
    }
    dst_args.Store4(dst_at, uint4(out_verts, rec.y, out_start, rec.w));
}
