//! Radio names from an sdrtrunk playlist (`default.xml`): each alias's single radio IDs under its alias list, compared
//! with ours so a re-import adds new names and never overwrites one unasked.
use crate::db::RadioName;
use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    /// Radio names per sdrtrunk alias list, in file order.
    pub by_list: BTreeMap<String, Vec<RadioName>>,
    /// Radio ID ranges (not supported).
    pub ranges_skipped: usize,
    /// One line per other skipped entry.
    pub skipped: Vec<String>,
}

/// P25 radio (unit) IDs are 24 bits; 0 means none.
pub const MAX_RADIO: u32 = 0xFF_FFFF;

/// A name as stored: control characters (line breaks, tabs, NUL) become spaces, so the artist tag stays one line that
/// readers such as ScannerPulse can split into number and name; then trimmed.
pub fn clean_name(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

pub fn parse(xml: &str) -> Result<Parsed, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("not an sdrtrunk playlist: {e}"))?;
    if !doc.root_element().has_tag_name("playlist") { return Err("not an sdrtrunk playlist (no <playlist>)".into()); }
    let mut p = Parsed::default();
    for alias in doc.descendants().filter(|n| n.has_tag_name("alias")) {
        let list = alias.attribute("list").unwrap_or_default();
        let name = clean_name(alias.attribute("name").unwrap_or_default());
        let name = name.as_str();
        for id in alias.children().filter(|n| n.has_tag_name("id")) {
            match id.attribute("type") {
                Some("radioRange") => { p.ranges_skipped += 1; continue; }
                Some("radio") => {}
                _ => continue,
            }
            let value = id.attribute("value").unwrap_or_default();
            let radio = match value.trim().parse::<u32>() {
                Ok(r) if (1..=MAX_RADIO).contains(&r) => r,
                _ => { p.skipped.push(format!("radio {value} in {list}: not a radio ID 1 to {MAX_RADIO}")); continue; }
            };
            if name.is_empty() || name.chars().count() > crate::rr_csv::NAME_MAX {
                p.skipped.push(format!("radio {radio} in {list}: no name, or longer than {} characters", crate::rr_csv::NAME_MAX));
                continue;
            }
            let names = p.by_list.entry(list.to_string()).or_default();
            if let Some(first) = names.iter().find(|r| r.radio == radio) {
                p.skipped.push(format!("radio {radio} in {list}: listed twice, kept {}", first.name));
                continue;
            }
            names.push(RadioName { radio, name: name.to_string() });
        }
    }
    Ok(p)
}

#[derive(Debug, Default, PartialEq)]
pub struct Comparison {
    /// In the playlist, not in ours.
    pub new: Vec<RadioName>,
    /// Same radio, different name: (ours, the playlist's).
    pub conflicts: Vec<(RadioName, RadioName)>,
    pub unchanged: usize,
    /// In ours, not in the playlist (kept).
    pub only_local: usize,
}

pub fn compare(ours: &[RadioName], theirs: &[RadioName]) -> Comparison {
    let mut c = Comparison::default();
    for t in theirs {
        match ours.iter().find(|o| o.radio == t.radio) {
            None => c.new.push(t.clone()),
            Some(o) if o.name == t.name => c.unchanged += 1,
            Some(o) => c.conflicts.push((o.clone(), t.clone())),
        }
    }
    c.only_local = ours.iter().filter(|o| !theirs.iter().any(|t| t.radio == o.radio)).count();
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn(radio: u32, name: &str) -> RadioName { RadioName { radio, name: name.into() } }

    const XML: &str = r#"<playlist>
      <alias name="K315" list="SGCo P25"><id type="priority" priority="-1"/>
        <id type="radio" value="733730" protocol="APCO25"/><id type="radio" value="733083" protocol="APCO25"/>
        <id type="radioRange" min="1" max="9" protocol="APCO25"/><id type="talkgroup" value="27" protocol="APCO25"/></alias>
      <alias name="K74" list="SGCo P25"><id type="radio" value="733730" protocol="APCO25"/></alias>
      <alias name="Unit 5" list="KSICS"><id type="radio" value="5" protocol="APCO25"/></alias>
      <alias name="Bad" list="KSICS"><id type="radio" value="0" protocol="APCO25"/></alias>
      <alias name=" " list="KSICS"><id type="radio" value="6" protocol="APCO25"/></alias>
    </playlist>"#;

    #[test]
    fn parses_radios_and_counts_ranges() {
        let p = parse(XML).unwrap();
        assert_eq!(p.by_list["SGCo P25"], vec![rn(733_730, "K315"), rn(733_083, "K315")]);
        assert_eq!(p.by_list["KSICS"], vec![rn(5, "Unit 5")]);
        assert_eq!(p.ranges_skipped, 1);
        assert!(parse("<nope/>").is_err() && parse("not xml").is_err());
    }

    #[test]
    fn control_characters_in_names_become_spaces() {
        let p = parse(r#"<playlist><alias name="Car&#10;12&#9;A" list="L"><id type="radio" value="7"/></alias></playlist>"#).unwrap();
        assert_eq!(p.by_list["L"], vec![rn(7, "Car 12 A")]);
        assert_eq!(clean_name(" a\u{0}b\r\n "), "a b");
    }

    #[test]
    fn repeat_id_first_wins() {
        let p = parse(XML).unwrap();
        assert!(p.skipped.contains(&"radio 733730 in SGCo P25: listed twice, kept K315".to_string()), "{:?}", p.skipped);
        assert!(p.skipped.iter().any(|s| s.contains("radio 0")), "{:?}", p.skipped);
        assert!(p.skipped.iter().any(|s| s.contains("radio 6")), "no name: {:?}", p.skipped);
    }

    #[test]
    fn compare_sorts_new_conflict_unchanged_local() {
        let ours = [rn(1, "Same"), rn(2, "Mine"), rn(3, "Only ours")];
        let theirs = [rn(1, "Same"), rn(2, "Theirs"), rn(4, "New")];
        assert_eq!(compare(&ours, &theirs), Comparison {
            new: vec![rn(4, "New")], conflicts: vec![(rn(2, "Mine"), rn(2, "Theirs"))], unchanged: 1, only_local: 1,
        });
    }
}
