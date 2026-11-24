#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seed(pub u64);

impl Seed {
    pub fn roll(self, stream: &str, n: u64) -> u64 {
        let mut x = mix(self.0 ^ 0x9e37_79b9_7f4a_7c15);
        for byte in stream.bytes() {
            x = mix(x ^ u64::from(byte));
        }
        mix(x ^ n.wrapping_mul(0xd6e8_feb8_6659_fd93))
    }

    pub fn below(self, stream: &str, n: u64, bound: u64) -> u64 {
        self.roll(stream, n) % bound.max(1)
    }

    pub fn between(self, stream: &str, n: u64, low: u64, high: u64) -> u64 {
        low + self.below(stream, n, high.saturating_sub(low) + 1)
    }

    pub fn chance(self, stream: &str, n: u64, per_mille: u64) -> bool {
        self.below(stream, n, 1000) < per_mille
    }

    pub fn bytes<const N: usize>(self, stream: &str) -> [u8; N] {
        let mut out = [0; N];
        for (i, chunk) in out.chunks_mut(8).enumerate() {
            let word = self.roll(stream, i as u64).to_be_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        out
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
