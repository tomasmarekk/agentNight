//! Connectivity check, only running while shutdown is armed.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::Duration;

use crate::policy::Net;

const TIMEOUT: Duration = Duration::from_secs(4);
const EVERY: Duration = Duration::from_secs(20);
const IPS: &[&str] = &["1.1.1.1:443", "8.8.8.8:443", "9.9.9.9:443"];
const HOSTS: &[&str] = &["api.anthropic.com:443", "api.openai.com:443"];

pub struct NetMonitor {
    state: Arc<AtomicU8>,
    active: Arc<AtomicBool>,
    thread: Thread,
}

impl NetMonitor {
    pub fn start() -> Self {
        let state = Arc::new(AtomicU8::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let (s, a) = (state.clone(), active.clone());
        let handle = thread::Builder::new()
            .name("net".into())
            .spawn(move || loop {
                if a.load(Ordering::Relaxed) {
                    let up = online();
                    if a.load(Ordering::Relaxed) {
                        s.store(if up { 1 } else { 2 }, Ordering::Relaxed);
                    }
                    thread::park_timeout(EVERY);
                } else {
                    thread::park();
                }
            })
            .expect("spawn net thread");
        NetMonitor {
            state,
            active,
            thread: handle.thread().clone(),
        }
    }

    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
        self.state.store(0, Ordering::Relaxed);
        self.thread.unpark();
    }

    pub fn status(&self) -> Net {
        match self.state.load(Ordering::Relaxed) {
            1 => Net::Online,
            2 => Net::Offline,
            _ => Net::Unknown,
        }
    }
}

/// True if any well-known endpoint accepts a TCP connection.
fn online() -> bool {
    let reach = |addr: &SocketAddr| TcpStream::connect_timeout(addr, TIMEOUT).is_ok();
    if IPS
        .iter()
        .filter_map(|a| a.parse::<SocketAddr>().ok())
        .any(|a| reach(&a))
    {
        return true;
    }
    HOSTS.iter().any(|h| {
        h.to_socket_addrs()
            .map(|mut addrs| addrs.any(|a| reach(&a)))
            .unwrap_or(false)
    })
}
