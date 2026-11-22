// HDR desktop (FP16 scRGB, linear, 1.0 = 80 nits) -> SDR NV12 (BT.709, limited
// range, sRGB-encoded) at the recording size, in one pass. See hdr_grab.rs.
//
// Each thread writes a 4x2 block of output pixels: two 4-byte rows of luma and
// one 4-byte row of interleaved chroma (U0 V0 U1 V1), so every store is a
// whole, aligned uint. Requires out width % 4 == 0 and out height % 2 == 0.

cbuffer Params : register(b0)
{
    uint2 out_size;      // recording width, height
    float2 scale;        // desktop pixels per output pixel (x, y)
    float2 src_inv;      // 1 / desktop width, 1 / desktop height
    float inv_white;     // 1 / SDR white in scRGB units (e.g. 1/3 for 240 nits)
    uint cursor_mode;    // 0 none, 1 straight alpha, 2 mask (a=1 opaque, a=0.5 invert)
    int2 cursor_pos;     // cursor top-left in desktop pixels
    uint2 cursor_size;
};

Texture2D<float4> src : register(t0);
Texture2D<float4> cursor : register(t1);
SamplerState samp : register(s0);
RWByteAddressBuffer outbuf : register(u0);

float3 to_linear(float3 c)
{
    float3 lo = c / 12.92;
    float3 hi = pow(abs((c + 0.055) / 1.055), 2.4);
    return lerp(hi, lo, step(c, 0.04045));
}

float3 to_srgb(float3 c)
{
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(abs(c), 1.0 / 2.4) - 0.055;
    return lerp(hi, lo, step(c, 0.0031308));
}

// Linear SDR colour (0..1) of the desktop at continuous pixel position p,
// with the cursor composited.
float3 tap(float2 p)
{
    // Highlights above SDR white are clipped (not tone-mapped); negative
    // (out-of-sRGB-gamut) components are clamped.
    float3 c = saturate(src.SampleLevel(samp, p * src_inv, 0).rgb * inv_white);
    if (cursor_mode != 0)
    {
        int2 q = int2(floor(p)) - cursor_pos;
        if (all(q >= 0) && all(q < int2(cursor_size)))
        {
            float4 k = cursor.Load(int3(q, 0));
            if (cursor_mode == 1)
                c = lerp(c, to_linear(k.rgb), k.a);
            else if (k.a > 0.75)
                c = to_linear(k.rgb);
            else if (k.a > 0.25)
                c = to_linear(1.0 - to_srgb(c));
        }
    }
    return c;
}

// sRGB-encoded colour of output pixel o: four taps a quarter output pixel
// apart (an exact 2x2 box for a 2:1 downscale, a fair box filter up to ~4:1).
float3 pixel(uint2 o)
{
    float2 center = (float2(o) + 0.5) * scale;
    float2 d = scale * 0.25;
    float3 c = tap(center + float2(-d.x, -d.y)) + tap(center + float2(d.x, -d.y))
             + tap(center + float2(-d.x, d.y)) + tap(center + float2(d.x, d.y));
    return to_srgb(saturate(c * 0.25));
}

static const float3 KR_KG_KB = float3(0.2126, 0.7152, 0.0722);

uint luma8(float3 g)
{
    return (uint)round(16.0 + 219.0 * dot(g, KR_KG_KB));
}

// Chroma of the average of four sRGB-encoded pixels, as U | V << 8.
uint chroma16(float3 a, float3 b, float3 c, float3 d)
{
    float3 g = (a + b + c + d) * 0.25;
    float y = dot(g, KR_KG_KB);
    uint u = (uint)round(128.0 + 224.0 * (g.b - y) / 1.8556);
    uint v = (uint)round(128.0 + 224.0 * (g.r - y) / 1.5748);
    return u | (v << 8);
}

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint x0 = id.x * 4;
    uint y0 = id.y * 2;
    if (x0 >= out_size.x || y0 >= out_size.y)
        return;

    float3 t[4];
    float3 b[4];
    [unroll] for (uint i = 0; i < 4; i++)
    {
        t[i] = pixel(uint2(x0 + i, y0));
        b[i] = pixel(uint2(x0 + i, y0 + 1));
    }
    uint w = out_size.x;
    uint row0 = luma8(t[0]) | (luma8(t[1]) << 8) | (luma8(t[2]) << 16) | (luma8(t[3]) << 24);
    uint row1 = luma8(b[0]) | (luma8(b[1]) << 8) | (luma8(b[2]) << 16) | (luma8(b[3]) << 24);
    uint uv = chroma16(t[0], t[1], b[0], b[1]) | (chroma16(t[2], t[3], b[2], b[3]) << 16);
    outbuf.Store(y0 * w + x0, row0);
    outbuf.Store((y0 + 1) * w + x0, row1);
    outbuf.Store(w * out_size.y + (y0 / 2) * w + x0, uv);
}
