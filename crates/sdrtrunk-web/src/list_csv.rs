//! This program's own CSV files for an alias list: talkgroups (`number,name,record`) and radio names (`radio,name`).
//! Written by the Talkgroups page's "Export CSV" links and read back by its imports. RFC 4180 quoting (`rr_csv::fields`).
use std::fmt::Write;

use crate::db::{RadioName, Talkgroup};
use crate::radio_names::{clean_name, MAX_RADIO};
use crate::rr_csv::{fields, NAME_MAX};

const TALKGROUP_HEADER: [&str; 3] = ["number", "name", "record"];
const RADIO_HEADER: [&str; 2] = ["radio", "name"];

/// A field as written: quoted (quotes doubled) only when it holds a comma, a quote or a line break.
fn quote(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// The talkgroups as a file: header `number,name,record`, sorted by number, `record` is `yes` or `no`.
pub fn write_talkgroups(ts: &[Talkgroup]) -> String {
    let mut ts: Vec<&Talkgroup> = ts.iter().collect();
    ts.sort_by_key(|t| t.number);
    let mut out = String::from("number,name,record\n");
    for t in ts { let _ = writeln!(out, "{},{},{}", t.number, quote(&t.name), if t.record { "yes" } else { "no" }); }
    out
}

/// The radio names as a file: header `radio,name`, sorted by radio.
pub fn write_radios(rs: &[RadioName]) -> String {
    let mut rs: Vec<&RadioName> = rs.iter().collect();
    rs.sort_by_key(|r| r.radio);
    let mut out = String::from("radio,name\n");
    for r in rs { let _ = writeln!(out, "{},{}", r.radio, quote(&r.name)); }
    out
}

/// The data rows (fields trimmed) after a header equal to `want`, ignoring case; None for another header. A byte-order
/// mark and blank lines are ignored; `lines` takes CRLF as well as LF.
fn rows<'a>(text: &'a str, want: &'a [&str]) -> Option<impl Iterator<Item = Vec<String>> + 'a> {
    let mut lines = text.trim_start_matches('\u{feff}').lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().map(fields)?;
    let same = header.len() == want.len() && header.iter().zip(want).all(|(h, w)| h.trim().eq_ignore_ascii_case(w));
    same.then(|| lines.map(|l| fields(l).into_iter().map(|f| f.trim().to_string()).collect()))
}

/// The file starts with the talkgroup export's header.
pub fn is_talkgroup_file(text: &str) -> bool { rows(text, &TALKGROUP_HEADER).is_some() }

/// A name as a manual save takes it: cleaned, 1 to `NAME_MAX` characters.
pub fn check_name(raw: &str) -> Result<String, String> {
    let name = clean_name(raw);
    match name.chars().count() {
        0 => Err("no name".into()),
        n if n > NAME_MAX => Err(format!("name longer than {NAME_MAX} characters")),
        _ => Ok(name),
    }
}

/// A `record` field: `yes`, `true`, `1` or `no`, `false`, `0`, any case.
pub fn parse_record(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() { "yes" | "true" | "1" => Some(true), "no" | "false" | "0" => Some(false), _ => None }
}

/// Talkgroups in the file (the first of a number wins), plus one message per skipped row. Err for another header.
pub fn parse_talkgroups(text: &str) -> Result<(Vec<Talkgroup>, Vec<String>), String> {
    let rows = rows(text, &TALKGROUP_HEADER)
        .ok_or("This is not a talkgroup CSV from this program (expected the header number,name,record).")?;
    let (mut out, mut skipped) = (Vec::<Talkgroup>::new(), Vec::new());
    for f in rows {
        let cell = |i: usize| f.get(i).map_or("", String::as_str);
        let number = match cell(0).parse::<u16>() {
            Ok(n) if n > 0 => n,
            _ => { skipped.push(format!("skipped \"{}\": not a talkgroup number 1 to 65535", cell(0))); continue; }
        };
        if out.iter().any(|t| t.number == number) { skipped.push(format!("skipped talkgroup {number}: listed twice")); continue; }
        let name = match check_name(cell(1)) {
            Ok(n) => n,
            Err(why) => { skipped.push(format!("skipped talkgroup {number}: {why}")); continue; }
        };
        let Some(record) = parse_record(cell(2)) else {
            skipped.push(format!("skipped talkgroup {number}: record \"{}\" is not yes or no", cell(2)));
            continue;
        };
        out.push(Talkgroup { number, name, record });
    }
    Ok((out, skipped))
}

/// Radio names in the file (the first of a radio wins), plus one message per skipped row. Err for another header.
pub fn parse_radios(text: &str) -> Result<(Vec<RadioName>, Vec<String>), String> {
    let rows = rows(text, &RADIO_HEADER).ok_or("This is not a radio name CSV from this program (expected the header radio,name).")?;
    let (mut out, mut skipped) = (Vec::<RadioName>::new(), Vec::new());
    for f in rows {
        let cell = |i: usize| f.get(i).map_or("", String::as_str);
        let radio = match cell(0).parse::<u32>() {
            Ok(r) if (1..=MAX_RADIO).contains(&r) => r,
            _ => { skipped.push(format!("skipped \"{}\": not a radio ID 1 to {MAX_RADIO}", cell(0))); continue; }
        };
        if out.iter().any(|r| r.radio == radio) { skipped.push(format!("skipped radio {radio}: listed twice")); continue; }
        match check_name(cell(1)) {
            Ok(name) => out.push(RadioName { radio, name }),
            Err(why) => skipped.push(format!("skipped radio {radio}: {why}")),
        }
    }
    Ok((out, skipped))
}

/// A download's file name: `<list>-<what>-<YYYY-MM-DD>.csv`, the list's characters outside `[A-Za-z0-9._-]` as `_`
/// (no path separators or quotes reach `Content-Disposition`).
pub fn file_name(list: &str, what: &str, date: chrono::NaiveDate) -> String {
    let safe: String = list.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    format!("{safe}-{what}-{}.csv", date.format("%Y-%m-%d"))
}

/// How a file's talkgroups relate to the list's: `conflicts` are (ours, the file's) where the name or record differs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TgComparison { pub new: Vec<Talkgroup>, pub conflicts: Vec<(Talkgroup, Talkgroup)>, pub unchanged: usize, pub only_local: usize }

pub fn compare_talkgroups(ours: &[Talkgroup], file: &[Talkgroup]) -> TgComparison {
    let mut c = TgComparison::default();
    for t in file {
        match ours.iter().find(|o| o.number == t.number) {
            None => c.new.push(t.clone()),
            Some(o) if o == t => c.unchanged += 1,
            Some(o) => c.conflicts.push((o.clone(), t.clone())),
        }
    }
    c.only_local = ours.iter().filter(|o| !file.iter().any(|t| t.number == o.number)).count();
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{RadioName, Talkgroup};

    fn tg(n: u16, name: &str, record: bool) -> Talkgroup { Talkgroup { number: n, name: name.into(), record } }

    #[test]
    fn talkgroups_round_trip_with_awkward_names() {
        let ts = vec![tg(1, "Fire, \"Tac\" Ü", true), tg(85, "Plain", false)];
        let text = write_talkgroups(&ts);
        assert_eq!(text.lines().next(), Some("number,name,record"));
        assert!(text.contains("1,\"Fire, \"\"Tac\"\" Ü\",yes"));
        assert!(is_talkgroup_file(&text));
        assert_eq!(parse_talkgroups(&text).unwrap(), (ts, vec![]));
    }

    #[test]
    fn talkgroups_from_a_spreadsheet() {
        let text = "\u{feff}Number,Name,Record\r\n5,E,YES\r\n6,F,0\r\n5,dup,yes\r\n0,zero,yes\r\n7,,yes\r\n8,G,maybe\r\n\r\n";
        let (rows, skipped) = parse_talkgroups(text).unwrap();
        assert_eq!(rows, vec![tg(5, "E", true), tg(6, "F", false)]);
        assert_eq!(skipped.len(), 4, "{skipped:?}"); // dup 5, 0, empty name, bad record
    }

    #[test]
    fn wrong_header_is_refused() {
        assert!(!is_talkgroup_file("Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\n"));
        assert!(parse_talkgroups("a,b\n1,2\n").is_err());
        assert!(parse_radios("number,name,record\n").is_err());
    }

    #[test]
    fn radios_round_trip_and_limits() {
        let rs = vec![RadioName { radio: 7, name: "Engine, 7".into() }, RadioName { radio: 0xFF_FFFF, name: "Max".into() }];
        let text = write_radios(&rs);
        assert_eq!(text.lines().next(), Some("radio,name"));
        assert_eq!(parse_radios(&text).unwrap(), (rs, vec![]));
        let (rows, skipped) = parse_radios("radio,name\n16777216,Too big\n9,\n9,Nine\n").unwrap();
        assert_eq!((rows.len(), skipped.len()), (1, 2));
    }

    #[test]
    fn compare_finds_new_and_differences() {
        let ours = vec![tg(1, "A", true), tg(2, "B", true), tg(3, "C", true)];
        let file = vec![tg(1, "A", true), tg(2, "B", false), tg(4, "D", true)];
        let c = compare_talkgroups(&ours, &file);
        assert_eq!((c.new, c.unchanged, c.only_local), (vec![tg(4, "D", true)], 1, 1));
        assert_eq!(c.conflicts, vec![(tg(2, "B", true), tg(2, "B", false))]);
    }

    #[test]
    fn safe_file_names() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        assert_eq!(file_name("KSICS", "talkgroups", d), "KSICS-talkgroups-2026-11-24.csv");
        assert_eq!(file_name("a/b \"c\"", "radio-names", d), "a_b__c_-radio-names-2026-11-24.csv");
    }
}
