use std::cell::Cell;

/// A pseudo-random number generator based on Wang Yi's Wyrand.
///
/// See: https://github.com/wangyi-fudan/wyhash
#[derive(Clone, Debug)]
pub(crate) struct Rng {
    seed: Cell<u64>,
}

impl Rng {
    /// Creates a new RNG with the provided seed.
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            seed: Cell::new(seed),
        }
    }

    /// Generates a pseudo-random number within the range `0..2⁶⁴`.
    pub(crate) fn gen(&self) -> u64 {
        let seed = self.seed.get().wrapping_add(0xA0761D6478BD642F);
        self.seed.set(seed);
        let t = seed as u128 * (seed ^ 0xE7037ED1A0B428DB) as u128;
        (t as u64) ^ (t >> 64) as u64
    }

    /// Generates a pseudo-random number within the range `0..upper_bound`.
    ///
    /// This generator is biased as it uses the fast (but crude) multiply-shift
    /// method. The bias is negligible, however, as long as the bound is much
    /// smaller than 2⁶⁴.
    pub(crate) fn gen_bounded(&self, upper_bound: u64) -> u64 {
        ((self.gen() as u128 * upper_bound as u128) >> 64) as u64
    }
}

#[cfg(all(test, not(asynchronix_loom), not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn rng_gen_bounded_chi2() {
        const RNG_SEED: u64 = 12345;
        const DICE_ROLLS: u64 = 1_000_000;
        const DICE_FACES: u64 = 6; // beware: modify the p-values if you change this.
        const CHI2_PVAL_LOWER: f64 = 0.210; // critical chi2 for lower p-value = 0.001 and DoF = DICE_FACES - 1
        const CHI2_PVAL_UPPER: f64 = 20.515; // critical chi2 for upper p-value = 0.999 and DoF = DICE_FACES - 1.

        let rng = Rng::new(RNG_SEED);

        let mut tally = [0u64; 6];

        for _ in 0..DICE_ROLLS {
            let face = rng.gen_bounded(DICE_FACES);
            tally[face as usize] += 1;
        }

        let expected = DICE_ROLLS as f64 / DICE_FACES as f64;

        let chi2 = (0..DICE_FACES).fold(0f64, |chi2, face| {
            let actual = tally[face as usize] as f64;

            chi2 + (actual - expected) * (actual - expected) / expected
        });

        println!("tally = {:?}", tally);
        println!("chi2 = {}", chi2);

        assert!(chi2 > CHI2_PVAL_LOWER);
        assert!(chi2 < CHI2_PVAL_UPPER);
    }
}
