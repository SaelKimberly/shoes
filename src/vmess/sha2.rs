use aws_lc_rs::digest::{Context, SHA256};

trait VmessHash: std::fmt::Debug {
    fn setup_new(&self) -> Box<dyn VmessHash>;
    fn update(&mut self, data: &[u8]);
    fn finalize(&mut self) -> [u8; 32];
}

struct Sha256Hash(Context);

impl std::fmt::Debug for Sha256Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sha256Hash").field(&"Context").finish()
    }
}

impl Sha256Hash {
    fn create() -> Self {
        Self(Context::new(&SHA256))
    }
}

impl VmessHash for Sha256Hash {
    fn setup_new(&self) -> Box<dyn VmessHash> {
        Box::new(Sha256Hash(self.0.clone()))
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.0.clone().finish().as_ref());
        out
    }
}

#[derive(Debug)]
struct RecursiveHash {
    inner: Box<dyn VmessHash>,
    outer: Box<dyn VmessHash>,
    default_inner: [u8; 64],
    default_outer: [u8; 64],
}

impl RecursiveHash {
    fn create(key: &[u8], hash: Box<dyn VmessHash>) -> Self {
        // for hmac, we would normally have to get a derived key
        // by hashing the key when it's longer than 64 bytes, but
        // that doesn't happen for vmess's usecase.
        assert!(key.len() <= 64);

        let mut default_outer = [0x5c; 64];
        let mut default_inner = [0x36; 64];

        for (i, &b) in key.iter().enumerate() {
            default_outer[i] ^= b;
            default_inner[i] ^= b;
        }

        let mut inner = hash.setup_new();
        let outer = hash;
        inner.update(&default_inner);
        Self {
            inner,
            outer,
            default_inner,
            default_outer,
        }
    }
}

impl VmessHash for RecursiveHash {
    fn setup_new(&self) -> Box<dyn VmessHash> {
        let new_inner = self.inner.setup_new();
        let new_outer = self.outer.setup_new();

        let mut new_default_inner = [0u8; 64];
        let mut new_default_outer = [0u8; 64];
        new_default_inner.copy_from_slice(&self.default_inner);
        new_default_outer.copy_from_slice(&self.default_outer);

        Box::new(RecursiveHash {
            inner: new_inner,
            outer: new_outer,
            default_inner: new_default_inner,
            default_outer: new_default_outer,
        })
    }

    fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    fn finalize(&mut self) -> [u8; 32] {
        self.outer.update(&self.default_outer);
        self.outer.update(&self.inner.finalize());
        self.outer.finalize()
    }
}

pub(crate) fn kdf(key: &[u8], path: &[&[u8]]) -> [u8; 32] {
    let mut current = Box::new(RecursiveHash::create(
        b"VMess AEAD KDF",
        Box::new(Sha256Hash::create()),
    ));
    for path_item in path.iter() {
        current = Box::new(RecursiveHash::create(path_item, current))
    }
    current.update(key);
    current.finalize()
}

pub(crate) fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(aws_lc_rs::digest::digest(&SHA256, data).as_ref());
    out
}
