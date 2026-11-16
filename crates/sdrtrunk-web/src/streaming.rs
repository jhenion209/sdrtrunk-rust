//! Icecast streaming, like the Java reference's `AudioStreamingBroadcaster` + `IcecastHTTPAudioBroadcaster`: each
//! enabled stream keeps one HTTP source connection to its Icecast server and plays the finished, clear calls of its
//! talkgroups in order, paced in real time, with silence between them so listeners stay connected. Calls older than
//! the stream's maximum age are dropped; a lost connection is retried every 30 s. As each call starts, its title
//! ("TO:23 WPD West FROM:703313") goes to the server's `/admin/metadata`.
use crate::db::FieldError;
use crate::receiver::now_ms;
use sdrtrunk_audio::mp3::StreamEncoder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One talkgroup a stream carries: its number in an alias list (the same number is a different talkgroup in another
/// system's list).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StreamTalkgroup { pub list: String, pub group: u16 }

impl StreamTalkgroup {
    pub fn new(list: &str, group: u16) -> StreamTalkgroup { StreamTalkgroup { list: list.into(), group } }
}

/// One Icecast stream and the talkgroups (from any alias lists) it carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
    /// Database row id (not part of the stored JSON); 0 before the first save.
    #[serde(skip)]
    pub id: i64,
    pub enabled: bool,
    pub name: String,
    pub description: String,
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub user: String,
    /// Sent to the server only; never shown on a page.
    pub password: String,
    pub public: bool,
    pub bitrate_kbps: u16,
    pub sample_rate: u32,
    /// Calls wait this long before they play, and are dropped once this old.
    pub delay_ms: u64,
    pub max_age_ms: u64,
    pub members: Vec<StreamTalkgroup>,
    /// Streams saved before `members` had one alias list and its talkgroup numbers: read (see `upgraded`), never written.
    #[serde(default, skip_serializing, rename = "alias_list")]
    pub legacy_list: String,
    #[serde(default, skip_serializing, rename = "talkgroups")]
    pub legacy_groups: Vec<u16>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            id: 0, enabled: true, name: String::new(), description: String::new(), host: String::new(), port: 8000,
            mount: "/".into(), user: "source".into(), password: String::new(), public: false, bitrate_kbps: 16,
            sample_rate: 8_000, delay_ms: 0, max_age_ms: 600_000, members: Vec::new(), legacy_list: String::new(),
            legacy_groups: Vec::new(),
        }
    }
}

fn err(field: &'static str, message: &str) -> FieldError { FieldError { field, message: message.into() } }

impl StreamConfig {
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        if self.name.trim().is_empty() || self.name.len() > 60 { e.push(err("name", "Names are 1 to 60 characters.")); }
        if self.host.is_empty() || self.host.chars().any(|c| c.is_whitespace() || c == '/' || c == ':') {
            e.push(err("host", "Enter the server's name or address, such as icecast.local."));
        }
        if self.port == 0 { e.push(err("port", "Enter a port such as 8000.")); }
        if !self.mount.starts_with('/') || self.mount.len() < 2 || self.mount.chars().any(|c| c.is_whitespace() || c == '?' || c == '#') {
            e.push(err("mount", "Mounts start with / and have no spaces, such as /scan21."));
        }
        if self.user.is_empty() { e.push(err("user", "Enter the source username (Icecast's is usually \"source\").")); }
        if ![8, 16, 24, 32, 48, 64].contains(&self.bitrate_kbps) { e.push(err("bitrate_kbps", "Choose a bitrate.")); }
        if ![8_000, 16_000].contains(&self.sample_rate) { e.push(err("sample_rate", "Choose 8 or 16 kHz.")); }
        if self.delay_ms > 3_600_000 { e.push(err("delay_ms", "Delay at most 60 minutes.")); }
        if !(10_000..=3_600_000).contains(&self.max_age_ms) { e.push(err("max_age_ms", "Maximum age is 10 seconds to 60 minutes.")); }
        if self.members.is_empty() { e.push(err("talkgroups", "Enter at least one talkgroup.")); }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// Whether a call on talkgroup `group` of alias list `list` plays on this stream.
    pub fn carries(&self, list: &str, group: u16) -> bool { self.enabled && self.members.iter().any(|m| m.list == list && m.group == group) }

    /// A stream as stored before talkgroups could come from several lists, in the current form.
    pub fn upgraded(mut self) -> StreamConfig {
        if self.members.is_empty() {
            self.members = self.legacy_groups.iter().map(|&g| StreamTalkgroup::new(&self.legacy_list, g)).collect();
        }
        (self.legacy_list, self.legacy_groups) = (String::new(), Vec::new());
        self
    }
}

/// Our alias list for an sdrtrunk alias list: the same name (any case), else the longest of ours that its name starts
/// with ("SGCo P25" -> "SGCo"); None when nothing matches.
pub fn guess_list(source: &str, ours: &[String]) -> Option<String> {
    let s = source.to_lowercase();
    ours.iter().find(|o| o.to_lowercase() == s)
        .or_else(|| ours.iter().filter(|o| s.starts_with(&o.to_lowercase())).max_by_key(|o| o.len()))
        .cloned()
}

/// Imported streams with their sdrtrunk alias lists renamed to ours by `map`; talkgroups of unmapped lists are left out.
pub fn map_lists(found: Vec<StreamConfig>, map: &HashMap<String, String>) -> Vec<StreamConfig> {
    found.into_iter().map(|c| {
        let mut members: Vec<StreamTalkgroup> = c.members.iter()
            .filter_map(|m| map.get(&m.list).filter(|l| !l.is_empty()).map(|l| StreamTalkgroup::new(l, m.group))).collect();
        members.sort();
        members.dedup();
        StreamConfig { members, ..c }
    }).collect()
}

/// A finished, clear call: 8 kHz audio and its title.
#[derive(Clone, Debug)]
pub struct StreamCall { pub alias_list: String, pub group: u16, pub start_ms: i64, pub title: String, pub pcm: Vec<i16> }

/// The Java reference's title: "TO:23 WPD West FROM:703313" (the talkgroup's name when listed; FROM:UNKNOWN without a
/// radio id).
pub fn title(group: u16, name: Option<&str>, source: u32) -> String {
    let to = match name { Some(n) if !n.is_empty() => format!("TO:{group} {n}"), _ => format!("TO:{group}") };
    if source == 0 { format!("{to} FROM:UNKNOWN") } else { format!("{to} FROM:{source}") }
}

/// What a stream is doing: "connected", "connecting", "error" or "off", calls waiting and sent, the last call's
/// start, and the last problem.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StreamStatus { pub state: String, pub error: Option<String>, pub queued: usize, pub sent: u64, pub last_sent_ms: i64 }

struct Worker { config: StreamConfig, tx: Sender<StreamCall>, status: Arc<Mutex<StreamStatus>>, stop: Arc<AtomicBool>, handle: Option<JoinHandle<()>> }

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

/// The running streams, by id, and stream imports waiting for confirmation, by username.
pub struct Streams { retry: Duration, workers: Mutex<BTreeMap<i64, Worker>>, pub pending: Mutex<HashMap<String, Vec<StreamConfig>>> }

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { m.lock().unwrap_or_else(|p| p.into_inner()) }

impl Streams {
    /// `retry`: how long after a failed or lost connection to try again (30 s; shorter in tests).
    pub fn new(retry: Duration) -> Streams { Streams { retry, workers: Mutex::default(), pending: Mutex::default() } }

    /// Stops all workers together (each may be mid-way through a second of audio), then waits for them.
    fn stop_workers(w: &mut BTreeMap<i64, Worker>) {
        for worker in w.values() { worker.stop.store(true, Ordering::Relaxed); }
        w.clear();
    }

    /// Stops every stream and starts the enabled ones as configured.
    pub fn apply(&self, configs: Vec<StreamConfig>) {
        let mut w = lock(&self.workers);
        Self::stop_workers(&mut w);
        for c in configs.into_iter().filter(|c| c.enabled) {
            let (tx, rx) = channel();
            let status = Arc::new(Mutex::new(StreamStatus { state: "connecting".into(), ..StreamStatus::default() }));
            let stop = Arc::new(AtomicBool::new(false));
            let (cfg, st, flag, retry) = (c.clone(), status.clone(), stop.clone(), self.retry);
            let handle = std::thread::Builder::new().name(format!("stream-{}", c.id))
                .spawn(move || run(cfg, rx, st, flag, retry)).expect("spawn stream");
            w.insert(c.id, Worker { config: c, tx, status, stop, handle: Some(handle) });
        }
    }

    /// Queues a finished call on every enabled stream that carries its talkgroup.
    pub fn submit(&self, call: &StreamCall) {
        for w in lock(&self.workers).values().filter(|w| w.config.carries(&call.alias_list, call.group)) {
            let _ = w.tx.send(call.clone());
        }
    }

    /// A stream's status; "off" when it is not running.
    pub fn status(&self, id: i64) -> StreamStatus {
        lock(&self.workers).get(&id).map(|w| lock(&w.status).clone()).unwrap_or(StreamStatus { state: "off".into(), ..StreamStatus::default() })
    }

    pub fn stop_all(&self) { Self::stop_workers(&mut lock(&self.workers)); }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 { s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' }); }
    }
    s
}

/// Percent-encodes everything but unreserved characters (for the title request's query).
fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A header value: no line breaks.
fn clean(s: &str) -> String { s.chars().filter(|c| !c.is_control()).collect() }

fn auth(c: &StreamConfig) -> String { format!("Basic {}", base64(format!("{}:{}", c.user, c.password).as_bytes())) }

fn open(c: &StreamConfig) -> Result<TcpStream, String> {
    let addr = (c.host.as_str(), c.port).to_socket_addrs().map_err(|e| format!("cannot find {}: {e}", c.host))?
        .next().ok_or_else(|| format!("cannot find {}", c.host))?;
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("cannot connect to {}:{}: {e}", c.host, c.port))?;
    s.set_write_timeout(Some(Duration::from_secs(5))).ok();
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    Ok(s)
}

/// The status code of an HTTP response read from `s` (None when the server said nothing in time).
fn response_code(s: &mut TcpStream) -> Option<u16> {
    let mut buf = [0u8; 512];
    let n = s.read(&mut buf).ok().filter(|&n| n > 0)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    text.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

/// Opens the source connection: HTTP PUT on the mount with the Java reference's headers.
fn connect(c: &StreamConfig) -> Result<TcpStream, String> {
    let mut s = open(c)?;
    let req = format!(
        "PUT {} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: {}\r\nUser-Agent: SDRTrunk-Rust/{}\r\nAccept: */*\r\nContent-Type: audio/mpeg\r\n\
         ice-public: {}\r\nice-name: {}\r\nice-description: {}\r\nice-audio-info: samplerate={};quality=7;channels=1\r\n\
         ice-bitrate: {}\r\nExpect: 100-continue\r\n\r\n",
        c.mount, c.host, c.port, auth(c), env!("CARGO_PKG_VERSION"), u8::from(c.public), clean(&c.name), clean(&c.description),
        c.sample_rate, c.bitrate_kbps);
    s.write_all(req.as_bytes()).map_err(|e| format!("cannot send to the server: {e}"))?;
    // Icecast answers 100 Continue (or 200); a newer one may answer only once audio arrives, so silence is accepted.
    match response_code(&mut s) {
        None | Some(100) | Some(200) => Ok(s),
        Some(401) => Err("the server rejected the username or password".into()),
        Some(403) => Err("the server refused the mount (in use by another source, or not allowed)".into()),
        Some(code) => Err(format!("the server answered HTTP {code}")),
    }
}

/// Sends a title to `/admin/metadata` (on its own thread: audio never waits for it).
fn send_title(c: &StreamConfig, title: &str, status: &Arc<Mutex<StreamStatus>>) {
    let (c, title, status) = (c.clone(), title.to_string(), status.clone());
    std::thread::spawn(move || {
        let req = format!("GET /admin/metadata?mode=updinfo&mount={}&charset=UTF-8&song={} HTTP/1.0\r\nHost: {}:{}\r\n\
            Authorization: {}\r\nUser-Agent: SDRTrunk-Rust/{}\r\n\r\n", pct(&c.mount), pct(&title), c.host, c.port, auth(&c),
            env!("CARGO_PKG_VERSION"));
        let result = open(&c).and_then(|mut s| {
            s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
            match response_code(&mut s) { Some(200) => Ok(()), code => Err(format!("the server answered {code:?}")) }
        });
        if let Err(e) = result { lock(&status).error = Some(format!("title not updated: {e}")); }
    });
}

/// One second of 8 kHz audio per write.
const SLICE: usize = 8_000;

/// Plays `pcm` on the connection in one-second slices, paced in real time from `next`. Err: the connection broke.
fn play(s: &mut TcpStream, enc: &mut StreamEncoder, pcm: &[i16], next: &mut Instant, stop: &AtomicBool) -> Result<(), String> {
    for slice in pcm.chunks(SLICE) {
        if stop.load(Ordering::Relaxed) { return Ok(()); }
        let bytes = enc.encode(slice)?;
        s.write_all(&bytes).map_err(|e| format!("connection lost: {e}"))?;
        *next += Duration::from_secs_f64(slice.len() as f64 / 8_000.0);
        let now = Instant::now();
        if *next > now { std::thread::sleep(*next - now); } else { *next = now; }
    }
    Ok(())
}

fn run(c: StreamConfig, rx: Receiver<StreamCall>, status: Arc<Mutex<StreamStatus>>, stop: Arc<AtomicBool>, retry: Duration) {
    let mut queue: VecDeque<StreamCall> = VecDeque::new();
    let mut conn: Option<TcpStream> = None;
    let mut last_try: Option<Instant> = None;
    let mut enc = match StreamEncoder::new(c.sample_rate, c.bitrate_kbps) {
        Ok(e) => e,
        Err(e) => { let mut st = lock(&status); st.state = "error".into(); st.error = Some(e); return; }
    };
    let mut next = Instant::now();
    let silence = vec![0i16; SLICE];
    while !stop.load(Ordering::Relaxed) {
        queue.extend(rx.try_iter());
        let now = now_ms();
        queue.retain(|call| now - call.start_ms <= c.max_age_ms as i64);
        lock(&status).queued = queue.len();
        if conn.is_none() && last_try.is_none_or(|t| t.elapsed() >= retry) {
            last_try = Some(Instant::now());
            lock(&status).state = "connecting".into();
            match connect(&c) {
                Ok(s) => { conn = Some(s); next = Instant::now(); let mut st = lock(&status); st.state = "connected".into(); st.error = None; }
                Err(e) => { let mut st = lock(&status); st.state = "error".into(); st.error = Some(e); }
            }
        }
        let Some(s) = conn.as_mut() else { std::thread::sleep(Duration::from_millis(100)); continue };
        let ready = queue.front().is_some_and(|call| now - call.start_ms >= c.delay_ms as i64);
        let result = if ready {
            let call = queue.pop_front().expect("front");
            send_title(&c, &call.title, &status);
            let r = play(s, &mut enc, &call.pcm, &mut next, &stop);
            if r.is_ok() { let mut st = lock(&status); st.sent += 1; st.last_sent_ms = call.start_ms; st.queued = queue.len(); }
            r
        } else {
            play(s, &mut enc, &silence, &mut next, &stop)
        };
        if let Err(e) = result {
            conn = None;
            let mut st = lock(&status);
            st.state = "error".into();
            st.error = Some(e);
        }
    }
}

// ---------- import from an sdrtrunk playlist ----------

/// Icecast HTTP streams in an sdrtrunk playlist (`default.xml`), each with the talkgroups of the aliases that name it
/// as their broadcast channel, under the sdrtrunk alias list each alias is in (`map_lists` renames them to ours).
/// Returns them and how many other streams were skipped.
pub fn parse_playlist(xml: &str) -> Result<(Vec<StreamConfig>, usize), String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("not an sdrtrunk playlist: {e}"))?;
    if !doc.root_element().has_tag_name("playlist") { return Err("not an sdrtrunk playlist (no <playlist>)".into()); }
    let mut channels: HashMap<&str, Vec<StreamTalkgroup>> = HashMap::new();
    for alias in doc.descendants().filter(|n| n.has_tag_name("alias")) {
        let list = alias.attribute("list").unwrap_or_default();
        let ids: Vec<_> = alias.children().filter(|n| n.has_tag_name("id")).collect();
        let groups: Vec<StreamTalkgroup> = ids.iter().filter(|i| i.attribute("type") == Some("talkgroup"))
            .filter_map(|i| i.attribute("value")?.parse().ok()).map(|g| StreamTalkgroup::new(list, g)).collect();
        for ch in ids.iter().filter(|i| i.attribute("type") == Some("broadcastChannel")).filter_map(|i| i.attribute("channel")) {
            channels.entry(ch).or_default().extend(groups.iter().cloned());
        }
    }
    let (mut out, mut skipped) = (Vec::new(), 0);
    for s in doc.descendants().filter(|n| n.has_tag_name("stream")) {
        if s.attribute("type") != Some("icecastHTTPConfiguration") { skipped += 1; continue; }
        let a = |k: &str| s.attribute(k).unwrap_or_default().to_string();
        let num = |k: &str, d: u64| s.attribute(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let name = a("name");
        let mut members = channels.get(name.as_str()).cloned().unwrap_or_default();
        members.sort();
        members.dedup();
        let d = StreamConfig::default();
        out.push(StreamConfig {
            enabled: a("enabled") != "false", description: a("description"), host: a("host"), port: num("port", 8000) as u16,
            mount: a("mount_point"), user: s.attribute("user_name").unwrap_or("source").to_string(), password: a("password"),
            public: a("public") == "true", bitrate_kbps: num("bitrate", 16) as u16, sample_rate: num("sample_rate", 8000) as u32,
            delay_ms: num("delay", 0), max_age_ms: num("maximum_recording_age", d.max_age_ms), members, name, ..d
        });
    }
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn helpers() {
        assert_eq!(base64(b"source:hackme"), "c291cmNlOmhhY2ttZQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(pct("TO:23 WPD/West"), "TO%3A23%20WPD%2FWest");
        assert_eq!(title(23, Some("WPD West"), 703_313), "TO:23 WPD West FROM:703313");
        assert_eq!(title(85, None, 0), "TO:85 FROM:UNKNOWN");
        assert!(StreamConfig { name: "x".into(), host: "h".into(), mount: "/m".into(), members: vec![StreamTalkgroup::new("SGCo", 1)],
            ..StreamConfig::default() }.validate().is_ok());
        let old: StreamConfig = serde_json::from_str(r#"{"name":"x","alias_list":"SGCo","talkgroups":[23,85]}"#).unwrap();
        let new = old.upgraded();
        assert_eq!(new.members, vec![StreamTalkgroup::new("SGCo", 23), StreamTalkgroup::new("SGCo", 85)], "saved before several lists");
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains(r#""members":[{"list":"SGCo","group":23}"#) && !json.contains("alias_list"), "{json}");
        let ours = ["SGCo".to_string(), "KSICS".to_string(), "SG".to_string()];
        assert_eq!(guess_list("SGCo P25", &ours).as_deref(), Some("SGCo"), "longest prefix");
        assert_eq!(guess_list("ksics", &ours).as_deref(), Some("KSICS"));
        assert_eq!(guess_list("MPCo P25", &ours), None);
        let bad = StreamConfig { mount: "m".into(), ..StreamConfig::default() }.validate().unwrap_err();
        assert_eq!(bad.iter().map(|e| e.field).collect::<Vec<_>>(), vec!["name", "host", "mount", "talkgroups"]);
    }

    const PLAYLIST: &str = r#"<playlist version="4">
      <alias name="WPD West" list="SGCo P25" group="Law"><id type="talkgroup" value="23" protocol="APCO25"/>
        <id type="broadcastChannel" channel="WPD West"/><id type="record"/></alias>
      <alias name="Fire" list="SGCo P25"><id type="talkgroup" value="85" protocol="APCO25"/><id type="talkgroup" value="86" protocol="APCO25"/>
        <id type="broadcastChannel" channel="Fire Ops"/></alias>
      <alias name="Other" list="SGCo P25"><id type="talkgroup" value="99" protocol="APCO25"/></alias>
      <alias name="KHP" list="KSICS"><id type="talkgroup" value="85" protocol="APCO25"/><id type="broadcastChannel" channel="Fire Ops"/></alias>
      <stream type="icecastHTTPConfiguration" description="Patrol West" public="false" sample_rate="8000" user_name="source"
        bitrate="16" channels="1" mount_point="/scan21" inline="true" host="192.0.2.10" name="WPD West" enabled="true"
        port="8000" delay="0" password="example-pw" maximum_recording_age="600000"/>
      <stream type="icecastHTTPConfiguration" mount_point="/fire" host="192.0.2.10" name="Fire Ops" enabled="false" port="8001"
        password="p2" maximum_recording_age="300000"/>
      <stream type="broadcastifyCallConfiguration" name="Calls"/>
    </playlist>"#;

    #[test]
    fn playlist_import() {
        let (s, skipped) = parse_playlist(PLAYLIST).unwrap();
        assert_eq!((s.len(), skipped), (2, 1), "Broadcastify Calls is not imported");
        assert_eq!(s[0], StreamConfig { name: "WPD West".into(), description: "Patrol West".into(), host: "192.0.2.10".into(),
            mount: "/scan21".into(), password: "example-pw".into(), members: vec![StreamTalkgroup::new("SGCo P25", 23)], ..StreamConfig::default() });
        let fire = vec![StreamTalkgroup::new("KSICS", 85), StreamTalkgroup::new("SGCo P25", 85), StreamTalkgroup::new("SGCo P25", 86)];
        assert_eq!((s[1].enabled, s[1].port, s[1].members.clone(), s[1].max_age_ms), (false, 8001, fire, 300_000),
            "talkgroups from two alias lists; 85 in each is a different talkgroup");
        let map: HashMap<String, String> = [("SGCo P25".to_string(), "SGCo".to_string()), ("KSICS".into(), String::new())].into();
        let mapped = map_lists(s, &map);
        assert_eq!(mapped[1].members, vec![StreamTalkgroup::new("SGCo", 85), StreamTalkgroup::new("SGCo", 86)], "KSICS left out");
        assert!(parse_playlist("<html/>").is_err());
        assert!(parse_playlist("not xml").is_err());
    }

    /// A fake Icecast server: answers the PUT with `put_code`, records each request's head and counts the audio bytes
    /// after the PUT (closing the connection once `drop_after` bytes arrived, if set), and answers titles with 200.
    struct Fake { port: u16, heads: Arc<Mutex<Vec<String>>>, audio: Arc<Mutex<usize>>, drop_after: Arc<Mutex<Option<usize>>> }

    fn fake(put_code: u16) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (heads, audio, drop_after) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(0)), Arc::new(Mutex::new(None::<usize>)));
        let (h, a, d) = (heads.clone(), audio.clone(), drop_after.clone());
        std::thread::spawn(move || for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let (h, a, d) = (h.clone(), a.clone(), d.clone());
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut b = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) { Ok(0) | Err(_) => return, Ok(n) => buf.extend_from_slice(&b[..n]) }
                }
                let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                lock(&h).push(head.clone());
                if !head.starts_with("PUT") { let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n"); return; }
                let _ = s.write_all(format!("HTTP/1.1 {put_code} X\r\n\r\n").as_bytes());
                if put_code != 100 { return; }
                *lock(&a) += buf.len() - end;
                loop {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            *lock(&a) += n;
                            let limit = *lock(&d);
                            if limit.is_some_and(|limit| *lock(&a) >= limit) { *lock(&d) = None; return; }
                        }
                    }
                }
            });
        });
        Fake { port, heads, audio, drop_after }
    }

    fn config(port: u16) -> StreamConfig {
        StreamConfig { id: 1, name: "WPD West".into(), description: "Patrol West".into(), host: "127.0.0.1".into(), port,
            mount: "/scan21".into(), password: "hackme".into(), members: vec![StreamTalkgroup::new("SGCo", 23)], ..StreamConfig::default() }
    }

    fn wait(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit { if f() { return true; } std::thread::sleep(Duration::from_millis(20)); }
        false
    }

    fn call(group: u16, start_ms: i64) -> StreamCall {
        StreamCall { alias_list: "SGCo".into(), group, start_ms, title: title(group, Some("WPD West"), 703_313), pcm: vec![1000; 4_000] }
    }

    #[test]
    fn streams_calls_titles_and_silence() {
        let f = fake(100);
        let streams = Streams::new(Duration::from_millis(200));
        streams.apply(vec![config(f.port), StreamConfig { id: 2, enabled: false, ..config(f.port) }]);
        assert!(wait(Duration::from_secs(3), || streams.status(1).state == "connected"));
        assert_eq!(streams.status(2).state, "off", "a disabled stream does not run");
        let put = lock(&f.heads)[0].clone();
        for h in ["PUT /scan21 HTTP/1.1", "Authorization: Basic c291cmNlOmhhY2ttZQ==", "Content-Type: audio/mpeg", "ice-name: WPD West",
                  "ice-description: Patrol West", "ice-public: 0", "ice-bitrate: 16", "Expect: 100-continue"] {
            assert!(put.contains(h), "{h} in {put}");
        }
        assert!(wait(Duration::from_secs(3), || *lock(&f.audio) > 1_000), "silence keeps flowing while idle");

        streams.submit(&call(99, now_ms()));
        streams.submit(&StreamCall { alias_list: "KSICS".into(), ..call(23, now_ms()) });
        streams.submit(&call(23, now_ms() - 700_000));
        streams.submit(&call(23, now_ms()));
        assert!(wait(Duration::from_secs(4), || streams.status(1).sent == 1));
        assert!(wait(Duration::from_secs(2), || lock(&f.heads).iter().any(|h|
            h.starts_with("GET /admin/metadata?mode=updinfo&mount=%2Fscan21&charset=UTF-8&song=TO%3A23%20WPD%20West%20FROM%3A703313 "))),
            "{:?}", lock(&f.heads));
        std::thread::sleep(Duration::from_millis(300));
        let st = streams.status(1);
        assert_eq!((st.sent, st.queued, st.error), (1, 0, None), "other talkgroups, other lists and stale calls are not played");
    }

    #[test]
    fn errors_and_reconnect() {
        let f = fake(401);
        let streams = Streams::new(Duration::from_millis(200));
        streams.apply(vec![config(f.port)]);
        assert!(wait(Duration::from_secs(3), || streams.status(1).error.as_deref() == Some("the server rejected the username or password")));
        streams.submit(&call(23, now_ms()));
        assert!(wait(Duration::from_secs(2), || streams.status(1).queued == 1), "calls wait while it cannot connect");

        let f = fake(100);
        *lock(&f.drop_after) = Some(500);
        streams.apply(vec![config(f.port)]);
        assert!(wait(Duration::from_secs(5), || lock(&f.heads).iter().filter(|h| h.starts_with("PUT")).count() >= 2),
            "dropped by the server, then connected again");
        streams.stop_all();
        assert_eq!(streams.status(1).state, "off");
    }
}
