//! Work around tract 0.23.10's AVX2 int8 GEMM. Its legacy SSE `movq` in
//! the inner loop and `extractps` in the store mix with 256-bit AVX
//! instructions. Use tract's VEX-encoded integer kernel on this tier.

use tract_onnx::prelude::{DatumType, tract_linalg};

use tract_linalg::isa::{self, Arch, Isa, IsaSet};
use tract_linalg::mmm::MatMatMul;
use tract_linalg::mmm_routines::MmmRoutine;
use tract_linalg::mmm_tiers::MmmTier;
use tract_linalg::x86_64::mmm::avx_mmm_i32_8x4;

const KERNEL: &str = "avx_mmm_i32_8x4_fell";

fn needs_fallback(isa: &IsaSet) -> bool {
    isa.has(Isa::X86_64Avx2) && !isa.has(Isa::X86_64Avx512f) && !isa.has(Isa::X86_64AvxVnni)
}

fn fallback_kernel() -> Box<dyn MatMatMul> {
    let mut kernel = avx_mmm_i32_8x4.clone().with_boost(|| {
        if needs_fallback(&isa::native()) {
            // Tract ranks kernels by ISA when the sequence length is
            // symbolic. Prefer the fallback over the stock AVX2 kernel.
            isa::peer_of(Isa::X86_64Avx, Isa::X86_64Avx2) + 1
        } else {
            isa::NEVER_PREFERRED
        }
    });
    kernel.name = KERNEL.into();
    kernel.mmm()
}

inventory::submit! { MmmRoutine { make: fallback_kernel } }

// Tract consults the tier before ranking kernels when the output width is known.
inventory::submit! {
    MmmTier {
        arch: Some(Arch::X86_64),
        precedence: 9,
        name: "fell-avx2-int8",
        applies: needs_fallback,
        preferred: |_, dt, query, _| (dt == DatumType::I32 && query.n != Some(1)).then_some(KERNEL),
    }
}
