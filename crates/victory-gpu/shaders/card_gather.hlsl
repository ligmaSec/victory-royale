#define RS "SRV(t0), SRV(t1), UAV(u0)"
ByteAddressBuffer source : register(t0);
ByteAddressBuffer regions : register(t1);
RWByteAddressBuffer destination : register(u0);
[RootSignature(RS)]
[numthreads(32, 1, 1)]
void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
    uint3 part = regions.Load3(group.x * 16u);
    for (uint offset = lane * 16u; offset < part.z; offset += 512u) {
        destination.Store4(part.y + offset, source.Load4(part.x + offset));
    }
}
