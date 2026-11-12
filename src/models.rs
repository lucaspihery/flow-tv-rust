//! Catalogue en mémoire et modèles Slint « paresseux ».
//!
//! Slint ne demande (`row_data`) que les lignes visibles d'une `ListView` :
//! même avec des dizaines de milliers de chaînes, seules quelques dizaines de
//! cartes existent réellement à l'écran.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use slint::{Model, ModelNotify, ModelRc, ModelTracker, VecModel};

use crate::logos::{self, LogoListener};
use crate::xtream::{Category, Channel};
use crate::{ChannelItem, ChannelRow};

#[derive(Default)]
pub struct Catalog {
    pub channels: Vec<Channel>,
    pub search_keys: Vec<String>,
    pub categories: Vec<Category>,
    pub favorites: Vec<i64>,
    fav_set: HashSet<i64>,
    pub by_id: HashMap<i64, usize>,
}

impl Catalog {
    pub fn set_data(&mut self, categories: Vec<Category>, channels: Vec<Channel>) {
        self.search_keys = channels.iter().map(|c| fold(&c.name)).collect();
        self.by_id = channels.iter().enumerate().map(|(i, c)| (c.stream_id, i)).collect();
        self.channels = channels;
        self.categories = categories;
    }

    pub fn set_favorites(&mut self, favs: Vec<i64>) {
        self.fav_set = favs.iter().copied().collect();
        self.favorites = favs;
    }

    pub fn clear(&mut self) {
        *self = Catalog::default();
    }

    pub fn is_favorite(&self, stream_id: i64) -> bool {
        self.fav_set.contains(&stream_id)
    }

    /// Ajoute/retire un favori, renvoie le nouvel état.
    pub fn toggle_favorite(&mut self, stream_id: i64) -> bool {
        if self.fav_set.remove(&stream_id) {
            self.favorites.retain(|f| *f != stream_id);
            false
        } else {
            self.fav_set.insert(stream_id);
            self.favorites.push(stream_id);
            true
        }
    }

    pub fn index_of(&self, stream_id: i64) -> Option<usize> {
        self.by_id.get(&stream_id).copied()
    }

    /// Construit l'élément affiché. `pending` reçoit l'URL du logo s'il n'est pas encore chargé.
    pub fn item(&self, idx: usize, pending: Option<&mut Vec<String>>) -> ChannelItem {
        let Some(c) = self.channels.get(idx) else { return ChannelItem::default() };
        let (logo, has_logo) = match logos::get(&c.logo) {
            Some(Some(img)) => (img, true),
            Some(None) => (Default::default(), false),
            None => {
                if let Some(p) = pending {
                    p.push(c.logo.clone());
                }
                (Default::default(), false)
            }
        };
        let name = display_name(&c.name);
        ChannelItem {
            id: c.stream_id as i32,
            initials: initials(&name).into(),
            tint: tint(&c.name),
            name: name.into(),
            number: c.num as i32,
            logo,
            has_logo,
            favorite: self.is_favorite(c.stream_id),
        }
    }
}

/// Minuscules sans accents pour la recherche.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'û' | 'ü' | 'ú' | 'ù' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// Retire les décorations fréquentes des noms IPTV (« FR: », « |FR| », « ### »...).
pub fn display_name(raw: &str) -> String {
    let mut s = raw.trim();
    // Préfixes de type « FR: », « FR - », « |FR| », « [FR] »
    for _ in 0..2 {
        let t = s.trim_start_matches(['|', '[', '(', '#', '★', '*', ' ']);
        let code_len = t.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
        if (2..=4).contains(&code_len) {
            let rest = &t[code_len..];
            let rest_trim = rest.trim_start_matches([' ']);
            if let Some(r) = rest_trim
                .strip_prefix(':')
                .or_else(|| rest_trim.strip_prefix('|'))
                .or_else(|| rest_trim.strip_prefix(']'))
                .or_else(|| rest_trim.strip_prefix(')'))
                .or_else(|| rest_trim.strip_prefix(" - "))
                .or_else(|| rest_trim.strip_prefix("- "))
            {
                if !r.trim().is_empty() && t[..code_len].chars().all(|c| c.is_ascii_uppercase()) {
                    s = r.trim_start_matches(['|', ']', ')', ':', ' ', '-']);
                    continue;
                }
            }
        }
        break;
    }
    let s = s.trim_matches(['#', '★', '*', ' ', '|']);
    if s.is_empty() {
        raw.trim().to_string()
    } else {
        s.to_string()
    }
}

fn initials(name: &str) -> String {
    let words: Vec<&str> =
        name.split(|c: char| c.is_whitespace() || c == '-' || c == '_').filter(|w| !w.is_empty()).collect();
    let Some(first) = words.first() else { return "TV".into() };
    let first_clean: String = first.chars().filter(|c| c.is_alphanumeric()).collect();
    if (1..=4).contains(&first_clean.chars().count()) {
        return first_clean.to_uppercase();
    }
    let mut out: String =
        words.iter().take(2).filter_map(|w| w.chars().find(|c| c.is_alphanumeric())).collect();
    if out.is_empty() {
        out = "TV".into();
    }
    out.to_uppercase()
}

fn tint(name: &str) -> i32 {
    let h = name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    (h % 8) as i32
}

// ---------------------------------------------------------------------------
// Grille (lignes de N cartes)

pub struct ChannelGrid {
    catalog: Rc<RefCell<Catalog>>,
    indices: RefCell<Rc<Vec<usize>>>,
    columns: Cell<usize>,
    waiting: RefCell<HashMap<String, Vec<usize>>>,
    notify: ModelNotify,
}

impl ChannelGrid {
    pub fn new(catalog: Rc<RefCell<Catalog>>) -> Rc<Self> {
        let m = Rc::new(Self {
            catalog,
            indices: RefCell::new(Rc::new(Vec::new())),
            columns: Cell::new(5),
            waiting: RefCell::new(HashMap::new()),
            notify: ModelNotify::default(),
        });
        let weak: std::rc::Weak<dyn LogoListener> = Rc::downgrade(&(m.clone() as Rc<dyn LogoListener>));
        logos::add_listener(weak);
        m
    }

    pub fn set_indices(&self, idx: Rc<Vec<usize>>) {
        *self.indices.borrow_mut() = idx;
        self.waiting.borrow_mut().clear();
        self.notify.reset();
    }

    pub fn set_columns(&self, cols: usize) {
        let cols = cols.max(1);
        if cols != self.columns.get() {
            self.columns.set(cols);
            self.waiting.borrow_mut().clear();
            self.notify.reset();
        }
    }

    /// Rafraîchit la carte d'une chaîne (ex. favori modifié).
    pub fn refresh_channel(&self, channel_idx: usize) {
        let pos = self.indices.borrow().iter().position(|i| *i == channel_idx);
        if let Some(p) = pos {
            self.notify.row_changed(p / self.columns.get());
        }
    }
}

impl Model for ChannelGrid {
    type Data = ChannelRow;

    fn row_count(&self) -> usize {
        let n = self.indices.borrow().len();
        n.div_ceil(self.columns.get())
    }

    fn row_data(&self, row: usize) -> Option<ChannelRow> {
        let cols = self.columns.get();
        let indices = self.indices.borrow().clone();
        let start = row * cols;
        if start >= indices.len() {
            return None;
        }
        let end = (start + cols).min(indices.len());
        let catalog = self.catalog.borrow();
        let mut pending = Vec::new();
        let items: Vec<ChannelItem> =
            indices[start..end].iter().map(|i| catalog.item(*i, Some(&mut pending))).collect();
        drop(catalog);
        if !pending.is_empty() {
            let mut w = self.waiting.borrow_mut();
            for url in pending {
                let rows = w.entry(url).or_default();
                if !rows.contains(&row) {
                    rows.push(row);
                }
            }
        }
        Some(ChannelRow { items: ModelRc::new(VecModel::from(items)) })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

impl LogoListener for ChannelGrid {
    fn logo_ready(&self, url: &str) {
        let rows = self.waiting.borrow_mut().remove(url);
        if let Some(rows) = rows {
            let count = self.row_count();
            for r in rows {
                if r < count {
                    self.notify.row_changed(r);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Liste simple (panneau de zapping du lecteur)

pub struct ChannelList {
    catalog: Rc<RefCell<Catalog>>,
    indices: RefCell<Rc<Vec<usize>>>,
    waiting: RefCell<HashMap<String, Vec<usize>>>,
    notify: ModelNotify,
}

impl ChannelList {
    pub fn new(catalog: Rc<RefCell<Catalog>>) -> Rc<Self> {
        let m = Rc::new(Self {
            catalog,
            indices: RefCell::new(Rc::new(Vec::new())),
            waiting: RefCell::new(HashMap::new()),
            notify: ModelNotify::default(),
        });
        let weak: std::rc::Weak<dyn LogoListener> = Rc::downgrade(&(m.clone() as Rc<dyn LogoListener>));
        logos::add_listener(weak);
        m
    }

    pub fn set_indices(&self, idx: Rc<Vec<usize>>) {
        *self.indices.borrow_mut() = idx;
        self.waiting.borrow_mut().clear();
        self.notify.reset();
    }

    pub fn indices(&self) -> Rc<Vec<usize>> {
        self.indices.borrow().clone()
    }

    pub fn refresh_channel(&self, channel_idx: usize) {
        let pos = self.indices.borrow().iter().position(|i| *i == channel_idx);
        if let Some(p) = pos {
            self.notify.row_changed(p);
        }
    }
}

impl Model for ChannelList {
    type Data = ChannelItem;

    fn row_count(&self) -> usize {
        self.indices.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<ChannelItem> {
        let idx = *self.indices.borrow().get(row)?;
        let mut pending = Vec::new();
        let item = self.catalog.borrow().item(idx, Some(&mut pending));
        for url in pending {
            self.waiting.borrow_mut().entry(url).or_default().push(row);
        }
        Some(item)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

impl LogoListener for ChannelList {
    fn logo_ready(&self, url: &str) {
        let rows = self.waiting.borrow_mut().remove(url);
        if let Some(rows) = rows {
            let count = self.row_count();
            for r in rows {
                if r < count {
                    self.notify.row_changed(r);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_iptv_names() {
        assert_eq!(display_name("FR: TF1 HD"), "TF1 HD");
        assert_eq!(display_name("|FR| M6"), "M6");
        assert_eq!(display_name("[UK] BBC One"), "BBC One");
        assert_eq!(display_name("FR - France 2"), "France 2");
        assert_eq!(display_name("##### FRANCE #####"), "FRANCE");
        assert_eq!(display_name("TF1"), "TF1");
        assert_eq!(display_name("Canal+ Sport"), "Canal+ Sport");
    }

    #[test]
    fn makes_initials() {
        assert_eq!(initials("TF1 HD"), "TF1");
        assert_eq!(initials("France 2"), "F2");
        assert_eq!(initials("Eurosport"), "E");
    }

    #[test]
    fn folds_accents() {
        assert_eq!(fold("Télé Évasion"), "tele evasion");
    }
}
