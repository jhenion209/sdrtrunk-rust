mod common;
use axum::http::StatusCode;
use common::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sdrtrunk_web::app::AppState;
use sdrtrunk_web::auth::Role;
use sdrtrunk_web::db::{System, Talkgroup, TunerMode};
use sdrtrunk_web::receiver::Sdrs;

const A: &str = "735863DC-2F67AFE3";
const B: &str = "10A862DC-34348363";

/// Two fake Airspys; opening counts and then fails (no decoding needed here). Records the serial each open asked for.
fn counting() -> (AppState, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let (opens, serials) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(Vec::new())));
    let (o, s) = (opens.clone(), serials.clone());
    let st = state_with(Sdrs {
        list: Arc::new(|| vec![A.into(), B.into()]),
        open: Arc::new(move |_, _, serial| { o.fetch_add(1, Ordering::SeqCst); s.lock().unwrap().push(serial.to_string()); Err("test".into()) }),
    });
    (st, opens, serials)
}

async fn wait_opens(opens: &AtomicUsize, n: usize) -> bool {
    // Up to 5 s: decoder threads start slower while the whole suite runs at once.
    for _ in 0..500 { if opens.load(Ordering::SeqCst) >= n { return true; } tokio::time::sleep(Duration::from_millis(10)).await; }
    false
}

fn form<'a>(csrf: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut f = vec![("csrf", csrf), ("enabled", "on"), ("system_name", "KSICS"), ("site_name", "KSICS-Sedgwick"),
        ("channel_name", "T-KSICS-Wichita"), ("primary", "852.1125"), ("backups", "852.4125"), ("failover_ms", "1000"),
        ("modulation", "c4fm"), ("tuner", "")];
    for (k, v) in extra { f.retain(|(n, _)| n != k); f.push((k, v)); }
    f
}

#[tokio::test]
async fn add_edit_delete_system() {
    let (st, opens, serials) = counting();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    assert!(wait_opens(&opens, 1).await, "system 1 started with the state");
    let page = send(&st, "GET", "/systems/new", c, &[]).await.body;
    assert!(page.contains(A) && page.contains(B) && page.contains("Automatic"), "tuner picker lists connected Airspys");

    let r = send(&st, "POST", "/systems/new", c, &form(&csrf, &[("tuner", "0x10a862dc34348363")])).await;
    assert_eq!((r.status, r.location.as_deref()), (StatusCode::SEE_OTHER, Some("/systems?saved=1")));
    let k = st.db.systems().into_iter().find(|s| s.system_name == "KSICS").unwrap();
    assert_eq!((k.control_hz.clone(), k.modulation.as_str(), k.tuner.as_str()), (vec![852_112_500, 852_412_500], "c4fm", B),
        "serial stored in sdrtrunk's form");
    assert!(wait_opens(&opens, 2).await, "the new system started");
    assert!(serials.lock().unwrap().contains(&B.to_string()), "pinned tuner opened");
    assert_eq!(st.db.audit_log(1)[0].action, "added system KSICS / T-KSICS-Wichita");
    let list = send(&st, "GET", "/systems", c, &[]).await.body;
    assert!(list.contains("KSICS / T-KSICS-Wichita") && list.contains("P25 / Control") && list.contains("C4FM"), "{list}");

    // Unticking "Receive this system" sends no `enabled` field: the system is kept but stopped.
    let off: Vec<_> = form(&csrf, &[]).into_iter().filter(|(n, _)| *n != "enabled").collect();
    let r = send(&st, "POST", &format!("/systems/{}", k.id), c, &off).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(!st.db.system(k.id).unwrap().enabled);
    assert!(st.receivers.get(k.id).is_none(), "disabled system stopped");

    let r = send(&st, "POST", &format!("/systems/{}/delete", k.id), c, &[("csrf", &csrf)]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(st.db.system(k.id).is_none());
    assert_eq!(st.db.audit_log(1)[0].action, "deleted system KSICS / T-KSICS-Wichita");
    assert_eq!(send(&st, "GET", "/systems/999", c, &[]).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_input_not_saved() {
    let (st, _, _) = counting();
    let (cookie, csrf) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    for (extra, message) in [
        (("primary", "abc"), "MHz, for example 858.9625"),
        (("primary", "5"), "between 24 and 1800 MHz"),
        (("failover_ms", "x"), "Use 200 to 60000 ms."),
        (("modulation", "qam"), "Choose C4FM or LSM"),
        (("tuner", "not-a-serial"), "Choose a connected SDR"),
        (("site_name", " "), "names must be 1 to 60 characters"),
    ] {
        let r = send(&st, "POST", "/systems/new", c, &form(&csrf, &[extra])).await;
        assert_eq!(r.status, StatusCode::OK, "{extra:?}");
        assert!(r.body.contains(message), "{extra:?}: {}", r.body);
    }
    assert_eq!(st.db.systems().len(), 1, "nothing added");
    assert_eq!(send(&st, "POST", "/systems/new", c, &form("forged", &[])).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&st, "POST", "/systems/1/delete", c, &[("csrf", "forged")]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(st.db.systems().len(), 1);
}

#[tokio::test]
async fn recordings_folder_restarts_every_system() {
    let (st, opens, _) = counting();
    st.db.save_system(&System { system_name: "KSICS".into(), ..System::default() }).unwrap();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    assert!(wait_opens(&opens, 1).await);
    let before = opens.load(Ordering::SeqCst);
    let r = send(&st, "POST", "/recordings", Some(&cookie), &[("csrf", &csrf), ("recording_dir", "/srv/recordings")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(st.db.settings().recording_dir, "/srv/recordings");
    assert!(wait_opens(&opens, before + 2).await, "both systems restarted");
    assert_eq!(st.db.audit_log(1)[0].action, "changed recordings directory");
    let r = send(&st, "POST", "/recordings", Some(&cookie), &[("csrf", &csrf), ("recording_dir", "relative")]).await;
    assert!(r.body.contains("full path"));
    assert_eq!(st.db.settings().recording_dir, "/srv/recordings");
}

#[tokio::test]
async fn talkgroup_lists_are_per_system() {
    let (st, _, _) = counting();
    st.db.save_system(&System { system_name: "KSICS".into(), ..System::default() }).unwrap();
    st.db.upsert_talkgroup("P25", &Talkgroup { number: 314, name: "SG Fire".into(), record: true }).unwrap();
    st.db.upsert_talkgroup("KSICS", &Talkgroup { number: 314, name: "KHP Troop F-1".into(), record: true }).unwrap();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    let p25 = send(&st, "GET", "/talkgroups", c, &[]).await.body;
    assert!(p25.contains("SG Fire") && !p25.contains("KHP Troop F-1"), "first system by default");
    assert!(p25.contains("href=\"/talkgroups?list=KSICS\""), "system picker");
    let ksics = send(&st, "GET", "/talkgroups?list=KSICS", c, &[]).await.body;
    assert!(ksics.contains("KHP Troop F-1") && !ksics.contains("SG Fire"));
    let r = send(&st, "POST", "/talkgroups/314/delete", c, &[("csrf", &csrf), ("list", "KSICS")]).await;
    assert_eq!(r.location.as_deref(), Some("/talkgroups?list=KSICS"));
    assert_eq!((st.db.talkgroups("KSICS").len(), st.db.talkgroups("P25").len()), (0, 1), "only that system's entry deleted");
    let r = send(&st, "POST", "/talkgroups", c, &[("csrf", &csrf), ("list", "Nope"), ("number", "5"), ("name", "X")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "unknown system refused");
}

#[tokio::test]
async fn no_systems_yet() {
    let (st, _, _) = counting();
    st.db.delete_system(1).unwrap();
    st.receivers.apply(&st.db, 1);
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    assert!(send(&st, "GET", "/", c, &[]).await.body.contains("Add a system"));
    let tg = send(&st, "GET", "/talkgroups", c, &[]).await.body;
    assert!(tg.contains("Alias list: <strong>P25</strong>") && tg.contains("No system uses this list yet"), "the list outlives its system");
    assert!(send(&st, "GET", "/systems", c, &[]).await.body.contains("No systems yet"));
}

#[tokio::test]
async fn tuners_page_settings() {
    let (st, opens, _) = counting();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    assert!(wait_opens(&opens, 1).await);
    let list = send(&st, "GET", "/tuners", c, &[]).await.body;
    assert!(list.contains(&format!("Airspy {A}")) && list.contains(&format!("Airspy {B}")) && list.contains("Live"), "{list}");
    assert!(send(&st, "GET", &format!("/tuners/{A}"), c, &[]).await.body.contains("Sample rate"), "Airspy form");

    let before = opens.load(Ordering::SeqCst);
    let r = send(&st, "POST", &format!("/tuners/{A}"), c, &[("csrf", &csrf), ("mode", "live"), ("ppm", "-3.8"), ("auto_ppm", "on"),
        ("gain", "12"), ("rate_hz", "3000000")]).await;
    assert_eq!((r.status, r.location.as_deref()), (StatusCode::SEE_OTHER, Some("/tuners?saved=1")));
    assert_eq!(st.db.tuner(A), sdrtrunk_web::db::TunerConfig { ppm: -3.8, gain: 12, rate_hz: 3_000_000, ..Default::default() });
    assert!(send(&st, "GET", "/tuners", c, &[]).await.body.contains("(automatic)"));
    assert_eq!(st.db.audit_log(1)[0].action, format!("changed tuner Airspy {A}"));
    assert!(wait_opens(&opens, before + 1).await, "the system that failed on it is retried");

    let rtl = "rtl:00000001";
    let page = send(&st, "GET", &format!("/tuners/{rtl}"), c, &[]).await.body;
    assert!(page.contains("blank for automatic") && !page.contains("Sample rate"), "RTL-SDR form");
    send(&st, "POST", &format!("/tuners/{rtl}"), c, &[("csrf", &csrf), ("mode", "disabled"), ("ppm", "52"), ("rtl_gain_db", "32.8")]).await;
    let t = st.db.tuner(rtl);
    assert_eq!((t.mode, t.ppm, t.auto_ppm, t.rtl_gain_db), (TunerMode::Disabled, 52.0, false, Some(32.8)), "an unticked box turns Automatic PPM off");
    let r = send(&st, "POST", &format!("/tuners/{rtl}"), c, &[("csrf", &csrf), ("mode", "setup"), ("ppm", "0"), ("center_mhz", "")]).await;
    assert!(r.body.contains("Setup mode needs a center frequency"), "Setup mode sits at a fixed center");
    send(&st, "POST", &format!("/tuners/{rtl}"), c, &[("csrf", &csrf), ("mode", "setup"), ("ppm", "0"), ("center_mhz", "853"),
        ("min_mhz", "851.5"), ("max_mhz", "855")]).await;
    let t = st.db.tuner(rtl);
    assert_eq!((t.mode, t.center_hz, t.min_hz, t.max_hz), (TunerMode::Setup, Some(853_000_000), Some(851_500_000), Some(855_000_000)));
    let page = send(&st, "GET", &format!("/tuners/{rtl}"), c, &[]).await.body;
    assert!(page.contains(r#"value="853""#) && page.contains(r#"value="851.5""#), "lock center and limits shown as saved");
    let r = send(&st, "POST", &format!("/tuners/{rtl}"), c, &[("csrf", &csrf), ("mode", "live"), ("ppm", "0"), ("center_mhz", "abc"),
        ("min_mhz", "860"), ("max_mhz", "850")]).await;
    assert!(r.body.contains("Enter MHz such as 853.0") && r.body.contains("The minimum must be below the maximum."));
    assert!(send(&st, "GET", "/tuners", c, &[]).await.body.contains("not connected"), "configured but unplugged tuners stay listed");

    let r = send(&st, "POST", &format!("/tuners/{A}"), c, &[("csrf", &csrf), ("mode", "live"), ("ppm", "x"), ("gain", "30"), ("rate_hz", "6000000")]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Enter a number such as -4") && r.body.contains("Gain must be 0 to 21."));
    assert_eq!(st.db.tuner(A).gain, 12, "nothing saved");
    assert_eq!(send(&st, "GET", "/tuners/nonsense", c, &[]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&st, "POST", &format!("/tuners/{A}"), c, &[("csrf", "forged"), ("ppm", "1")]).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn system_uses_chosen_alias_list() {
    let (st, _, _) = counting();
    st.db.create_alias_list("KSICS Statewide").unwrap();
    st.db.upsert_talkgroup("KSICS Statewide", &Talkgroup { number: 314, name: "KHP Troop F-1".into(), record: false }).unwrap();
    let (cookie, csrf) = login_as(&st, "admin", Role::Admin);
    let c = Some(cookie.as_str());
    assert!(send(&st, "GET", "/systems/new", c, &[]).await.body.contains("<option value=\"KSICS Statewide\">"), "lists offered");
    send(&st, "POST", "/systems/new", c, &form(&csrf, &[("alias_list", "KSICS Statewide")])).await;
    let k = st.db.systems().into_iter().find(|s| s.system_name == "KSICS").unwrap();
    assert_eq!(k.list_name(), "KSICS Statewide");
    let p = st.receivers.get(k.id).unwrap().policy.lock().unwrap().talkgroups.clone();
    assert_eq!(p, st.db.talkgroups("KSICS Statewide"), "the recorder names and records from the chosen list");
    let tg = send(&st, "GET", "/talkgroups?list=KSICS+Statewide", c, &[]).await.body;
    assert!(tg.contains("Used by KSICS / T-KSICS-Wichita"), "{tg}");
}

struct Zeros;
impl sdrtrunk_web::receiver::IqSource for Zeros {
    fn next_block(&mut self) -> Option<Vec<i16>> { std::thread::sleep(Duration::from_millis(5)); Some(vec![0; 2 * 32_768]) }
    fn tune(&mut self, _: u32) -> Result<(), String> { Ok(()) }
    fn dropped(&self) -> u64 { 0 }
    fn rate_hz(&self) -> u32 { 3_000_000 }
}

#[tokio::test]
async fn spectrum_streams_frames() {
    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let st = state_with(Sdrs { list: Arc::new(|| vec![A.into()]), open: Arc::new(|_, _, _| Ok(Box::new(Zeros) as Box<dyn sdrtrunk_web::receiver::IqSource>)) });
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    for _ in 0..100 { if st.receivers.on_tuner(A).is_some() { break; } tokio::time::sleep(Duration::from_millis(10)).await; }
    let page = send(&st, "GET", &format!("/tuners/{A}/spectrum"), c, &[]).await.body;
    assert!(page.contains(&format!("data-src=\"/tuners/{A}/spectrum/events\"")) && page.contains("Used by P25 / Control"), "{page}");
    assert!(send(&st, "GET", "/tuners", c, &[]).await.body.contains(&format!("/tuners/{A}/spectrum")), "link from the list");

    let req = Request::get(format!("/tuners/{A}/spectrum/events")).header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
    let mut body = sdrtrunk_web::app::router(st.clone()).oneshot(req).await.unwrap().into_body();
    let mut seen = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(f)) = body.frame().await {
            if let Some(d) = f.data_ref() { seen.push_str(&String::from_utf8_lossy(d)); }
            if seen.contains("event: frame") && seen.contains("\"markers\"") { break; }
        }
    }).await;
    assert!(seen.contains("event: frame") && seen.contains("\"center_hz\":858912500") && seen.contains("\"kind\":\"control\""), "{seen}");

    let idle = send(&st, "GET", "/tuners/rtl:00000009/spectrum", c, &[]).await.body;
    assert!(idle.contains("Not in use"));
    assert_eq!(send(&st, "GET", "/tuners/bogus/spectrum", c, &[]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&st, "GET", &format!("/tuners/{A}/spectrum/events"), None, &[]).await.status, StatusCode::SEE_OTHER, "login required");
}

#[tokio::test]
async fn signal_page_history_and_symbols() {
    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use sdrtrunk_web::quality::Summary;
    let st = state_with(Sdrs { list: Arc::new(|| vec![A.into()]), open: Arc::new(|_, _, _| Ok(Box::new(Zeros) as Box<dyn sdrtrunk_web::receiver::IqSource>)) });
    let (cookie, _) = login_as(&st, "owner", Role::Owner);
    let c = Some(cookie.as_str());
    for _ in 0..100 { if st.receivers.status(1).is_some_and(|s| s.state == "running") { break; } tokio::time::sleep(Duration::from_millis(10)).await; }

    let status = send(&st, "GET", "/", c, &[]).await.body;
    assert!(status.contains("decode quality") && status.contains("href=\"/systems/1/signal\"") && status.contains("class=\"pill state-running\""), "{status}");
    let page = send(&st, "GET", "/systems/1/signal", c, &[]).await.body;
    assert!(page.contains("No samples in the last 1h") && page.contains("data-src=\"/systems/1/symbols/events\""), "{page}");
    assert!(page.contains("not measured") && page.contains("0.000 ppm (automatic)"), "{page}");

    let now = sdrtrunk_web::receiver::now_ms();
    let s = Summary { signal_db: Some(-42.0), signal_min: Some(-45.0), signal_max: Some(-40.0), current_db: Some(-41.0), valid: 90, attempted: 100.0 };
    st.db.add_quality(1, now - 120_000, &s).unwrap();
    st.db.add_quality(1, now - 60_000, &s).unwrap();
    let page = send(&st, "GET", "/systems/1/signal?range=6h", c, &[]).await.body;
    assert!(page.matches("<svg").count() == 2 && page.contains("aria-current=\"page\">6h"), "two charts, 6h chosen");
    let csv = send(&st, "GET", "/systems/1/signal.csv?range=24h", c, &[]).await.body;
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[0].starts_with("time_utc,signal_dbfs") && lines[1].ends_with(",-42.0,-45.0,-40.0,90.0,90,100.0"), "{csv}");

    let req = Request::get("/systems/1/symbols/events").header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
    let mut body = sdrtrunk_web::app::router(st.clone()).oneshot(req).await.unwrap().into_body();
    let mut seen = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(f)) = body.frame().await {
            if let Some(d) = f.data_ref() { seen.push_str(&String::from_utf8_lossy(d)); }
            if seen.contains("event: symbols") && seen.contains("\"soft\":[") { break; }
        }
    }).await;
    assert!(seen.contains("event: symbols"), "{seen}");

    // The control channel's own spectrum: centred on the control channel, at the channel rate.
    assert!(page.contains("data-src=\"/systems/1/channel/events\"") && page.contains("view-waterfall"), "channel panel");
    let req = Request::get("/systems/1/channel/events").header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
    let mut body = sdrtrunk_web::app::router(st.clone()).oneshot(req).await.unwrap().into_body();
    let mut seen = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(f)) = body.frame().await {
            if let Some(d) = f.data_ref() { seen.push_str(&String::from_utf8_lossy(d)); }
            if seen.contains("event: frame") && seen.contains("\"markers\"") { break; }
        }
    }).await;
    assert!(seen.contains("event: frame") && seen.contains("\"center_hz\":858962500") && seen.contains("\"rate_hz\":50000"), "{seen}");
    assert_eq!(send(&st, "GET", "/systems/9/signal", c, &[]).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&st, "GET", "/systems/1/signal.csv", None, &[]).await.status, StatusCode::SEE_OTHER, "login required");
}
