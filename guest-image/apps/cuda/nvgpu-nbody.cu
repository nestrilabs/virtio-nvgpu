// A CUDA runtime program for the application pass (apps.sh cuda), beyond
// cuda-smoke's driver-API round trip: device properties, a vector add checked
// on the host, pinned and pageable host<->device bandwidth, and an all-pairs
// n-body step timed with events (GFLOP/s, 20 flops per interaction as NVIDIA's
// nbody sample counts them). Exit 0 only if every check passed.
#include <cstdio>
#include <cstdlib>
#include <cmath>
#include <vector>
#include <cuda_runtime.h>

#define CK(x) do { cudaError_t e_ = (x); if (e_ != cudaSuccess) { \
    std::printf("FAIL %s: %s\n", #x, cudaGetErrorString(e_)); return 1; } } while (0)

__global__ void vadd(const float* a, const float* b, float* c, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) c[i] = a[i] + b[i];
}

__global__ void nbody(const float4* p, float4* acc, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    float4 me = p[i];
    float3 a = {0, 0, 0};
    for (int j = 0; j < n; j++) {
        float4 o = p[j];
        float dx = o.x - me.x, dy = o.y - me.y, dz = o.z - me.z;
        float d2 = dx * dx + dy * dy + dz * dz + 0.01f;
        float inv = rsqrtf(d2);
        float s = o.w * inv * inv * inv;
        a.x += dx * s; a.y += dy * s; a.z += dz * s;
    }
    acc[i] = make_float4(a.x, a.y, a.z, 0);
}

static double bw(void* dst, const void* src, size_t sz, cudaMemcpyKind k) {
    cudaEvent_t s, e;
    cudaEventCreate(&s); cudaEventCreate(&e);
    cudaEventRecord(s);
    for (int r = 0; r < 10; r++) cudaMemcpy(dst, src, sz, k);
    cudaEventRecord(e); cudaEventSynchronize(e);
    float ms; cudaEventElapsedTime(&ms, s, e);
    cudaEventDestroy(s); cudaEventDestroy(e);
    return 10.0 * sz / (ms / 1e3) / 1e9;
}

int main() {
    int n = 0;
    CK(cudaGetDeviceCount(&n));
    cudaDeviceProp pr;
    CK(cudaGetDeviceProperties(&pr, 0));
    std::printf("device 0 of %d: %s, sm_%d%d, %d SMs, %.1f GiB\n", n, pr.name, pr.major, pr.minor,
                pr.multiProcessorCount, pr.totalGlobalMem / 1073741824.0);

    const int N = 1 << 24;
    std::vector<float> a(N), b(N), c(N);
    for (int i = 0; i < N; i++) { a[i] = i * 0.5f; b[i] = 1.0f - i; }
    float *da, *db, *dc;
    CK(cudaMalloc(&da, N * 4)); CK(cudaMalloc(&db, N * 4)); CK(cudaMalloc(&dc, N * 4));
    CK(cudaMemcpy(da, a.data(), N * 4, cudaMemcpyHostToDevice));
    CK(cudaMemcpy(db, b.data(), N * 4, cudaMemcpyHostToDevice));
    vadd<<<(N + 255) / 256, 256>>>(da, db, dc, N);
    CK(cudaGetLastError());
    CK(cudaMemcpy(c.data(), dc, N * 4, cudaMemcpyDeviceToHost));
    int bad = 0;
    for (int i = 0; i < N; i++) if (c[i] != a[i] + b[i]) bad++;
    std::printf("vectorAdd %d elements: %s (%d wrong)\n", N, bad ? "FAIL" : "PASS", bad);

    float* pinned;
    CK(cudaMallocHost(&pinned, N * 4));
    std::printf("bandwidth: pinned H2D %.1f GB/s, D2H %.1f GB/s; pageable H2D %.1f GB/s; D2D %.1f GB/s\n",
                bw(da, pinned, N * 4, cudaMemcpyHostToDevice), bw(pinned, da, N * 4, cudaMemcpyDeviceToHost),
                bw(da, a.data(), N * 4, cudaMemcpyHostToDevice), bw(db, da, N * 4, cudaMemcpyDeviceToDevice));
    CK(cudaFreeHost(pinned));

    const int B = 65536;
    std::vector<float4> p(B);
    for (int i = 0; i < B; i++)
        p[i] = make_float4(std::sin(i * 1.3f), std::cos(i * 0.7f), std::sin(i * 0.11f), 1.0f);
    float4 *dp, *dacc;
    CK(cudaMalloc(&dp, B * 16)); CK(cudaMalloc(&dacc, B * 16));
    CK(cudaMemcpy(dp, p.data(), B * 16, cudaMemcpyHostToDevice));
    nbody<<<B / 256, 256>>>(dp, dacc, B);
    CK(cudaDeviceSynchronize());
    cudaEvent_t s, e;
    cudaEventCreate(&s); cudaEventCreate(&e);
    cudaEventRecord(s);
    const int steps = 10;
    for (int r = 0; r < steps; r++) nbody<<<B / 256, 256>>>(dp, dacc, B);
    cudaEventRecord(e);
    CK(cudaEventSynchronize(e));
    float ms; cudaEventElapsedTime(&ms, s, e);
    // The host checks one body's acceleration against its own sum.
    std::vector<float4> acc(B);
    CK(cudaMemcpy(acc.data(), dacc, B * 16, cudaMemcpyDeviceToHost));
    double ax = 0;
    for (int j = 0; j < B; j++) {
        double dx = p[j].x - p[7].x, dy = p[j].y - p[7].y, dz = p[j].z - p[7].z;
        double d2 = dx * dx + dy * dy + dz * dz + 0.01, inv = 1 / std::sqrt(d2);
        ax += dx * p[j].w * inv * inv * inv;
    }
    bool ok = std::fabs(ax - acc[7].x) <= 1e-3 * std::fabs(ax) + 1e-2;
    double gflops = 20.0 * B * (double)B * steps / (ms / 1e3) / 1e9;
    std::printf("nbody %d bodies x %d steps: %.1f ms/step, %.0f GFLOP/s, check %s (gpu %.4f host %.4f)\n",
                B, steps, ms / steps, gflops, ok ? "PASS" : "FAIL", acc[7].x, ax);
    bad += !ok;
    std::printf("%s\n", bad ? "RESULT FAIL" : "RESULT PASS");
    return bad ? 1 : 0;
}
