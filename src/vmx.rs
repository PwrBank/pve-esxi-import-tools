use std::collections::HashMap;

use anyhow::{bail, Context as _, Error};
use regex::Regex;

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
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let mut this = Self::default();
        this.parse_do(data)?;
        Ok(this)
    }

    fn parse_do(&mut self, data: &[u8]) -> Result<(), Error> {
        let data = std::str::from_utf8(data)
            .context("config file is not valid utf-8")?
            .trim_start();

        let disk_re = Regex::new(r#"^((?:scsi|ide|sata|nvme)\d+:\d+)\.fileName$"#)
            .expect("failed to create disk key regex");

        for line in data.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
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
            } else if let Some(cap) = disk_re.captures(key) {
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
        }

        Ok(())
    }
}
