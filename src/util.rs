//! Small helpers shared by the CLI and the GUI.

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// Decimal GB the way card vendors print it.
pub fn vendor_gb(n: u64) -> String {
    format!("{:.1} GB", n as f64 / 1e9)
}

/// Parses a size such as `32G`, `29.7GiB`, `250347520s` (sectors), `64000000000` (bytes), or
/// `128GB`. Suffixes: K/M/G/T (binary), KB/MB/GB/TB (decimal), KiB/MiB/GiB/TiB (binary), s (512-byte
/// sectors).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().replace(' ', "");
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let split = lower.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(lower.len());
    let (num, unit) = lower.split_at(split);
    let v: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "" | "b" => 1.0,
        "s" | "sec" | "sectors" => 512.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0 * 1024.0,
        "g" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    let bytes = v * mult;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizes() {
        assert_eq!(parse_size("32G"), Some(32 << 30));
        assert_eq!(parse_size("128GB"), Some(128_000_000_000));
        assert_eq!(parse_size("250347520s"), Some(250347520 * 512));
        assert_eq!(parse_size("1.5 GiB"), Some(3 << 29));
        assert_eq!(parse_size("bad"), None);
        assert_eq!(human_bytes(250347520 * 512), "119.38 GiB");
    }
}
