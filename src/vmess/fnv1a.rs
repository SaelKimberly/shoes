pub(crate) struct Fnv1aHasher(u32);

const FNV_PRIME: u32 = 16777619;

impl Fnv1aHasher {
    pub(crate) fn new() -> Self {
        Self(0x811c9dc5)
    }

    pub(crate) fn write(&mut self, data: &[u8]) {
        let mut hash = self.0;
        for byte in data.iter() {
            hash ^= *byte as u32;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        self.0 = hash;
    }

    pub(crate) fn finish(self) -> u32 {
        self.0
    }
}
