#![allow(unknown_lints)]
#![allow(unused_doc_comment)]
#![cfg_attr(feature="clippy", feature(plugin))]

extern crate amy;
extern crate byteorder;
extern crate rand;
extern crate sha1;
extern crate url;
#[macro_use]
extern crate lazy_static;
extern crate net2;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate bincode;
extern crate toml;
extern crate ctrlc;
#[macro_use]
extern crate error_chain;
extern crate c_ares;
extern crate httparse;
extern crate base64;
extern crate base32;
extern crate shellexpand;
extern crate chrono;
extern crate openssl;

// TODO: Get rid of this
extern crate num;

#[macro_use]
mod log;

mod handle;
mod bencode;
mod torrent;
mod util;
mod socket;
mod disk;
mod tracker;
mod control;
mod listener;
mod rpc;
mod throttle;
mod config;

use std::thread;
use std::sync::{atomic, mpsc};
use std::io;

// We need to do this for the log macros
use log::LogLevel;
use control::acio;

pub const DHT_EXT: (usize, u8) = (7, 1);
pub const EXT_PROTO: (usize, u8) = (5, 0x10);
pub const UT_META_ID: u8 = 9;

/// Throttler max token amount
pub const THROT_TOKS: usize = 2 * 1024 * 1024;

lazy_static! {
    pub static ref SHUTDOWN: atomic::AtomicBool = {
        atomic::AtomicBool::new(false)
    };

    pub static ref CONFIG: config::Config = {
        if let Ok(cfg)  = config::ConfigFile::try_load() {
            info!("Loaded config file");
            config::Config::from_file(cfg)
        } else {
            info!("Using default config");
            Default::default()
        }
    };

    pub static ref PEER_ID: [u8; 20] = {
        use rand::{self, Rng};

        let mut pid = [0u8; 20];
        let prefix = b"-SY0001-";
        for i in 0..prefix.len() {
            pid[i] = prefix[i];
        }

        let mut rng = rand::thread_rng();
        for i in prefix.len()..20 {
            pid[i] = rng.gen::<u8>();
        }
        pid
    };
}

fn init() -> io::Result<Vec<thread::JoinHandle<()>>> {
    let cpoll = amy::Poller::new()?;
    let mut creg = cpoll.get_registrar()?;
    let (dh, dhj) = disk::start(&mut creg)?;
    let (lh, lhj) = listener::Listener::start(&mut creg)?;
    let (rh, rhj) = rpc::RPC::start(&mut creg)?;
    let (th, thj) = tracker::Tracker::start(&mut creg)?;
    let chans = acio::ACChans {
        disk_tx: dh.tx,
        disk_rx: dh.rx,
        rpc_tx: rh.tx,
        rpc_rx: rh.rx,
        trk_tx: th.tx,
        trk_rx: th.rx,
        lst_tx: lh.tx,
        lst_rx: lh.rx,
    };
    let (tx, rx) = mpsc::channel();
    let chj = thread::spawn(move || {
        let throttler = throttle::Throttler::new(None, None, THROT_TOKS, &creg);
        let acio = acio::ACIO::new(cpoll, creg, chans);
        match control::Control::new(acio, throttler) {
            Ok(mut c) => {
                tx.send(Ok(())).unwrap();
                c.run();
            }
            Err(e) => {
                tx.send(Err(e)).unwrap();
            }
        }
    });
    rx.recv().unwrap()?;

    ctrlc::set_handler(|| {
        info!("Trigggering shutdown via signal!");
        SHUTDOWN.store(true, atomic::Ordering::SeqCst);
    }).map_err(|_| util::io_err_val("Signal installation failed!"))?;

    Ok(vec![chj, dhj, lhj, rhj, thj])
}

fn main() {
    if cfg!(debug_assertions) {
        log::log_init(log::LogLevel::Debug);
    } else {
        log::log_init(log::LogLevel::Info);
    }
    info!("Initializing");
    match init() {
        Ok(threads) => {
            info!("Initialized");
            for thread in threads {
                if let Err(_) = thread.join() {
                    error!("Unclean shutdown detected, terminating");
                    return;
                }
            }
            info!("Shutdown complete");
        }
        Err(e) => {
            error!("Couldn't initialize synapse: {}", e);
        }
    }
}
