#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1)"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint unused; };
RWByteAddressBuffer args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= records) return;
    uint at = id.x * stride;
    uint cost;
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint count = min(args.Load(at), 1u << 24);
        uint instances = min(args.Load(at + 4), (1u << 26) / max(count, 1u));
        cost = count * instances;
        args.Store2(at, uint2(count, instances));
    } else if (kind == 2) {
        uint3 g = min(args.Load3(at), uint3(65535, 65535, 65535));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64;
        args.Store3(at, g);
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = args.Load3(at);
        // An unfilled record must run empty without charging the list's budget.
        if (all(g == 0xffffffffu)) {
            args.Store3(at, uint3(0, 0, 0));
            return;
        }
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128;
        args.Store3(at, g);
    }
    // Reserve min(cost, remaining) with a saturating compare-and-exchange so
    // the counter never passes BUDGET (a plain add would wrap at 2^32 after
    // enough rejected records and reopen the budget). Both exits share the
    // single store below.
    bool reject = false;
    uint before;
    budget.InterlockedAdd(0, 0u, before);
    [allow_uav_condition]
    while (true) {
        if (before >= BUDGET) {
            reject = true;
            break;
        }
        uint remaining = BUDGET - before;
        uint next = before + min(cost, remaining);
        uint observed;
        budget.InterlockedCompareExchange(0, before, next, observed);
        if (observed == before) {
            reject = cost > remaining;
            break;
        }
        before = observed;
    }
    if (reject) {
        // Draws keep their layout with no vertices; groups become zero.
        args.Store(at, 0);
    }
}
