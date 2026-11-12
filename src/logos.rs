//! Chargement asynchrone des logos de chaînes.
//!
//! - Les requêtes viennent uniquement des lignes *visibles* (le modèle de grille
//!   ne demande un logo qu'au moment où Slint affiche la ligne).
//! - Les plus récentes passent en premier : en défilant vite, on ne télécharge
//!   pas des centaines de logos déjà sortis de l'écran.
//! - Cache disque (octets bruts) + cache mémoire (images décodées, réduites).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Weak;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

const MAX_QUEUE: usize = 240;
const MAX_IMAGES: usize = 2500;
const WORKERS: usize = 6;
const LOGO_MAX_PX: u32 = 320;

pub trait LogoListener {
    fn logo_ready(&self, url: &str);
}

struct Queue {
    items: Mutex<VecDeque<String>>,
    cv: Condvar,
}

pub struct LogoCache {
    images: HashMap<String, Option<Image>>,
    order: VecDeque<String>,
    pending: HashMap<String, Instant>,
    queue: Arc<Queue>,
    listeners: Vec<Weak<dyn LogoListener>>,
}

thread_local! {
    static CACHE: RefCell<Option<LogoCache>> = const { RefCell::new(None) };
}

/// Démarre les threads de téléchargement. À appeler une fois depuis le thread UI.
pub fn init() {
    let queue = Arc::new(Queue { items: Mutex::new(VecDeque::new()), cv: Condvar::new() });
    let dir = crate::db::cache_dir().join("logos");
    let _ = std::fs::create_dir_all(&dir);
    for i in 0..WORKERS {
        let q = queue.clone();
        let dir = dir.clone();
        let _ = std::thread::Builder::new().name(format!("logos-{i}")).spawn(move || worker(q, dir));
    }
    CACHE.with(|c| {
        *c.borrow_mut() = Some(LogoCache {
            images: HashMap::new(),
            order: VecDeque::new(),
            pending: HashMap::new(),
            queue,
            listeners: Vec::new(),
        })
    });
}

pub fn add_listener(l: Weak<dyn LogoListener>) {
    CACHE.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.listeners.push(l);
        }
    });
}

/// Renvoie l'image si elle est en cache, sinon programme son chargement.
/// `Some(None)` = échec définitif (logo invalide) ; `None` = pas encore prêt.
pub fn get(url: &str) -> Option<Option<Image>> {
    if url.is_empty() {
        return Some(None);
    }
    CACHE.with(|c| {
        let mut guard = c.borrow_mut();
        let cache = guard.as_mut()?;
        if let Some(img) = cache.images.get(url) {
            return Some(img.clone());
        }
        let now = Instant::now();
        let stale =
            cache.pending.get(url).map(|t| now.duration_since(*t) > Duration::from_secs(20)).unwrap_or(true);
        if stale {
            cache.pending.insert(url.to_string(), now);
            let mut q = cache.queue.items.lock().unwrap();
            q.retain(|u| u != url);
            q.push_front(url.to_string());
            q.truncate(MAX_QUEUE);
            drop(q);
            cache.queue.cv.notify_one();
        }
        None
    })
}

fn deliver(url: String, buffer: Option<SharedPixelBuffer<Rgba8Pixel>>) {
    let listeners = CACHE.with(|c| {
        let mut guard = c.borrow_mut();
        let cache = guard.as_mut()?;
        cache.pending.remove(&url);
        cache.images.insert(url.clone(), buffer.map(Image::from_rgba8));
        cache.order.push_back(url.clone());
        while cache.order.len() > MAX_IMAGES {
            if let Some(old) = cache.order.pop_front() {
                cache.images.remove(&old);
            }
        }
        cache.listeners.retain(|l| l.strong_count() > 0);
        Some(cache.listeners.clone())
    });
    // Notification hors de l'emprunt du cache (les modèles relisent le cache).
    for l in listeners.unwrap_or_default() {
        if let Some(l) = l.upgrade() {
            l.logo_ready(&url);
        }
    }
}

fn worker(queue: Arc<Queue>, dir: PathBuf) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .user_agent(crate::xtream::USER_AGENT)
        .build()
        .into();
    loop {
        let url = {
            let mut q = queue.items.lock().unwrap();
            loop {
                if let Some(u) = q.pop_front() {
                    break u;
                }
                q = queue.cv.wait(q).unwrap();
            }
        };
        let buffer = load(&agent, &dir, &url);
        let _ = slint::invoke_from_event_loop(move || deliver(url, buffer));
    }
}

fn cache_file(dir: &std::path::Path, url: &str) -> PathBuf {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    dir.join(format!("{:016x}.img", h.finish()))
}

fn load(agent: &ureq::Agent, dir: &std::path::Path, url: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = cache_file(dir, url);
    let bytes = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => b,
        _ => {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return None;
            }
            let mut resp = agent.get(url).call().ok()?;
            let b = resp.body_mut().with_config().limit(4 * 1024 * 1024).read_to_vec().ok()?;
            let _ = std::fs::write(&path, &b);
            b
        }
    };
    let img = image::load_from_memory(&bytes).ok()?;
    let img = if img.width() > LOGO_MAX_PX || img.height() > LOGO_MAX_PX {
        img.thumbnail(LOGO_MAX_PX, LOGO_MAX_PX)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), w, h))
}
