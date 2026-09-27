//! Parsing of the Rockchip `parameter.txt` file that describes the partition layout.
//!
//! Only the keys that matter for making a card are interpreted: the `CMDLINE` `mtdparts` list
//! (every partition as `size@offset(name[:flags])` in 512-byte sectors, `-` meaning "grow to the
//! end of the disk"), `TYPE` and optional `uuid:name=...` overrides, exactly like rkdeveloptool.

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionDef {
    pub name: String,
    /// Start in 512-byte sectors.
    pub offset: u64,
    /// Size in sectors; `None` for a grow partition.
    pub size: Option<u64>,
    pub bootable: bool,
    /// Explicit partition GUID from a `uuid:` line, if any.
    pub uuid: Option<[u8; 16]>,
}

impl PartitionDef {
    pub fn is_grow(&self) -> bool {
        self.size.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameter {
    pub firmware_ver: String,
    pub machine_model: String,
    pub machine_id: String,
    pub manufacturer: String,
    pub machine: String,
    /// `GPT` for GPT layouts (the only layout this tool supports for SD boot cards).
    pub part_type: String,
    pub cmdline: String,
    pub partitions: Vec<PartitionDef>,
}

fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    // Textual UUIDs are big endian; GPT stores the first three fields little endian.
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    Some(raw)
}

impl Parameter {
    pub fn parse(text: &str) -> Result<Parameter> {
        let mut p = Parameter::default();
        let mut uuids: Vec<(String, [u8; 16])> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("uuid:") {
                if let Some((name, val)) = rest.split_once('=') {
                    if let Some(u) = parse_uuid(val) {
                        uuids.push((name.trim().to_string(), u));
                    }
                }
                continue;
            }
            let Some((key, val)) = line.split_once(':') else { continue };
            let key = key.trim();
            let val = val.trim();
            match key {
                "FIRMWARE_VER" => p.firmware_ver = val.into(),
                "MACHINE_MODEL" => p.machine_model = val.into(),
                "MACHINE_ID" => p.machine_id = val.into(),
                "MANUFACTURER" => p.manufacturer = val.into(),
                "MACHINE" => p.machine = val.into(),
                "TYPE" => p.part_type = val.to_ascii_uppercase(),
                "CMDLINE" => {
                    p.cmdline = val.into();
                    p.partitions = parse_mtdparts(val)?;
                }
                _ => {}
            }
        }
        if p.partitions.is_empty() {
            bail!("parameter has no mtdparts partition list");
        }
        for part in &mut p.partitions {
            if let Some((_, u)) = uuids.iter().find(|(n, _)| n == &part.name) {
                part.uuid = Some(*u);
            }
        }
        Ok(p)
    }

    pub fn partition(&self, name: &str) -> Option<&PartitionDef> {
        self.partitions.iter().find(|p| p.name == name)
    }

    /// The last sector (exclusive) used by a fixed-size partition.
    pub fn fixed_end(&self) -> u64 {
        self.partitions.iter().filter_map(|p| p.size.map(|s| p.offset + s)).max().unwrap_or(0)
    }
}

/// Parses `mtdparts=rk29xxnand:size@off(name),...` (the text after the colon is what matters).
pub fn parse_mtdparts(cmdline: &str) -> Result<Vec<PartitionDef>> {
    let Some(pos) = cmdline.find("mtdparts") else { bail!("CMDLINE has no mtdparts") };
    let after = &cmdline[pos..];
    let Some(colon) = after.find(':') else { bail!("mtdparts has no ':'") };
    let list = &after[colon + 1..];
    let mut parts = Vec::new();
    for item in list.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // size@offset(name:flags)
        let Some((size_s, rest)) = item.split_once('@') else { bail!("bad mtdparts item '{item}'") };
        let Some(paren) = rest.find('(') else { bail!("bad mtdparts item '{item}'") };
        let off_s = &rest[..paren];
        let name_s = rest[paren + 1..].trim_end_matches(')');
        let offset = parse_num(off_s).ok_or_else(|| anyhow::anyhow!("bad offset in '{item}'"))?;
        let size = if size_s.trim() == "-" {
            None
        } else {
            Some(parse_num(size_s).ok_or_else(|| anyhow::anyhow!("bad size in '{item}'"))?)
        };
        let (name, flags) = match name_s.split_once(':') {
            Some((n, f)) => (n.to_string(), f.to_string()),
            None => (name_s.to_string(), String::new()),
        };
        let bootable = flags.contains("bootable");
        if size.is_none() && !flags.contains("grow") {
            // "-" without :grow is still a grow partition for the SD tool.
        }
        parts.push(PartitionDef { name, offset, size, bootable, uuid: None });
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SAMPLE: &str = "FIRMWARE_VER: 14.0\nMACHINE_MODEL: RG DS Plus\nTYPE: GPT\nCMDLINE:mtdparts=rk29xxnand:0x00002000@0x00002000(security),0x00000800@0x0000c000(vbmeta),0x00020000@0x0000c800(boot:bootable),-@0x01225400(userdata:grow)\n";
    #[test]
    fn parses() {
        let p = Parameter::parse(SAMPLE).unwrap();
        assert_eq!(p.part_type, "GPT");
        assert_eq!(p.partitions.len(), 4);
        assert_eq!(p.partitions[0], PartitionDef { name: "security".into(), offset: 0x2000, size: Some(0x2000), bootable: false, uuid: None });
        assert!(p.partitions[2].bootable);
        assert!(p.partitions[3].is_grow());
        assert_eq!(p.partitions[3].offset, 0x01225400);
        assert_eq!(p.fixed_end(), 0xc800 + 0x20000);
    }
    #[test]
    fn uuid_override() {
        let t = format!("{SAMPLE}uuid:boot=12345678-9abc-def0-1234-56789abcdef0\n");
        let p = Parameter::parse(&t).unwrap();
        let u = p.partition("boot").unwrap().uuid.unwrap();
        assert_eq!(&u[..4], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(&u[8..], &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
    }
}
