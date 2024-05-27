//! Bit manipulation and algorithms.

/// Find the position of the `Nᵗʰ` set bit starting the search from the least
/// significant bit.
///
/// A rank `N=1` specifies the first set bit starting from the LSB, a rank `N=2`
/// specifies the second set bit starting from the LSB, etc.
///
/// The rank is to be provided as a closure that takes as argument the total
/// number of set bits in the value (same as `value.count_ones()`). The rank
/// returned by the closure should therefore never be greater than the closure's
/// argument.
///
/// The returned position is 0-based. If the bit to be found is the LSB, or if
/// the provided rank is 0, the returned position is 0. If in turn the bit to be
/// found is the MSB, or if the specified rank is strictly greater than the
/// total number of bits set, the returned position is `usize::BITS - 1`.
///
/// It is recommended to check for zero values before calling this function
/// since the returned position is then meaningless regardless of the rank.
///
/// Implementation notes: the implementation is based on a tree-of-adders
/// algorithm followed by binary search, with overall theoretical complexity
/// `O(log(usize::BITS))`. In release mode the function is optimized to fully
/// branchless code with a pretty moderate cost of about 70 instructions on
/// x86-64 and less than 60 instruction on aarch64, independently of the inputs.
/// The use of the `popcnt` intrinsic was also investigated to compute sub-sums
/// in the binary search but was found to be slower than the tree-of-adders.
#[allow(clippy::assertions_on_constants)]
pub(crate) fn find_bit<F: FnOnce(usize) -> usize>(value: usize, rank_fn: F) -> usize {
    const P: usize = usize::BITS.trailing_zeros() as usize; // P = log2(usize::BITS)
    const M: [usize; P] = sum_masks();

    const _: () = assert!(usize::BITS.is_power_of_two());
    const _: () = assert!(P >= 2);

    // Partial sub-sums in groups of adjacent 2^p bits.
    let mut sum = [0; P + 1];

    // The zero-order sub-sums (2^p == 1) simply reflect the original value.
    sum[0] = value;

    // Sub-sums for groups of 2 adjacent bits. The RHS is equivalent to
    // `(sum[0] & M[0]) + ((sum[0] >> 1) & M[0]);`.
    sum[1] = value - ((value >> 1) & M[0]);

    // Sub-sums for groups of 4 adjacent bits.
    sum[2] = (sum[1] & M[1]) + ((sum[1] >> 2) & M[1]);

    // Sub-sums for groups of 8, 16 etc. adjacent bits.
    //
    // The below loop seems to be reliably unrolled in release mode, which in
    // turn enables constant propagation and folding. To stay on the safe side,
    // however, the sum masks `M[p]` are const-evaluated as they use integer
    // division and would be otherwise very expensive should loop unrolling fail
    // to kick in.
    for p in 2..P {
        // From p>=2, the mask can be applied to pairwise sums rather than to
        // each operand separately as there is no risk that sub-sums will
        // overflow on neighboring groups. The RHS is thus equivalent to
        // `(sum[p] & M[p]) + ((sum[0] >> (1 << p)) & M[p]);`
        sum[p + 1] = (sum[p] + (sum[p] >> (1 << p))) & M[p];
    }

    let mut rank = rank_fn(sum[P]);

    // Find the bit using binary search.
    //
    // The below loop seems to be reliably unrolled in release mode so the whole
    // function is effectively optimized to fully branchless code.
    let mut shift = 0usize;
    for p in (0..P).rev() {
        // Low bits mask of width 2^p.
        let sub_mask = (1 << (1 << p)) - 1;

        // Bit sum of the lower half of the current subset.
        let lower_sum = (sum[p] >> shift) & sub_mask;

        // Update the rank and the shift if the bit lies in the upper half. The
        // below is a branchless version of:
        // ```
        // if rank > lower_sum {
        //     rank -= lower_sum;
        //     shift += 1 << p;
        // }
        //```
        let cmp_mask = ((lower_sum as isize - rank as isize) >> (isize::BITS - 1)) as usize;
        rank -= lower_sum & cmp_mask;
        shift += (1 << p) & cmp_mask;
    }

    shift
}

/// Generates masks for the tree-of-adder bit summing algorithm.
///
/// The masks are generated according to the pattern:
///
/// ```text
/// m[0]   = 0b010101010101...010101010101;
/// m[1]   = 0b001100110011...001100110011;
/// m[2]   = 0b000011110000...111100001111;
/// ...
/// m[P-1] = 0b000000000000...111111111111;
/// ```
#[allow(clippy::assertions_on_constants)]
const fn sum_masks() -> [usize; usize::BITS.trailing_zeros() as usize] {
    const P: usize = usize::BITS.trailing_zeros() as usize; // P = log2(usize::BITS)
    const _: () = assert!(
        usize::BITS == 1 << P,
        "sum masks are only supported for `usize` with a power-of-two bit width"
    );

    let mut m = [0usize; P];
    let mut p = 0;
    while p != P {
        m[p] = !0 / (1 + (1 << (1 << p)));
        p += 1;
    }

    m
}

#[cfg(all(test, not(asynchronix_loom), not(miri)))]
mod tests {
    use super::*;
    use crate::util::rng;

    // Fuzzing test.
    #[test]
    fn find_bit_fuzz() {
        const SAMPLES: usize = 10_000;

        #[inline(always)]
        fn check(value: usize) {
            let bitsum = value.count_ones() as usize;

            for rank in 1..=bitsum {
                let pos = find_bit(value, |s| {
                    assert_eq!(s, bitsum);

                    rank
                });

                // Check that the bit is indeed set.
                assert!(
                    value & (1 << pos) != 0,
                    "input value: {:064b}\nrequested rank: {}\nreturned position: {}",
                    value,
                    rank,
                    pos
                );

                // Check that the bit is indeed of the requested rank.
                assert_eq!(
                    rank,
                    (value & ((1 << pos) - 1)).count_ones() as usize + 1,
                    "input value: {:064b}\nrequested rank: {}\nreturned position: {}",
                    value,
                    rank,
                    pos
                );
            }
        }

        // Check behavior with a null input value.
        let pos = find_bit(0, |s| {
            assert_eq!(s, 0);
            0
        });
        assert_eq!(pos, 0);

        // Check behavior with other special values.
        check(1);
        check(1 << (usize::BITS - 1));
        check(usize::MAX);

        // Check behavior with random values.
        let rng = rng::Rng::new(12345);
        for _ in 0..SAMPLES {
            // Generate a random usize from one or more random u64 ...for the
            // day we get 128+ bit platforms :-)
            let mut r = rng.gen() as usize;
            let mut shift = 64;
            while shift < usize::BITS {
                r |= (rng.gen() as usize) << shift;
                shift += 64;
            }
            check(r);
        }
    }
}
