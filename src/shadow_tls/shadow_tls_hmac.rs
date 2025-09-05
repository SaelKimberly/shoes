#[derive(Debug, Clone)]
pub(crate) struct ShadowTlsHmac {
    context: aws_lc_rs::hmac::Context,
}

impl ShadowTlsHmac {
    pub(crate) fn new(key: &aws_lc_rs::hmac::Key) -> Self {
        Self {
            context: aws_lc_rs::hmac::Context::with_key(key),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        self.context.update(data);
    }

    pub(crate) fn digest(&self) -> [u8; 4] {
        let tag = self.context.clone().sign();
        let mut out = [0u8; 4];
        out.copy_from_slice(&tag.as_ref()[0..4]);
        out
    }

    pub(crate) fn finalized_digest(self) -> [u8; 4] {
        let tag = self.context.sign();
        let mut out = [0u8; 4];
        out.copy_from_slice(&tag.as_ref()[0..4]);
        out
    }
}
