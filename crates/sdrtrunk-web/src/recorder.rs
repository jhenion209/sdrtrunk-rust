//! Call recorder: grant -> voice channel -> one MP3 per transmission, named and tagged like sdrtrunk's so ScannerPulse
//! ingests them unchanged. Encrypted transmissions (LDU1 link control or LDU2 ESS) and talkgroups set not to record
//! are never written.
use crate::db::Talkgroup;
use crate::streaming::{title, StreamCall, Streams};
use chrono::{Local, TimeZone};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use sdrtrunk_audio::{mp3, Imbe};
use sdrtrunk_p25::control::{ControlChannel, ControlEvent, ImbeFrame};
use sdrtrunk_p25::trunk::CallGrant;

/// A transmission ends this long after its last voice frame when no terminator arrived (ms).
pub const END_MS: i64 = 1_000;
/// A quiet voice channel is released after this long (ms). Longer than `TrunkTracker::hold_ms` (5 s): within the hold a
/// rekey on the same channel produces no new grant, so the channel must still be open to hear it.
pub const RELEASE_MS: i64 = 6_000;

/// Opens and closes voice channels; `ControlChannel` in the receiver, a fake in tests.
pub trait Tuner {
    fn open(&mut self, hz: i64) -> Result<u32, String>;
    fn close(&mut self, id: u32);
}

impl Tuner for ControlChannel {
    fn open(&mut self, hz: i64) -> Result<u32, String> { self.add_voice(hz) }
    fn close(&mut self, id: u32) { self.remove_voice(id) }
}

/// What to record: listed talkgroups follow their own flag, unlisted ones follow `record_unlisted`. `streamed`
/// talkgroups are followed for their streams whether recorded or not. Shared with the web pages so edits apply to the
/// next call without a restart.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordPolicy { pub record_unlisted: bool, pub talkgroups: Vec<Talkgroup>, pub streamed: Vec<u16> }

impl Default for RecordPolicy {
    fn default() -> Self { RecordPolicy { record_unlisted: true, talkgroups: Vec::new(), streamed: Vec::new() } }
}

/// Names written into file names and tags (sdrtrunk's alias-list System, Site and channel Name).
#[derive(Clone, Debug)]
pub struct Names { pub system: String, pub site: String, pub channel: String }

struct Segment {
    start_ms: i64,
    last_ms: i64,
    group: u16,
    source: u32,
    /// None until the link control or ESS says.
    encrypted: Option<bool>,
    imbe: Imbe,
    pending: Vec<ImbeFrame>,
    pcm: Vec<i16>,
}

struct Chan { grant: CallGrant, last_ms: i64, seg: Option<Segment> }

pub struct Recorder {
    dir: PathBuf,
    names: Names,
    policy: Arc<Mutex<RecordPolicy>>,
    chans: HashMap<u32, Chan>,
    /// Where finished calls of streamed talkgroups go, and the alias list they belong to.
    streams: Option<(Arc<Streams>, String)>,
    pub recorded: u64,
    pub last_error: Option<String>,
}

impl Recorder {
    pub fn new(dir: &Path, names: Names, policy: Arc<Mutex<RecordPolicy>>) -> Recorder {
        Recorder { dir: dir.into(), names, policy, chans: HashMap::new(), streams: None, recorded: 0, last_error: None }
    }

    /// Hands finished calls of streamed talkgroups (alias list `list`) to `streams`.
    pub fn streaming(self, streams: Arc<Streams>, list: String) -> Recorder { Recorder { streams: Some((streams, list)), ..self } }

    /// Listed talkgroups follow their record flag; unlisted ones follow the global switch. Returns (record, name).
    fn policy(&self, group: u16) -> (bool, Option<String>) {
        let p = self.policy.lock().unwrap_or_else(|p| p.into_inner());
        p.talkgroups.iter().find(|t| t.number == group).map_or((p.record_unlisted, None), |t| (t.record, Some(t.name.clone())))
    }

    fn streamed(&self, group: u16) -> bool { self.policy.lock().unwrap_or_else(|p| p.into_inner()).streamed.contains(&group) }

    pub fn handle(&mut self, e: &ControlEvent, now_ms: i64, tuner: &mut dyn Tuner) {
        match e {
            ControlEvent::Call(g) => self.grant(g, now_ms, tuner),
            ControlEvent::Voice { id, frames, ess, lc } => {
                let Some(ch) = self.chans.get_mut(id) else { return };
                ch.last_ms = now_ms;
                let lc_voice = lc.and_then(|l| l.group_voice().map(|(g, s)| (g, s, l.is_encrypted())));
                // The channel now carries another talkgroup than the one granted (grant missed): new transmission.
                if let Some((g, _, _)) = lc_voice {
                    if ch.seg.as_ref().is_some_and(|s| s.group != g) { let seg = ch.seg.take(); self.finish(*id, seg); }
                }
                let ch = self.chans.get_mut(id).expect("still open");
                let seg = ch.seg.get_or_insert_with(|| Segment {
                    start_ms: now_ms, last_ms: now_ms, group: ch.grant.group, source: ch.grant.source, encrypted: None,
                    imbe: Imbe::new(), pending: Vec::new(), pcm: Vec::new(),
                });
                seg.last_ms = now_ms;
                if let Some((g, s, enc)) = lc_voice {
                    seg.group = g;
                    if s != 0 { seg.source = s; }
                    seg.encrypted.get_or_insert(enc);
                }
                if let Some(ess) = ess { seg.encrypted = Some(ess.is_encrypted()); }
                match seg.encrypted {
                    Some(true) => seg.pending.clear(),
                    Some(false) => {
                        for f in seg.pending.drain(..).chain(frames.iter().copied()) { seg.pcm.extend_from_slice(&seg.imbe.decode(&f).0); }
                    }
                    None => seg.pending.extend_from_slice(frames),
                }
            }
            ControlEvent::VoiceEnd { id } => {
                let Some(ch) = self.chans.get_mut(id) else { return };
                ch.last_ms = now_ms;
                let seg = ch.seg.take();
                self.finish(*id, seg);
            }
            ControlEvent::Site { .. } => {}
        }
    }

    fn grant(&mut self, g: &CallGrant, now_ms: i64, tuner: &mut dyn Tuner) {
        let existing = self.chans.iter().find(|(_, c)| c.grant.hz == g.hz).map(|(&id, _)| id);
        if !self.policy(g.group).0 && !self.streamed(g.group) {
            // The frequency now carries a talkgroup we neither record nor stream: stop following it.
            if let Some(id) = existing { self.release(id, tuner); }
            return;
        }
        match existing {
            Some(id) => {
                let ch = self.chans.get_mut(&id).expect("listed");
                ch.last_ms = now_ms;
                if ch.grant.group != g.group { let seg = ch.seg.take(); self.finish(id, seg); }
                self.chans.get_mut(&id).expect("listed").grant = g.clone();
            }
            None => match tuner.open(g.hz as i64) {
                Ok(id) => { self.chans.insert(id, Chan { grant: g.clone(), last_ms: now_ms, seg: None }); }
                Err(e) => self.error(format!("cannot follow TG {} on {} Hz: {e}", g.group, g.hz)),
            },
        }
    }

    /// Ends transmissions that went quiet without a terminator and releases idle voice channels.
    /// Voice channels being followed now: (Hz, talkgroup, talkgroup name if listed, encrypted). Encrypted: the grant
    /// said so, or the transmission's link control or ESS did.
    pub fn following(&self) -> Vec<(u64, u16, Option<String>, bool)> {
        self.chans.values().map(|c| {
            let encrypted = c.grant.encrypted || c.seg.as_ref().is_some_and(|s| s.encrypted == Some(true));
            (c.grant.hz, c.grant.group, self.policy(c.grant.group).1, encrypted)
        }).collect()
    }

    pub fn tick(&mut self, now_ms: i64, tuner: &mut dyn Tuner) {
        let ids: Vec<u32> = self.chans.keys().copied().collect();
        for id in ids {
            let ch = self.chans.get_mut(&id).expect("listed");
            if ch.seg.as_ref().is_some_and(|s| now_ms - s.last_ms > END_MS) { let seg = ch.seg.take(); self.finish(id, seg); }
            if self.chans[&id].seg.is_none() && now_ms - self.chans[&id].last_ms > RELEASE_MS { self.release(id, tuner); }
        }
    }

    fn release(&mut self, id: u32, tuner: &mut dyn Tuner) {
        let seg = self.chans.get_mut(&id).and_then(|c| c.seg.take());
        self.finish(id, seg);
        if self.chans.remove(&id).is_some() { tuner.close(id); }
    }

    /// Forgets voice channel `id` without closing it (its tuner is gone): the transmission ends as at a `VoiceEnd`, and
    /// the next grant on its frequency opens a fresh channel.
    pub fn drop_channel(&mut self, id: u32) {
        // `finish` reads the channel's grant, so remove it only afterwards (as `release` does).
        let seg = self.chans.get_mut(&id).and_then(|c| c.seg.take());
        self.finish(id, seg);
        self.chans.remove(&id);
    }

    fn error(&mut self, msg: String) {
        eprintln!("recorder: {msg}");
        self.last_error = Some(msg);
    }

    /// A finished transmission that is known clear and has audio goes to its streams (if streamed) and is written
    /// (if its talkgroup is recorded).
    fn finish(&mut self, id: u32, seg: Option<Segment>) {
        let Some(seg) = seg else { return };
        let (record, alias) = self.policy(seg.group);
        if seg.encrypted != Some(false) || seg.pcm.is_empty() { return; }
        if let Some((streams, list)) = self.streams.as_ref().filter(|_| self.streamed(seg.group)) {
            streams.submit(&StreamCall { alias_list: list.clone(), group: seg.group, start_ms: seg.start_ms,
                title: title(seg.group, alias.as_deref(), seg.source), pcm: seg.pcm.clone() });
        }
        if !record { return; }
        let (name, tags) = file_name_and_tags(&self.names, &seg, alias.as_deref(), &self.chans[&id].grant);
        let path = self.dir.join(&name);
        let tmp = self.dir.join(format!(".{name}.part"));
        let tag_refs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // ponytail: encodes on the decoder thread; a minute of 16 kbps audio takes milliseconds. Move to a writer
        // thread if long calls ever cause dropped samples.
        let result = std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())
            .and_then(|_| mp3::encode(&seg.pcm, &tag_refs))
            .and_then(|bytes| std::fs::write(&tmp, bytes).map_err(|e| e.to_string()))
            .and_then(|_| std::fs::rename(&tmp, &path).map_err(|e| e.to_string()));
        match result {
            Ok(()) => self.recorded += 1,
            Err(e) => self.error(format!("cannot write {}: {e}", path.display())),
        }
    }
}

/// sdrtrunk keeps letters, digits, '-' and '.' in names and turns anything else (spaces) into '-'.
fn clean(s: &str) -> String { s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect() }

/// `20261113_210324_System_Site_Name__TO_23_FROM_703313.mp3` and the ID3 frames sdrtrunk writes.
fn file_name_and_tags(n: &Names, seg: &Segment, alias: Option<&str>, g: &CallGrant) -> (String, Vec<(&'static str, String)>) {
    let t = Local.timestamp_millis_opt(seg.start_ms).single().unwrap_or_else(Local::now);
    let from = if seg.source != 0 { format!("_FROM_{}", seg.source) } else { String::new() };
    let name = format!("{}_{}_{}_{}__TO_{}{from}.mp3", t.format("%Y%m%d_%H%M%S"), clean(&n.system), clean(&n.site), clean(&n.channel), seg.group);
    let date = t.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let title = match alias { Some(a) if !a.is_empty() => format!("{}\"{a}\"", seg.group), _ => seg.group.to_string() };
    let comment = format!("Date:{date};System:{};Site:{};Name:{};Decoder:P25 Phase 1;Channel:{}-{};Frequency:{};",
        n.system, n.site, n.channel, g.channel.band, g.channel.number, g.hz);
    let mut tags = vec![("COMM", comment), ("TALB", n.channel.clone()), ("TCOM", format!("SDRTrunk-Rust {}", env!("CARGO_PKG_VERSION"))),
        ("TCON", "Scanner Audio".into()), ("TDAT", date), ("TIT1", n.system.clone()), ("TIT2", title)];
    if seg.source != 0 { tags.push(("TPE1", seg.source.to_string())); }
    tags.push(("TYER", t.format("%Y").to_string()));
    (name, tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdrtrunk_p25::ess::{EncryptionSync, LinkControl};
    use sdrtrunk_p25::tsbk_msg::Channel;

    #[derive(Default)]
    struct FakeTuner { next: u32, open: Vec<(u32, i64)>, closed: Vec<u32> }
    impl Tuner for FakeTuner {
        fn open(&mut self, hz: i64) -> Result<u32, String> { self.next += 1; self.open.push((self.next, hz)); Ok(self.next) }
        fn close(&mut self, id: u32) { self.closed.push(id); }
    }

    fn dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sdrtrunk-rec-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn names() -> Names { Names { system: "Sedgwick County".into(), site: "Simulcast".into(), channel: "T-Control".into() } }

    fn grant(group: u16, hz: u64) -> ControlEvent {
        ControlEvent::Call(CallGrant { group, source: 0, channel: Channel { band: 0, number: 4016 }, hz, encrypted: false, emergency: false })
    }

    fn lc(group: u16, source: u32, opts: u8) -> LinkControl { LinkControl { lco: 0, mfid: 0, service_options: opts, group, source } }
    fn ess(alg: u8) -> EncryptionSync { EncryptionSync { message_indicator: [0; 9], algorithm: alg, key_id: 0 } }
    fn ldu1(l: Option<LinkControl>) -> ControlEvent { ControlEvent::Voice { id: 1, frames: vec![[false; 144]; 9], ess: None, lc: l } }
    fn ldu2(e: Option<EncryptionSync>) -> ControlEvent { ControlEvent::Voice { id: 1, frames: vec![[false; 144]; 9], ess: e, lc: None } }

    fn files(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn records_clear_call_like_sdrtrunk() {
        let d = dir("clear");
        let tgs = Arc::new(Mutex::new(RecordPolicy { record_unlisted: true, talkgroups: vec![Talkgroup { number: 27, name: "Jail 1".into(), record: true }], streamed: vec![] }));
        let (mut r, mut t) = (Recorder::new(&d, names(), tgs), FakeTuner::default());
        r.handle(&grant(27, 858_737_500), 1_000, &mut t);
        assert_eq!(t.open, vec![(1, 858_737_500)]);
        r.handle(&ldu1(Some(lc(27, 711_013, 0x04))), 1_100, &mut t);
        r.handle(&ldu2(Some(ess(0x80))), 1_280, &mut t);
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 1_300, &mut t);
        assert_eq!(r.recorded, 1);
        let f = files(&d);
        let stamp = Local.timestamp_millis_opt(1_100).unwrap().format("%Y%m%d_%H%M%S").to_string();
        assert_eq!(f, vec![format!("{stamp}_Sedgwick-County_Simulcast_T-Control__TO_27_FROM_711013.mp3")]);
        let bytes = std::fs::read(d.join(&f[0])).unwrap();
        assert_eq!(&bytes[..3], b"ID3");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("27\"Jail 1\"") && text.contains("Frequency:858737500;") && text.contains("Channel:0-4016;"), "{}", &text[..300]);
        assert!(text.contains("System:Sedgwick County;Site:Simulcast;Name:T-Control;Decoder:P25 Phase 1;"));
        assert!(t.closed.is_empty(), "channel kept for a rekey");
    }

    #[test]
    fn dropped_channel_forgotten() {
        let (mut r, mut t) = (Recorder::new(&dir("dropped"), names(), Arc::default()), FakeTuner::default());
        r.handle(&grant(1, 859_962_500), 0, &mut t);
        r.handle(&ldu1(None), 10, &mut t);
        r.drop_channel(1);
        assert!(r.following().is_empty() && t.closed.is_empty(), "forgotten without closing (its tuner is gone)");
        r.handle(&grant(1, 859_962_500), 100, &mut t);
        assert_eq!(t.open, vec![(1, 859_962_500), (2, 859_962_500)], "the next grant opens a fresh channel");
    }

    #[test]
    fn dropped_channel_with_audio_written() {
        let d = dir("dropped-audio");
        let (mut r, mut t) = (Recorder::new(&d, names(), Arc::default()), FakeTuner::default());
        r.handle(&grant(27, 858_737_500), 1_000, &mut t);
        r.handle(&ldu1(Some(lc(27, 711_013, 0x04))), 1_100, &mut t);
        r.handle(&ldu2(Some(ess(0x80))), 1_280, &mut t);
        r.drop_channel(1);
        assert_eq!((r.recorded, files(&d).len()), (1, 1), "the clear call in progress is written");
        assert!(r.following().is_empty() && t.closed.is_empty());
    }

    #[test]
    fn encrypted_never_written() {
        let d = dir("enc");
        let (mut r, mut t) = (Recorder::new(&d, names(), Arc::default()), FakeTuner::default());
        r.handle(&grant(1, 859_962_500), 0, &mut t);
        r.handle(&ldu1(None), 10, &mut t);
        assert!(!r.following()[0].3, "not known encrypted yet");
        r.handle(&ldu2(Some(ess(0x84))), 200, &mut t);
        assert!(r.following()[0].3, "the ESS says encrypted: shown red on the spectrum");
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 300, &mut t);
        r.handle(&ldu1(Some(lc(1, 704_798, 0x44))), 1_000, &mut t);
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 1_100, &mut t);
        r.handle(&ldu1(None), 2_000, &mut t); // unknown: no link control, no ESS
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 2_100, &mut t);
        assert_eq!((r.recorded, files(&d).len()), (0, 0));
    }

    #[test]
    fn record_flag_and_idle_release() {
        let d = dir("flag");
        let tgs = Arc::new(Mutex::new(RecordPolicy { record_unlisted: true, talkgroups: vec![Talkgroup { number: 9, name: "Off".into(), record: false }], streamed: vec![] }));
        let (mut r, mut t) = (Recorder::new(&d, names(), tgs), FakeTuner::default());
        r.handle(&grant(9, 851_000_000), 0, &mut t);
        assert!(t.open.is_empty(), "talkgroup set not to record");
        r.handle(&grant(51, 852_000_000), 0, &mut t);
        r.handle(&ldu1(Some(lc(51, 0, 0))), 100, &mut t);
        r.tick(100 + END_MS, &mut t);
        assert_eq!(r.recorded, 0, "still within the end timeout");
        r.tick(101 + END_MS, &mut t);
        assert_eq!(r.recorded, 1, "quiet transmission ended without a terminator");
        assert!(files(&d)[0].ends_with("__TO_51.mp3"), "no source radio: no FROM");
        r.tick(101 + END_MS + RELEASE_MS, &mut t);
        assert_eq!(t.closed, vec![1], "idle channel released");
    }

    #[test]
    fn streamed_but_not_recorded() {
        use crate::streaming::{StreamConfig, StreamTalkgroup};
        let d = dir("streamed");
        let p = Arc::new(Mutex::new(RecordPolicy { record_unlisted: false,
            talkgroups: vec![Talkgroup { number: 23, name: "WPD West".into(), record: false }], streamed: vec![23] }));
        // A stream whose server is unreachable: calls wait in its queue.
        let streams = Arc::new(Streams::new(std::time::Duration::from_secs(60)));
        streams.apply(vec![StreamConfig { id: 1, name: "WPD West".into(), host: "127.0.0.1".into(), port: 1, mount: "/scan21".into(),
            members: vec![StreamTalkgroup::new("SGCo", 23)], ..StreamConfig::default() }]);
        let (mut r, mut t) = (Recorder::new(&d, names(), p).streaming(streams.clone(), "SGCo".into()), FakeTuner::default());
        let t0 = crate::receiver::now_ms();
        r.handle(&grant(23, 851_000_000), t0, &mut t);
        assert_eq!(t.open.len(), 1, "followed for its stream although not recorded");
        r.handle(&ldu1(Some(lc(23, 703_313, 0))), t0 + 100, &mut t);
        r.tick(t0 + 101 + END_MS, &mut t);
        assert_eq!(r.recorded, 0);
        assert!(files(&d).is_empty(), "no MP3 written");
        let start = std::time::Instant::now();
        while streams.status(1).queued != 1 && start.elapsed().as_secs() < 2 { std::thread::sleep(std::time::Duration::from_millis(20)); }
        assert_eq!(streams.status(1).queued, 1, "the call waits for its stream");
    }

    #[test]
    fn unlisted_follow_the_global_switch() {
        let d = dir("unlisted");
        let p = Arc::new(Mutex::new(RecordPolicy { record_unlisted: false, talkgroups: vec![Talkgroup { number: 51, name: "Fire".into(), record: true }], streamed: vec![] }));
        let (mut r, mut t) = (Recorder::new(&d, names(), p.clone()), FakeTuner::default());
        r.handle(&grant(27, 858_737_500), 0, &mut t);
        assert!(t.open.is_empty(), "unlisted talkgroup not followed when the switch is off");
        r.handle(&grant(51, 852_000_000), 0, &mut t);
        assert_eq!(t.open.len(), 1, "listed talkgroup still recorded");
        r.handle(&ldu1(Some(lc(51, 7, 0))), 10, &mut t);
        p.lock().unwrap().talkgroups.clear(); // removed from the list mid-call
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 100, &mut t);
        assert_eq!(r.recorded, 0, "a talkgroup removed from the list is no longer written");
        p.lock().unwrap().record_unlisted = true;
        r.handle(&grant(27, 858_737_500), 200, &mut t);
        assert_eq!(t.open.len(), 2, "switch on: unlisted followed again");
    }

    #[test]
    fn new_talkgroup_on_channel_starts_new_file() {
        let d = dir("switch");
        let (mut r, mut t) = (Recorder::new(&d, names(), Arc::default()), FakeTuner::default());
        r.handle(&grant(27, 858_737_500), 0, &mut t);
        r.handle(&ldu1(Some(lc(27, 5, 0))), 10, &mut t);
        r.handle(&grant(135, 858_737_500), 1_500, &mut t);
        r.handle(&ldu1(Some(lc(135, 6, 0))), 1_600, &mut t);
        r.handle(&ControlEvent::VoiceEnd { id: 1 }, 1_700, &mut t);
        let f = files(&d);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().any(|n| n.ends_with("__TO_27_FROM_5.mp3")) && f.iter().any(|n| n.ends_with("__TO_135_FROM_6.mp3")));
        assert_eq!(t.open.len(), 1, "same frequency, same voice channel");
    }
}
