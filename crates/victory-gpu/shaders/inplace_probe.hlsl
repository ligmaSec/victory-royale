#define RS "UAV(u0)"
RWByteAddressBuffer b : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() { b.Store(8, b.Load(0) + b.Load(4)); }
