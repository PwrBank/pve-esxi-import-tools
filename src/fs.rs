use std::sync::Arc;

use anyhow::Error;

use crate::esxi::EsxiClient;
use crate::vmx::VmConfig;

struct Fs {
    client: Arc<EsxiClient>,
    config: VmConfig,
}

impl Fs {
    pub fn new(client: Arc<EsxiClient>, config: VmConfig) -> Result<(), Error> {
        todo!();
    }
}
