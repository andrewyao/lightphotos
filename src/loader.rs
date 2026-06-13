//! Background image decoding. A worker thread decodes off the UI thread so the
//! window never blocks; decoded images are cached by path so revisiting prev/
//! next is instant. We preload neighbors to make arrow-key nav feel immediate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::image_decode::{self, DecodedImage};

type DecodeResult = (PathBuf, Result<DecodedImage, String>);

pub struct Loader {
    req_tx: Sender<PathBuf>,
    res_rx: Receiver<DecodeResult>,
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    /// Insertion order for simple LRU eviction.
    order: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    capacity: usize,
}

impl Loader {
    pub fn new(max_dim: u32) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<DecodeResult>();

        thread::Builder::new()
            .name("decode-worker".into())
            .spawn(move || {
                while let Ok(path) = req_rx.recv() {
                    let result = image_decode::decode(&path, max_dim);
                    // If the UI side is gone, stop.
                    if res_tx.send((path, result)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn decode worker");

        Self {
            req_tx,
            res_rx,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            capacity: 8,
        }
    }

    /// Ask the worker to decode `path` unless it's already cached or in flight.
    pub fn request(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.inflight.contains(&path) {
            return;
        }
        self.inflight.insert(path.clone());
        let _ = self.req_tx.send(path);
    }

    /// Drain finished decodes into the cache. Returns paths that just arrived.
    pub fn poll(&mut self) -> Vec<PathBuf> {
        let mut arrived = vec![];
        while let Ok((path, result)) = self.res_rx.try_recv() {
            self.inflight.remove(&path);
            match result {
                Ok(img) => {
                    self.insert(path.clone(), Arc::new(img));
                    arrived.push(path);
                }
                Err(e) => eprintln!("decode failed for {}: {e}", path.display()),
            }
        }
        arrived
    }

    pub fn get(&self, path: &PathBuf) -> Option<Arc<DecodedImage>> {
        self.cache.get(path).cloned()
    }

    fn insert(&mut self, path: PathBuf, img: Arc<DecodedImage>) {
        if !self.cache.contains_key(&path) {
            self.order.push_back(path.clone());
        }
        self.cache.insert(path, img);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }
}
