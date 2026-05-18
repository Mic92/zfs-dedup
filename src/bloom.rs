// Bloom filter for membership pre-checks where a false positive is
// safe: it may say "seen" for a key that wasn't, never the reverse.
// At ~10 bits/key (~1% FP) it is roughly an order of magnitude smaller
// than a HashSet<(u64,u64)>.
pub struct Bloom {
    bits: Box<[u64]>,
    mask: u64,
}

impl Bloom {
    const K: u32 = 7; // ln(2) * bits/key for 1% FP

    // ~10 bits/key, rounded up to a power of two so position math is a
    // mask instead of a modulo.
    pub fn new(n_keys: u64) -> Self {
        let bits = (n_keys * 10).next_power_of_two().max(64);
        Self {
            bits: vec![0u64; (bits / 64) as usize].into_boxed_slice(),
            mask: bits - 1,
        }
    }

    // Kirsch-Mitzenmacher double hashing: position_i = h1 + i*h2. With
    // `mask` a power of two only the low bits matter, so h1 and h2 must
    // be independent there; split k's halves rather than mixing it,
    // which keeps the low bits correlated and quintuples the FP rate.
    fn pos(&self, k: u64, i: u32) -> u64 {
        let step = (k >> 32) | 1;
        k.wrapping_add(u64::from(i).wrapping_mul(step)) & self.mask
    }

    pub fn insert(&mut self, k: u64) {
        for i in 0..Self::K {
            let pos = self.pos(k, i);
            self.bits[pos as usize / 64] |= 1 << (pos % 64);
        }
    }

    pub fn contains(&self, k: u64) -> bool {
        (0..Self::K).all(|i| {
            let pos = self.pos(k, i);
            self.bits[pos as usize / 64] & (1 << (pos % 64)) != 0
        })
    }

    // Returns whether `k` was already (probably) present, then marks it.
    pub fn check_insert(&mut self, k: u64) -> bool {
        let present = self.contains(k);
        self.insert(k);
        present
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives() {
        let mut b = Bloom::new(1000);
        for k in 0..1000u64 {
            b.insert(k.wrapping_mul(0x9e3779b97f4a7c15));
        }
        for k in 0..1000u64 {
            assert!(b.contains(k.wrapping_mul(0x9e3779b97f4a7c15)));
            assert!(b.check_insert(k.wrapping_mul(0x9e3779b97f4a7c15)));
        }
    }

    #[test]
    fn fp_rate() {
        let mut b = Bloom::new(100_000);
        for k in 0..100_000u64 {
            b.insert(k.wrapping_mul(0x9e3779b97f4a7c15));
        }
        let fp = (100_000..200_000u64)
            .filter(|k| b.contains(k.wrapping_mul(0x9e3779b97f4a7c15)))
            .count();
        assert!(fp < 2_000, "fp rate {fp}/100000");
    }
}
