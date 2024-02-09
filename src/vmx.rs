use std::collections::HashMap;

use anyhow::{bail, Context as _, Error};
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::io::AsyncRead;

static DISK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^((?:scsi|ide|sata|nvme)\d+:\d+)\.fileName$"#)
        .expect("failed to create disk key regex")
});

#[derive(Debug, Default)]
pub struct VmConfig {
    /// maps `scsiX:Y`, `ideX:Y` to file names
    pub disks: HashMap<String, String>,

    pub guest_os: String,

    /// In MB apparently?
    pub mem_size: u64,

    /// "efi" or "...bios" I guess?
    pub firmware: String,

    pub display_name: String,
}

impl VmConfig {
    pub async fn parse<R: AsyncRead + Unpin>(data: R) -> Result<Self, Error> {
        let mut this = Self::default();
        this.parse_do(data).await?;
        Ok(this)
    }

    async fn parse_do<R: AsyncRead + Unpin>(&mut self, input: R) -> Result<(), Error> {
        use tokio::io::AsyncBufReadExt as _;

        let mut reader = tokio::io::BufReader::new(input);

        let mut linebuf = String::new();
        loop {
            linebuf.clear();
            let got = reader.read_line(&mut linebuf).await?;
            if got == 0 {
                break;
            }

            self.parse_do_line(linebuf.trim_end())?;
        }

        Ok(())
    }

    fn parse_do_line(&mut self, line: &str) -> Result<(), Error> {
        let Some((key, value)) = line.split_once('=') else {
            return Ok(());
        };

        let key = key.trim_start().trim_end();
        let mut value = value.trim_start().trim_end();

        if value.starts_with('"') && value.ends_with('"') {
            value = &value[1..(value.len() - 1)];
        }

        if key == "guestOS" {
            self.guest_os = value.to_string();
        } else if key == "memSize" {
            self.mem_size = value.parse().context("failed to parse memory size")?;
        } else if key == "firmware" {
            self.firmware = value.to_string();
        } else if key == "displayName" {
            self.display_name = value.to_string();
        } else if let Some(cap) = DISK_RE.captures(key) {
            let kind = cap.get(1).unwrap();
            if self.disks.contains_key(kind.as_str()) {
                bail!(
                    "vm config contains multiple entries for '{}'",
                    kind.as_str()
                );
            }

            self.disks
                .insert(kind.as_str().to_string(), value.to_string());
        }
        // FIXME: parse other stuff
        // eg.`ethernetX` has addressType="generated" where MAC is in .generatedAddress, so
        // probably also has a *different* way to store fixed MACs?

        Ok(())
    }
}
