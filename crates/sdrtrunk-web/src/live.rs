//! How many live pages (Server-Sent Events streams: dashboard, signal, symbols, spectrum, streams, tuners) each
//! session has open, for the Connected hosts page. Kept in memory only; a restart starts from none.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Open live pages per session, keyed by the session's token hash.
#[derive(Default)]
pub struct LivePages {
    open: Mutex<HashMap<String, usize>>,
}

impl LivePages {
    /// Counts one more live page for the session; the count drops when the guard does (the stream ends).
    pub fn open(self: &Arc<Self>, token_hash: String) -> LiveGuard {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()).entry(token_hash.clone()).or_default() += 1;
        LiveGuard { pages: self.clone(), token_hash }
    }

    pub fn count(&self, token_hash: &str) -> usize {
        self.open.lock().unwrap_or_else(|p| p.into_inner()).get(token_hash).copied().unwrap_or(0)
    }
}

/// One open live page; dropped with its stream.
pub struct LiveGuard {
    pages: Arc<LivePages>,
    token_hash: String,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut open = self.pages.open.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = open.get_mut(&self.token_hash) {
            *n -= 1;
            if *n == 0 { open.remove(&self.token_hash); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_raises_and_drop_lowers() {
        let live = Arc::new(LivePages::default());
        assert_eq!(live.count("a"), 0);
        let one = live.open("a".into());
        let two = live.open("a".into());
        let other = live.open("b".into());
        assert_eq!((live.count("a"), live.count("b")), (2, 1));
        drop(one);
        assert_eq!(live.count("a"), 1);
        drop(two);
        drop(other);
        assert_eq!((live.count("a"), live.count("b")), (0, 0));
        assert!(live.open.lock().unwrap().is_empty(), "no entries left for ended sessions");
    }
}
