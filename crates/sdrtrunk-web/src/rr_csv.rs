//! RadioReference talkgroup CSV (premium download): parse and compare with this program's talkgroup list.
//! Header `Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category`, RFC 4180 quoting.
use crate::db::Talkgroup;

/// Talkgroup names are limited to this many characters (same as a manual save).
pub const NAME_MAX: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RrTalkgroup { pub number: u16, pub name: String, pub encrypted: bool, pub category: String }

/// Talkgroups in the file, plus one message per skipped row. Err when the file is not a RadioReference talkgroup CSV.
pub fn parse(text: &str) -> Result<(Vec<RrTalkgroup>, Vec<String>), String> {
    let mut lines = text.trim_start_matches('\u{feff}').lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().map(fields).unwrap_or_default();
    if header.len() < 7 || header[0] != "Decimal" || header[2] != "Alpha Tag" || header[3] != "Mode" || header[6] != "Category" {
        return Err("This is not a RadioReference talkgroup CSV (expected the header Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category).".into());
    }
    let (mut rows, mut skipped) = (Vec::<RrTalkgroup>::new(), Vec::new());
    for line in lines {
        let f = fields(line);
        let number = match f.first().and_then(|d| d.trim().parse::<u16>().ok()) {
            Some(n) if n > 0 && f.len() >= 7 => n,
            _ => { skipped.push(format!("skipped \"{}\": not a talkgroup number 1 to 65535", f.first().map_or("", |s| s.as_str()))); continue; }
        };
        if rows.iter().any(|r| r.number == number) { skipped.push(format!("skipped talkgroup {number}: listed twice")); continue; }
        let name = [&f[2], &f[4]].into_iter().map(|s| s.trim()).find(|s| !s.is_empty()).unwrap_or("");
        if name.is_empty() { skipped.push(format!("skipped talkgroup {number}: no name")); continue; }
        let mode = f[3].trim();
        rows.push(RrTalkgroup {
            number,
            name: name.chars().take(NAME_MAX).collect(),
            encrypted: mode.ends_with('E'),
            category: f[6].trim().to_string(),
        });
    }
    Ok((rows, skipped))
}

/// One CSV record split into fields (RFC 4180: quoted fields, `""` inside quotes is a quote).
pub(crate) fn fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => { cur.push('"'); chars.next(); }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[derive(Debug, Default, PartialEq)]
pub struct Comparison {
    /// In RadioReference, not in our list.
    pub new: Vec<RrTalkgroup>,
    /// Same number, different name: (our entry, RadioReference entry).
    pub conflicts: Vec<(Talkgroup, RrTalkgroup)>,
    pub unchanged: usize,
    pub only_local: usize,
}

pub fn compare(existing: &[Talkgroup], rr: &[RrTalkgroup]) -> Comparison {
    let mut c = Comparison::default();
    for r in rr {
        match existing.iter().find(|t| t.number == r.number) {
            None => c.new.push(r.clone()),
            Some(t) if t.name == r.name => c.unchanged += 1,
            Some(t) => c.conflicts.push((t.clone(), r.clone())),
        }
    }
    c.only_local = existing.iter().filter(|t| !rr.iter().any(|r| r.number == t.number)).count();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\r\n\
1,001,\"Wichita PD West\",\"D\",\"West Dispatch\",\"Law Dispatch\",\"Wichita Police\"\r\n\
85,055,\"\",\"DE\",\"Fire, \"\"Ops\"\" 2\",\"Fire Tac\",\"County/ Wichita Fire\"\r\n\
70000,11170,\"Too big\",\"D\",\"x\",\"x\",\"x\"\r\n\
1,001,\"Dup\",\"D\",\"x\",\"x\",\"x\"\r\n";

    #[test]
    fn parses_radioreference_rows() {
        let (rows, skipped) = parse(SAMPLE).unwrap();
        assert_eq!(rows, vec![
            RrTalkgroup { number: 1, name: "Wichita PD West".into(), encrypted: false, category: "Wichita Police".into() },
            RrTalkgroup { number: 85, name: "Fire, \"Ops\" 2".into(), encrypted: true, category: "County/ Wichita Fire".into() },
        ], "empty Alpha Tag falls back to Description; quotes and commas inside fields kept");
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].contains("70000") && skipped[1].contains("listed twice"));
    }

    #[test]
    fn refuses_other_files() {
        assert!(parse("P25, 1, \"x\"\n").is_err(), "DSD format");
        assert!(parse("").is_err());
        let long = format!("Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\n5,005,\"{}\",\"TE\",\"\",\"\",\"\"\n", "é".repeat(70));
        let (rows, _) = parse(&long).unwrap();
        assert_eq!(rows[0].name.chars().count(), NAME_MAX, "names cut to the talkgroup name limit");
        assert!(rows[0].encrypted, "TE is encrypted too");
    }

    #[test]
    fn compares_with_local_list() {
        let tg = |n: u16, name: &str| Talkgroup { number: n, name: name.into(), record: true };
        let rr = |n: u16, name: &str| RrTalkgroup { number: n, name: name.into(), encrypted: false, category: "C".into() };
        let c = compare(&[tg(1, "SG WPD West"), tg(3, "Same"), tg(9, "Only here")], &[rr(1, "Wichita PD West"), rr(3, "Same"), rr(5, "North")]);
        assert_eq!(c.new, vec![rr(5, "North")]);
        assert_eq!(c.conflicts, vec![(tg(1, "SG WPD West"), rr(1, "Wichita PD West"))]);
        assert_eq!((c.unchanged, c.only_local), (1, 1));
    }
}

/// Manual check against a real download: SDRTRUNK_RR_CSV=path cargo test -p sdrtrunk-web --lib rr_csv -- --ignored --nocapture
#[test]
#[ignore]
fn real_download() {
    let text = std::fs::read_to_string(std::env::var("SDRTRUNK_RR_CSV").expect("SDRTRUNK_RR_CSV")).unwrap();
    let (rows, skipped) = parse(&text).unwrap();
    println!("{} talkgroups, {} encrypted, {} skipped {:?}; first {:?}", rows.len(), rows.iter().filter(|r| r.encrypted).count(), skipped.len(), skipped, rows.first());
}
