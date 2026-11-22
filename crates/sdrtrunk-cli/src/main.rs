//! sdrtrunk-rust monitor: follow a P25 Phase 1 control channel and print voice grants.
//!
//!   sdrtrunk-rust --freq 858962500 [--ppm -4] [--gain 14] [--rate 6000000]          live Airspy (Linux)
//!   sdrtrunk-rust --freq 858962500 --file cap.bin --center 858915000 [--ppm -4]     int16 IQ capture (airspy_rx -t 2)
//!
//! --ppm is the tuner error (Airspy on the Pi: about -4, i.e. signals show up ~3.5 kHz high at 859 MHz).
//! --modulation lsm for simulcast sites (sdrtrunk "CQPSK"); default c4fm.
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use sdrtrunk_p25::control::{center_for, ControlChannel, ControlEvent, Modulation};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn num<T: std::str::FromStr>(name: &str, default: Option<T>) -> T {
    match arg(name) {
        Some(v) => v.parse().unwrap_or_else(|_| die(&format!("bad value for {name}: {v}"))),
        None => default.unwrap_or_else(|| die(&format!("missing {name}"))),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}\nusage: sdrtrunk-rust serve [--listen 0.0.0.0:8080] [--db PATH] [--secure-cookies] [--replay DIR]\n       sdrtrunk-rust record --db PATH --tuner ID --center HZ --seconds N --out DIR
       sdrtrunk-rust reset-password [USERNAME] [--db PATH]\n       sdrtrunk-rust --freq HZ [--ppm F] [--gain 0-21] [--rate HZ] [--modulation c4fm|lsm] [--file IQ.bin --center HZ]");
    std::process::exit(2)
}

/// `sdrtrunk-rust serve [--listen ADDR] [--db PATH] [--secure-cookies] [--replay DIR]`: web UI plus decoder.
/// `--replay DIR` uses the baseband recordings in DIR as tuners instead of the SDRs.
fn serve() -> ! {
    let listen: std::net::SocketAddr = num("--listen", Some("0.0.0.0:8080".parse().unwrap()));
    let db = arg("--db").map(std::path::PathBuf::from).unwrap_or_else(default_db);
    let secure = std::env::args().any(|a| a == "--secure-cookies");
    let sdrs = match arg("--replay") {
        Some(dir) => sdrtrunk_web::receiver::replay_sdrs(std::path::Path::new(&dir)).unwrap_or_else(|e| die(&format!("replay: {e}"))),
        None => sdrtrunk_web::receiver::sdrs(),
    };
    eprintln!("database {}", db.display());
    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| die(&format!("runtime: {e}")));
    match rt.block_on(sdrtrunk_web::app::serve(db, listen, secure, sdrs)) {
        Ok(()) => std::process::exit(0),
        Err(e) => die(&format!("serve: {e}")),
    }
}

/// /var/lib/sdrtrunk-rust/sdrtrunk-rust.db when that folder is writable (systemd StateDirectory), else ./sdrtrunk-rust.db.
fn default_db() -> std::path::PathBuf {
    let dir = std::path::Path::new("/var/lib/sdrtrunk-rust");
    let path = dir.join("sdrtrunk-rust.db");
    let writable = std::fs::create_dir_all(dir).is_ok()
        && std::fs::OpenOptions::new().create(true).append(true).open(&path).is_ok();
    if writable { path } else { "sdrtrunk-rust.db".into() }
}

/// `sdrtrunk-rust record --db PATH --tuner ID --center HZ --seconds N --out DIR`: one SDR's baseband to a WAV that
/// sdrtrunk, sdrtrunk-vce and `serve --replay` can play.
fn record() -> ! {
    let db = arg("--db").map(std::path::PathBuf::from).unwrap_or_else(default_db);
    let tuner = arg("--tuner").unwrap_or_else(|| die("missing --tuner"));
    let (center, seconds): (u32, u32) = (num("--center", None), num("--seconds", None));
    let out = std::path::PathBuf::from(arg("--out").unwrap_or_else(|| die("missing --out")));
    match sdrtrunk_web::receiver::record_tuner(&db, &tuner, center, seconds, &out) {
        Ok((path, written, dropped)) => {
            println!("{}: {written} samples, {dropped} dropped", path.display());
            std::process::exit(if dropped == 0 { 0 } else { 1 })
        }
        Err(e) => die(&format!("record: {e}")),
    }
}

/// `sdrtrunk-rust reset-password [USERNAME] [--db PATH]`: prints a new random password for USERNAME (signing them out
/// everywhere); without USERNAME, lists the accounts. Run it as the service's user so the database is writable.
fn reset_password() -> ! {
    let db_path = arg("--db").map(std::path::PathBuf::from).unwrap_or_else(default_db);
    let db = sdrtrunk_web::db::Db::open(&db_path).unwrap_or_else(|e| die(&format!("{}: {e}", db_path.display())));
    let name = std::env::args().skip(2).take_while(|a| a != "--db").next();
    let Some(name) = name else {
        println!("accounts in {}:", db_path.display());
        for u in db.users() { println!("  {} ({:?})", u.username, u.role); }
        println!("usage: sdrtrunk-rust reset-password USERNAME [--db PATH]");
        std::process::exit(2)
    };
    match db.reset_password(&name) {
        Ok(pw) => {
            println!("New password for {name}: {pw}");
            println!("Sign in with it, then change it on the Account page. {name} was signed out everywhere.");
            std::process::exit(0)
        }
        Err(e) => die(&format!("reset-password {name}: {e:?}")),
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("serve") { serve(); }
    if std::env::args().nth(1).as_deref() == Some("reset-password") { reset_password(); }
    if std::env::args().nth(1).as_deref() == Some("record") { record(); }
    let freq: i64 = num("--freq", None);
    let ppm: f64 = num("--ppm", Some(0.0));
    let rate: f64 = num("--rate", Some(6_000_000.0));
    let file = arg("--file");
    let center: f64 = num("--center", Some(center_for(freq)));
    let modulation = arg("--modulation").map_or(Modulation::C4fm, |m| {
        Modulation::parse(&m).unwrap_or_else(|| die(&format!("bad value for --modulation: {m} (c4fm or lsm)")))
    });
    let start_ms = now_ms();
    let mut cc = ControlChannel::new(center, rate, freq, ppm, start_ms, modulation).unwrap_or_else(|e| die(&e));
    eprintln!("control {:.4} MHz, tuner centre {:.4} MHz", freq as f64 / 1e6, center / 1e6);

    let mut print = |e: ControlEvent| match e {
        ControlEvent::Site { nac, wacn, system } => println!("site: NAC {nac:03X} WACN {wacn:05X} system {system:03X}"),
        ControlEvent::Call(g) => println!("{:8.1}s  TG {:<6} {:>9.4} MHz  src {:<8} {}{}",
            (now_ms() - start_ms) as f64 / 1000.0, g.group, g.hz as f64 / 1e6, g.source,
            if g.encrypted { "ENC " } else { "" }, if g.emergency { "EMERGENCY" } else { "" }),
        ControlEvent::Voice { .. } | ControlEvent::VoiceEnd { .. } => {}
    };
    let t0 = Instant::now();
    let report = |cc: &ControlChannel, dropped: u64| eprintln!("[{:.0}s] valid TSBK {} ({:.1}/s), dropped samples {dropped}",
        t0.elapsed().as_secs_f64(), cc.valid_tsbk(), cc.valid_tsbk() as f64 / t0.elapsed().as_secs_f64().max(1e-3));

    if let Some(path) = file {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        let iq: Vec<i16> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        for block in iq.chunks(2 * 65_536) { cc.process(block, &mut print); }
        report(&cc, 0);
        return;
    }
    live(center, rate, &mut cc, &mut print, &report, t0);
}

fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64 }

#[cfg(target_os = "linux")]
fn live(center: f64, rate: f64, cc: &mut ControlChannel, print: &mut dyn FnMut(ControlEvent),
        report: &dyn Fn(&ControlChannel, u64), t0: Instant) {
    use std::sync::atomic::Ordering;
    let gain: u8 = num("--gain", Some(14));
    let serial = sdrtrunk_source::airspy::list().first().and_then(|s| sdrtrunk_source::parse_serial(s)).unwrap_or_else(|| die("no Airspy found"));
    let radio = sdrtrunk_source::airspy::Airspy::open(serial, center as u32, rate as u32, gain)
        .unwrap_or_else(|e| die(&format!("airspy: {e}")));
    let mut next = 10.0;
    for block in radio.blocks.iter() {
        cc.process(&block, print);
        if t0.elapsed().as_secs_f64() >= next { report(cc, radio.dropped.load(Ordering::Relaxed)); next += 10.0; }
    }
}

#[cfg(not(target_os = "linux"))]
fn live(_: f64, _: f64, _: &mut ControlChannel, _: &mut dyn FnMut(ControlEvent), _: &dyn Fn(&ControlChannel, u64), _: Instant) {
    die("live Airspy input is Linux only; use --file here")
}
