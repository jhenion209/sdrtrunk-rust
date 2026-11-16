//! Tuner sources, ported from sdrtrunk `io.github.dsheirer.source`.
#[cfg(target_os = "linux")]
pub mod airspy;
pub mod frequency;
#[cfg(target_os = "linux")]
pub mod rtlsdr;

/// Stored form of a tuner id, from any accepted spelling: an Airspy serial ("735863DC-2F67AFE3", see `format_serial`)
/// or "rtl:" plus an RTL-SDR's USB serial ("rtl:00000001"). None if it is neither.
pub fn normalize_tuner(s: &str) -> Option<String> {
    let s = s.trim();
    if let Some(r) = s.strip_prefix("rtl:") {
        let r = r.trim();
        return (!r.is_empty() && r.len() <= 64 && r.bytes().all(|b| b.is_ascii_graphic())).then(|| format!("rtl:{r}"));
    }
    parse_serial(s).map(format_serial)
}

/// "Airspy 735863DC-2F67AFE3" or "RTL-SDR 00000001", for pages and messages.
pub fn tuner_label(id: &str) -> String {
    match id.strip_prefix("rtl:") { Some(r) => format!("RTL-SDR {r}"), None => format!("Airspy {id}") }
}

/// Airspy serial in sdrtrunk's form: "735863DC-2F67AFE3" (high and low 32 bits, upper-case hex).
pub fn format_serial(sn: u64) -> String { format!("{:08X}-{:08X}", sn >> 32, sn & 0xFFFF_FFFF) }

/// Reads sdrtrunk's form, or airspy_info's "0x735863DC2F67AFE3"; case and the dash do not matter.
pub fn parse_serial(s: &str) -> Option<u64> {
    let s = s.trim();
    let hex: String = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s).chars().filter(|&c| c != '-').collect();
    if hex.is_empty() || hex.len() > 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    u64::from_str_radix(&hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_forms() {
        assert_eq!(format_serial(0x735863DC_2F67AFE3), "735863DC-2F67AFE3");
        assert_eq!(parse_serial("735863DC-2F67AFE3"), Some(0x735863DC_2F67AFE3));
        assert_eq!(parse_serial("0x10a862dc34348363"), Some(0x10A862DC_34348363));
        for bad in ["", "xyz", "735863DC-2F67AFE3-00", "-"] { assert_eq!(parse_serial(bad), None, "{bad}"); }
    }

    #[test]
    fn tuner_ids() {
        assert_eq!(normalize_tuner("0x735863dc2f67afe3").as_deref(), Some("735863DC-2F67AFE3"));
        assert_eq!(normalize_tuner(" rtl: 00000001 ").as_deref(), Some("rtl:00000001"));
        for bad in ["rtl:", "rtl:a b", "nonsense"] { assert_eq!(normalize_tuner(bad), None, "{bad}"); }
        assert_eq!(tuner_label("rtl:00000001"), "RTL-SDR 00000001");
        assert_eq!(tuner_label("735863DC-2F67AFE3"), "Airspy 735863DC-2F67AFE3");
    }
}
