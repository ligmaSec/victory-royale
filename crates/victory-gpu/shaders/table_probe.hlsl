#define RS "RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0, numDescriptors=1)), DescriptorTable(Sampler(s0, numDescriptors=1)), UAV(u0)"
cbuffer Coords : register(b0) { float2 uv; };
Texture2D<float4> tex : register(t0);
SamplerState samp : register(s0);
RWByteAddressBuffer out_buf : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    float4 c = tex.SampleLevel(samp, uv, 0);
    out_buf.Store4(0, asuint(c));
}
