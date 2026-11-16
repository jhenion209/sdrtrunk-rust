//! Accounts (one owner, any number of admins), sessions, login throttling.
use crate::db::Db;
use argon2::password_hash::{rand_core::OsRng as HashRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::{rngs::OsRng, Rng, RngCore};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const IDLE_MS: i64 = 12 * 3600 * 1000;
pub const MAX_MS: i64 = 7 * 24 * 3600 * 1000;
pub const MIN_PASSWORD: usize = 12;
const THROTTLE_AFTER: u32 = 5;
const THROTTLE_MS: i64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role { Owner, Admin }

impl Role {
    fn as_str(self) -> &'static str { match self { Role::Owner => "owner", Role::Admin => "admin" } }
    fn parse(s: &str) -> Role { if s == "owner" { Role::Owner } else { Role::Admin } }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User { pub id: i64, pub username: String, pub role: Role }

#[derive(Clone, Debug)]
pub struct Session { pub user: User, pub csrf: String }

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError { WeakPassword, InvalidUsername, DuplicateUser, NotFound, OwnerProtected, BadCredentials, Throttled }

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuthError::WeakPassword => "Passwords must be at least 12 characters.",
            AuthError::InvalidUsername => "Usernames are 1 to 32 letters, digits, dots, dashes or underscores.",
            AuthError::DuplicateUser => "That username is taken.",
            AuthError::NotFound => "No such user.",
            AuthError::OwnerProtected => "The owner account cannot be removed or duplicated.",
            AuthError::BadCredentials => "Wrong username or password.",
            AuthError::Throttled => "Too many failed sign-ins. Wait 30 seconds and try again.",
        })
    }
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    if password.chars().count() < MIN_PASSWORD { return Err(AuthError::WeakPassword); }
    let salt = SaltString::generate(&mut HashRng);
    Ok(Argon2::default().hash_password(password.as_bytes(), &salt).expect("argon2 hash").to_string())
}

fn password_matches(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok()).unwrap_or(false)
}

/// Hash checked for unknown usernames so a miss takes as long as a wrong password.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_password("dummy password for timing").unwrap())
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn token_hash(token: &str) -> String { Sha256::digest(token.as_bytes()).iter().map(|x| format!("{x:02x}")).collect() }

pub fn valid_username(u: &str) -> bool {
    (1..=32).contains(&u.len()) && u.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

impl Db {
    pub fn user_count(&self) -> i64 {
        self.conn().query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn create_user(&self, username: &str, password: &str, role: Role, now: i64) -> Result<User, AuthError> {
        if !valid_username(username) { return Err(AuthError::InvalidUsername); }
        let hash = hash_password(password)?;
        let c = self.conn();
        if role == Role::Owner {
            let owners: i64 = c.query_row("SELECT COUNT(*) FROM users WHERE role = 'owner'", [], |r| r.get(0)).unwrap_or(0);
            if owners > 0 { return Err(AuthError::OwnerProtected); }
        }
        c.execute("INSERT INTO users (username, role, pw_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![username, role.as_str(), hash, now])
            .map_err(|_| AuthError::DuplicateUser)?;
        Ok(User { id: c.last_insert_rowid(), username: username.into(), role })
    }

    /// Owner first, then admins by name.
    pub fn users(&self) -> Vec<User> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, username, role FROM users ORDER BY role = 'admin', username").expect("query");
        st.query_map([], |r| Ok(User { id: r.get(0)?, username: r.get(1)?, role: Role::parse(&r.get::<_, String>(2)?) }))
            .expect("query").filter_map(Result::ok).collect()
    }

    pub fn verify_login(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let row: Option<(i64, String, String)> = self.conn()
            .query_row("SELECT id, role, pw_hash FROM users WHERE username = ?1", [username], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional().ok().flatten();
        match row {
            Some((id, role, hash)) if password_matches(&hash, password) =>
                Ok(User { id, username: username.into(), role: Role::parse(&role) }),
            Some(_) => Err(AuthError::BadCredentials),
            None => { password_matches(dummy_hash(), password); Err(AuthError::BadCredentials) }
        }
    }

    pub fn set_password(&self, user_id: i64, password: &str) -> Result<(), AuthError> {
        let hash = hash_password(password)?;
        let c = self.conn();
        if c.execute("UPDATE users SET pw_hash = ?1 WHERE id = ?2", params![hash, user_id]).unwrap_or(0) == 0 {
            return Err(AuthError::NotFound);
        }
        c.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id]).ok();
        Ok(())
    }

    pub fn remove_user(&self, user_id: i64) -> Result<(), AuthError> {
        let c = self.conn();
        let role: Option<String> = c.query_row("SELECT role FROM users WHERE id = ?1", [user_id], |r| r.get(0)).optional().ok().flatten();
        match role.as_deref() {
            None => Err(AuthError::NotFound),
            Some("owner") => Err(AuthError::OwnerProtected),
            Some(_) => {
                c.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id]).ok();
                c.execute("DELETE FROM users WHERE id = ?1", [user_id]).ok();
                Ok(())
            }
        }
    }

    /// Returns the raw token for the cookie; only its SHA-256 is stored.
    pub fn create_session(&self, user_id: i64, now: i64) -> String {
        let token = random_hex(32);
        self.conn().execute(
            "INSERT INTO sessions (token_hash, user_id, csrf, created_at, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![token_hash(&token), user_id, random_hex(16), now]).expect("insert session");
        token
    }

    pub fn session(&self, token: &str, now: i64) -> Option<Session> {
        let th = token_hash(token);
        let c = self.conn();
        let row: (i64, String, i64, i64, String, String) = c.query_row(
            "SELECT s.user_id, s.csrf, s.created_at, s.last_seen, u.username, u.role
             FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?1",
            [&th], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional().ok().flatten()?;
        let (user_id, csrf, created, last_seen, username, role) = row;
        if now - last_seen > IDLE_MS || now - created > MAX_MS {
            c.execute("DELETE FROM sessions WHERE token_hash = ?1", [&th]).ok();
            return None;
        }
        c.execute("UPDATE sessions SET last_seen = ?1 WHERE token_hash = ?2", params![now, th]).ok();
        Some(Session { user: User { id: user_id, username, role: Role::parse(&role) }, csrf })
    }

    /// True while the session exists and is within its limits. Unlike `session`, not counted as activity.
    pub fn session_valid(&self, token: &str, now: i64) -> bool {
        self.conn().query_row(
            "SELECT created_at, last_seen FROM sessions WHERE token_hash = ?1", [token_hash(token)],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .optional().ok().flatten()
            .is_some_and(|(created, last)| now - last <= IDLE_MS && now - created <= MAX_MS)
    }

    pub fn end_session(&self, token: &str) {
        self.conn().execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash(token)]).ok();
    }
}

/// Failed sign-ins per key ("user:NAME", "ip:ADDR"): count and time of the last failure.
// ponytail: in-memory and never pruned except on success; fine for a home receiver, add pruning if exposed widely.
#[derive(Default)]
pub struct Throttle(Mutex<HashMap<String, (u32, i64)>>);

impl Throttle {
    pub fn check(&self, keys: &[&str], now: i64) -> Result<(), AuthError> {
        let m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let blocked = keys.iter().any(|k| m.get(*k).is_some_and(|&(n, last)| n >= THROTTLE_AFTER && now - last < THROTTLE_MS));
        if blocked { Err(AuthError::Throttled) } else { Ok(()) }
    }
    pub fn failed(&self, keys: &[&str], now: i64) {
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // Entries past the window can no longer block anyone; dropping them bounds memory.
        m.retain(|_, &mut (_, last)| now - last <= THROTTLE_MS);
        for k in keys { let e = m.entry(k.to_string()).or_insert((0, now)); e.0 += 1; e.1 = now; }
    }
    /// Number of keys currently remembered.
    pub fn tracked(&self) -> usize { self.0.lock().unwrap_or_else(|p| p.into_inner()).len() }

    pub fn succeeded(&self, keys: &[&str]) {
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        for k in keys { m.remove(*k); }
    }
}

/// One-time first-run code, e.g. "K7QM-3XRP" (no easily confused characters).
pub fn new_setup_code() -> String {
    const A: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut r = OsRng;
    let mut s: String = (0..8).map(|_| A[r.gen_range(0..A.len())] as char).collect();
    s.insert(4, '-');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const PW: &str = "correct horse battery";
    const H: i64 = 3600 * 1000;

    fn db_with_owner() -> (Db, User) {
        let db = Db::open_in_memory().unwrap();
        let u = db.create_user("owner", PW, Role::Owner, 0).unwrap();
        (db, u)
    }

    #[test]
    fn password_min_length() {
        let db = Db::open_in_memory().unwrap();
        assert!(matches!(db.create_user("a", "12345678901", Role::Owner, 0), Err(AuthError::WeakPassword)));
        assert_eq!(db.user_count(), 0);
    }

    #[test]
    fn login_ok_and_bad() {
        let (db, u) = db_with_owner();
        assert_eq!(db.verify_login("owner", PW).unwrap().id, u.id);
        assert!(matches!(db.verify_login("owner", "wrong password!!"), Err(AuthError::BadCredentials)));
        assert!(matches!(db.verify_login("nobody", PW), Err(AuthError::BadCredentials)));
    }

    #[test]
    fn single_owner() {
        let (db, _) = db_with_owner();
        assert!(matches!(db.create_user("other", PW, Role::Owner, 0), Err(AuthError::OwnerProtected)));
        assert!(matches!(db.create_user("owner", PW, Role::Admin, 0), Err(AuthError::DuplicateUser)));
        assert_eq!(db.create_user("admin", PW, Role::Admin, 0).unwrap().role, Role::Admin);
        assert_eq!(db.users().len(), 2);
    }

    #[test]
    fn owner_cannot_be_removed() {
        let (db, u) = db_with_owner();
        assert!(matches!(db.remove_user(u.id), Err(AuthError::OwnerProtected)));
        let a = db.create_user("admin", PW, Role::Admin, 0).unwrap();
        let t = db.create_session(a.id, 0);
        db.remove_user(a.id).unwrap();
        assert!(db.session(&t, 1).is_none());
        assert!(matches!(db.remove_user(999), Err(AuthError::NotFound)));
    }

    #[test]
    fn session_idle_expiry() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        let s = db.session(&t, IDLE_MS - 1).unwrap();
        assert_eq!(s.user.username, "owner");
        assert!(!s.csrf.is_empty());
        assert!(db.session(&t, 2 * IDLE_MS + 1).is_none());
    }

    #[test]
    fn session_absolute_expiry() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        let mut now = 0;
        while now + H < MAX_MS { now += H; assert!(db.session(&t, now).is_some(), "at {now}"); }
        assert!(db.session(&t, MAX_MS + 1).is_none());
    }

    #[test]
    fn password_change_revokes_sessions() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        assert!(matches!(db.set_password(u.id, "short"), Err(AuthError::WeakPassword)));
        assert!(db.session(&t, 1).is_some());
        db.set_password(u.id, "another long password").unwrap();
        assert!(db.session(&t, 2).is_none());
        assert!(db.verify_login("owner", "another long password").is_ok());
    }

    #[test]
    fn end_session_logs_out() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        db.end_session(&t);
        assert!(db.session(&t, 1).is_none());
    }

    #[test]
    fn token_not_stored_raw() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        assert!(t.len() >= 43, "256-bit token");
        let stored: String = db.conn().query_row("SELECT token_hash FROM sessions", [], |r| r.get(0)).unwrap();
        assert_ne!(stored, t);
        let pw: String = db.conn().query_row("SELECT pw_hash FROM users", [], |r| r.get(0)).unwrap();
        assert!(pw.starts_with("$argon2id$"));
    }

    #[test]
    fn throttle_after_five() {
        let th = Throttle::default();
        let keys = ["user:owner", "ip:10.0.0.5"];
        for i in 0..5 { assert!(th.check(&keys, i).is_ok()); th.failed(&keys, i); }
        assert!(matches!(th.check(&keys, 4 + 29_000), Err(AuthError::Throttled)));
        assert!(matches!(th.check(&["ip:10.0.0.5"], 4 + 29_000), Err(AuthError::Throttled)), "per address too");
        assert!(th.check(&keys, 4 + 31_000).is_ok());
        assert!(th.check(&["user:other"], 4).is_ok());
        th.succeeded(&keys);
        assert!(th.check(&keys, 5).is_ok());
    }

    #[test]
    fn throttle_prunes_stale_entries() {
        let th = Throttle::default();
        th.failed(&["user:a"], 0);
        th.failed(&["user:b"], THROTTLE_MS + 1);
        assert_eq!(th.tracked(), 1, "entries older than the window are dropped");
    }

    #[test]
    fn session_valid_does_not_extend() {
        let (db, u) = db_with_owner();
        let t = db.create_session(u.id, 0);
        assert!(db.session_valid(&t, IDLE_MS - 1));
        assert!(!db.session_valid(&t, IDLE_MS + 1), "checking must not count as activity");
        db.end_session(&t);
        assert!(!db.session_valid(&t, 1));
    }

    #[test]
    fn setup_code_shape() {
        let c = new_setup_code();
        assert_eq!(c.len(), 9);
        assert_ne!(c, new_setup_code());
    }
}
