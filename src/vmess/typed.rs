pub(crate) type Aes128CfbEnc = cfb_mode::Encryptor<aes::Aes128>;
pub(crate) type Aes128CfbDec = cfb_mode::Decryptor<aes::Aes128>;
pub(crate) type VmessReader = digest::core_api::XofReaderCoreWrapper<sha3::Shake128ReaderCore>;
