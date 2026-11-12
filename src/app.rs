//! Contrôleur principal : relie l'interface Slint, le stockage, l'API Xtream et mpv.
//!
//! Tout l'état vit sur le thread de l'interface. Les opérations lentes (réseau,
//! lecture/écriture du cache) tournent dans des threads et renvoient leur
//! résultat via [`post`], qui exécute une closure sur le thread de l'interface.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::db::{self, Profile, Store};
use crate::logos::LogoListener;
use crate::models::{fold, Catalog, ChannelGrid, ChannelList};
use crate::player::{Player, PlayerEvent};
use crate::xtream::{self, Category, Channel, Client, EpgProgram};
use crate::{AppState, AppWindow, CategoryItem, EpgEntry, ProfileItem, Screen};

const FAV: &str = "__fav__";
const ALL: &str = "__all__";
const SYNC_MAX_AGE_SECS: i64 = 12 * 3600;
const EPG_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_RETRIES: u32 = 3;

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
}

/// Exécute `f` sur le thread de l'interface avec le contrôleur.
pub fn post<F: FnOnce(&Rc<Controller>) + Send + 'static>(f: F) {
    let _ = slint::invoke_from_event_loop(move || {
        let ctl = CONTROLLER.with(|c| c.borrow().clone());
        if let Some(ctl) = ctl {
            f(&ctl);
        }
    });
}

pub struct Controller {
    app: slint::Weak<AppWindow>,
    store: RefCell<Store>,
    catalog: Rc<RefCell<Catalog>>,
    grid: Rc<ChannelGrid>,
    zap: Rc<ChannelList>,
    profile: RefCell<Option<Profile>>,
    session: Cell<u64>,
    filtered: RefCell<Rc<Vec<usize>>>,
    hero_idx: Cell<Option<usize>>,
    hover_idx: Cell<Option<usize>>,

    pub player: Option<Player>,
    playing_idx: Cell<Option<usize>>,
    retries: Cell<u32>,
    file_loaded: Cell<bool>,
    volume: Cell<f32>,

    epg_cache: RefCell<HashMap<i64, (Instant, Vec<EpgProgram>)>>,
    epg_inflight: RefCell<HashSet<i64>>,

    hover_timer: Timer,
    hide_timer: Timer,
    toast_timer: Timer,
    clock_timer: Timer,
    search_timer: Timer,
    retry_timer: Timer,
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn host_of(server: &str) -> String {
    server
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(server)
        .to_string()
}

fn initial_of(name: &str) -> SharedString {
    name.trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".into())
        .into()
}

fn profile_item(p: &Profile) -> ProfileItem {
    ProfileItem {
        id: p.id as i32,
        name: p.name.clone().into(),
        color: p.color,
        initial: initial_of(&p.name),
        subtitle: host_of(&p.server).into(),
    }
}

fn fmt_time(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%H:%M").to_string()).unwrap_or_default()
}

fn epg_entries(progs: &[EpgProgram]) -> Vec<EpgEntry> {
    let now = now_secs();
    progs
        .iter()
        .filter(|p| p.end > now)
        .take(4)
        .map(|p| EpgEntry {
            title: p.title.clone().into(),
            description: p.description.clone().into(),
            time: format!("{} – {}", fmt_time(p.start), fmt_time(p.end)).into(),
            start: p.start as i32,
            end: p.end as i32,
        })
        .collect()
}

fn quality_label(height: i64) -> &'static str {
    match height {
        h if h >= 2000 => "4K",
        h if h >= 1400 => "QHD",
        h if h >= 1000 => "FHD",
        h if h >= 700 => "HD",
        h if h > 0 => "SD",
        _ => "",
    }
}

impl Controller {
    pub fn new(app: &AppWindow, store: Store) -> Rc<Self> {
        let catalog = Rc::new(RefCell::new(Catalog::default()));
        let grid = ChannelGrid::new(catalog.clone());
        let zap = ChannelList::new(catalog.clone());

        let player = match Player::new(|ev| post(move |c| c.on_player_event(ev))) {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("{e}");
                app.global::<AppState>().set_mpv_available(false);
                app.global::<AppState>().set_mpv_error(e.into());
                None
            }
        };

        let volume = store.setting("volume").and_then(|v| v.parse::<f32>().ok()).unwrap_or(80.0);

        let ctl = Rc::new(Self {
            app: app.as_weak(),
            store: RefCell::new(store),
            catalog,
            grid,
            zap,
            profile: RefCell::new(None),
            session: Cell::new(0),
            filtered: RefCell::new(Rc::new(Vec::new())),
            hero_idx: Cell::new(None),
            hover_idx: Cell::new(None),
            player,
            playing_idx: Cell::new(None),
            retries: Cell::new(0),
            file_loaded: Cell::new(false),
            volume: Cell::new(volume),
            epg_cache: RefCell::new(HashMap::new()),
            epg_inflight: RefCell::new(HashSet::new()),
            hover_timer: Timer::default(),
            hide_timer: Timer::default(),
            toast_timer: Timer::default(),
            clock_timer: Timer::default(),
            search_timer: Timer::default(),
            retry_timer: Timer::default(),
        });
        CONTROLLER.with(|c| *c.borrow_mut() = Some(ctl.clone()));
        let listener: std::rc::Weak<dyn LogoListener> = Rc::downgrade(&(ctl.clone() as Rc<dyn LogoListener>));
        crate::logos::add_listener(listener);

        let s = app.global::<AppState>();
        s.set_channel_rows(ModelRc::from(ctl.grid.clone()));
        s.set_zap_list(ModelRc::from(ctl.zap.clone()));
        s.set_volume(volume);
        s.set_now(now_secs() as i32);
        if let Some(p) = &ctl.player {
            p.set_volume(volume as f64);
        }

        ctl.install_callbacks(app);
        ctl.refresh_profiles();

        let weak = Rc::downgrade(&ctl);
        ctl.clock_timer.start(TimerMode::Repeated, Duration::from_secs(15), move || {
            if let Some(c) = weak.upgrade() {
                c.tick();
            }
        });
        ctl
    }

    fn ui(&self) -> Option<AppWindow> {
        self.app.upgrade()
    }

    fn with_state(&self, f: impl FnOnce(&AppState)) {
        if let Some(app) = self.ui() {
            f(&app.global::<AppState>());
        }
    }

    fn install_callbacks(self: &Rc<Self>, app: &AppWindow) {
        let s = app.global::<AppState>();
        macro_rules! cb {
            ($on:ident, |$c:ident $(, $arg:ident)*| $body:expr) => {{
                let w = Rc::downgrade(self);
                s.$on(move |$($arg),*| {
                    if let Some($c) = w.upgrade() {
                        $body;
                    }
                });
            }};
        }

        cb!(on_select_profile, |c, id| c.select_profile(id as i64));
        cb!(on_new_profile, |c| c.open_editor(None));
        cb!(on_edit_profile, |c, id| c.open_editor(Some(id as i64)));
        cb!(on_save_profile, |c| c.save_profile());
        cb!(on_cancel_edit, |c| c.cancel_edit());
        cb!(on_delete_profile, |c, id| c.delete_profile(id as i64));
        cb!(on_switch_profile, |c| c.switch_profile());
        cb!(on_edit_name_changed, |c, t| c.with_state(|s| s.set_edit_initial(initial_of(&t))));

        cb!(on_select_category, |c, id| c.select_category(&id));
        cb!(on_search_edited, |c, _t| c.schedule_search());
        cb!(on_columns_changed, |c, n| c.grid.set_columns(n.max(1) as usize));
        cb!(on_hover_channel, |c, id| c.hover_channel(id as i64));
        cb!(on_play, |c, id| c.play_stream(id as i64));
        cb!(on_toggle_favorite, |c, id| c.toggle_favorite(id as i64));
        cb!(on_refresh, |c| c.sync());

        cb!(on_back, |c| c.back());
        cb!(on_toggle_pause, |c| c.toggle_pause());
        cb!(on_volume_changed, |c, v| c.set_volume(v));
        cb!(on_toggle_mute, |c| c.toggle_mute());
        cb!(on_next_channel, |c| c.zap_by(1));
        cb!(on_prev_channel, |c| c.zap_by(-1));
        cb!(on_toggle_fullscreen, |c| c.toggle_fullscreen());
        cb!(on_activity, |c| c.activity());
        cb!(on_retry, |c| c.retry());
    }

    fn tick(self: &Rc<Self>) {
        self.with_state(|s| s.set_now(now_secs() as i32));
        // Le programme en cours a pu changer : on recalcule depuis le cache.
        if let Some(idx) = self.hero_idx.get() {
            self.show_epg_for(idx, false);
        }
        if let Some(idx) = self.playing_idx.get() {
            self.show_epg_for(idx, true);
        }
    }

    pub fn toast(self: &Rc<Self>, msg: &str) {
        self.with_state(|s| s.set_toast(msg.into()));
        let w = Rc::downgrade(self);
        self.toast_timer.start(TimerMode::SingleShot, Duration::from_millis(2600), move || {
            if let Some(c) = w.upgrade() {
                c.with_state(|s| s.set_toast("".into()));
            }
        });
    }

    // =====================================================================
    // Profils

    fn refresh_profiles(&self) {
        let profiles = self.store.borrow().profiles().unwrap_or_default();
        let items: Vec<ProfileItem> = profiles.iter().map(profile_item).collect();
        self.with_state(|s| s.set_profiles(ModelRc::new(VecModel::from(items))));
    }

    fn open_editor(&self, id: Option<i64>) {
        let p = id.and_then(|id| self.store.borrow().profile(id).ok().flatten());
        let count = self.store.borrow().profiles().map(|v| v.len()).unwrap_or(0);
        self.with_state(|s| {
            s.set_edit_error("".into());
            s.set_edit_busy(false);
            match &p {
                Some(p) => {
                    s.set_edit_id(p.id as i32);
                    s.set_edit_name(p.name.clone().into());
                    s.set_edit_server(p.server.clone().into());
                    s.set_edit_user(p.username.clone().into());
                    s.set_edit_pass(p.password.clone().into());
                    s.set_edit_color(p.color);
                    s.set_edit_initial(initial_of(&p.name));
                }
                None => {
                    s.set_edit_id(-1);
                    s.set_edit_name("".into());
                    s.set_edit_server("".into());
                    s.set_edit_user("".into());
                    s.set_edit_pass("".into());
                    s.set_edit_color((count % 8) as i32);
                    s.set_edit_initial("?".into());
                }
            }
            s.set_screen(Screen::Editor);
        });
    }

    fn cancel_edit(&self) {
        self.session.set(self.session.get() + 1);
        self.with_state(|s| {
            s.set_edit_busy(false);
            s.set_screen(Screen::Profiles);
        });
    }

    fn save_profile(self: &Rc<Self>) {
        let Some(app) = self.ui() else { return };
        let s = app.global::<AppState>();
        if s.get_edit_busy() {
            return;
        }
        let server = xtream::normalize_server(&s.get_edit_server());
        let username = s.get_edit_user().trim().to_string();
        let password = s.get_edit_pass().trim().to_string();
        let mut name = s.get_edit_name().trim().to_string();
        if s.get_edit_server().trim().is_empty() || username.is_empty() || password.is_empty() {
            s.set_edit_error(
                "Merci de renseigner le serveur, le nom d'utilisateur et le mot de passe.".into(),
            );
            return;
        }
        if name.is_empty() {
            name = username.clone();
        }
        let profile = Profile {
            id: s.get_edit_id().max(0) as i64,
            name,
            server: server.clone(),
            username: username.clone(),
            password: password.clone(),
            color: s.get_edit_color(),
            last_sync: 0,
        };
        s.set_edit_server(server.clone().into());
        s.set_edit_error("".into());
        s.set_edit_busy(true);

        let session = self.session.get();
        std::thread::spawn(move || {
            let client = Client::new(xtream::Credentials { server, username, password });
            let result = client.authenticate();
            post(move |c| {
                if c.session.get() != session {
                    return;
                }
                c.with_state(|s| s.set_edit_busy(false));
                match result {
                    Ok(info) => {
                        log::info!(
                            "compte {} (expiration {:?}, connexions {:?})",
                            info.status,
                            info.exp_date,
                            info.max_connections
                        );
                        let saved = c.store.borrow().save_profile(&profile);
                        match saved {
                            Ok(_) => {
                                c.refresh_profiles();
                                c.with_state(|s| s.set_screen(Screen::Profiles));
                                c.toast("Profil enregistré");
                            }
                            Err(e) => c.with_state(|s| {
                                s.set_edit_error(format!("Erreur d'enregistrement : {e}").into())
                            }),
                        }
                    }
                    Err(e) => c.with_state(|s| s.set_edit_error(e.into())),
                }
            });
        });
    }

    fn delete_profile(self: &Rc<Self>, id: i64) {
        let _ = self.store.borrow().delete_profile(id);
        if self.profile.borrow().as_ref().map(|p| p.id) == Some(id) {
            *self.profile.borrow_mut() = None;
            self.catalog.borrow_mut().clear();
        }
        self.refresh_profiles();
        self.with_state(|s| {
            s.set_screen(Screen::Profiles);
            if s.get_profiles().row_count() == 0 {
                s.set_manage_profiles(false);
            }
        });
        self.toast("Profil supprimé");
    }

    fn switch_profile(&self) {
        self.stop_playback();
        self.session.set(self.session.get() + 1);
        self.refresh_profiles();
        self.with_state(|s| {
            s.set_manage_profiles(false);
            s.set_syncing(false);
            s.set_screen(Screen::Profiles);
        });
    }

    fn select_profile(self: &Rc<Self>, id: i64) {
        let Ok(Some(p)) = self.store.borrow().profile(id) else { return };
        self.session.set(self.session.get() + 1);
        self.store.borrow().set_setting("last_profile", &id.to_string());
        self.catalog.borrow_mut().clear();
        self.epg_cache.borrow_mut().clear();
        self.hero_idx.set(None);
        self.grid.set_indices(Rc::new(Vec::new()));
        self.with_state(|s| {
            s.set_current_profile(profile_item(&p));
            s.set_categories(ModelRc::new(VecModel::from(Vec::<CategoryItem>::new())));
            s.set_selected_category(ALL.into());
            s.set_search("".into());
            s.set_home_error("".into());
            s.set_has_hero(false);
            s.set_visible_count(0);
            s.set_all_count(0);
            s.set_fav_count(0);
            s.set_first_load(true);
            s.set_sync_text("Chargement de vos chaînes...".into());
            s.set_screen(Screen::Home);
        });
        *self.profile.borrow_mut() = Some(p.clone());
        self.load_cache(p);
    }

    // =====================================================================
    // Catalogue

    fn load_cache(self: &Rc<Self>, p: Profile) {
        let session = self.session.get();
        std::thread::spawn(move || {
            let res = Store::open(&db::db_path())
                .and_then(|st| Ok((st.categories(p.id)?, st.channels(p.id)?, st.favorites(p.id)?)));
            post(move |c| {
                if c.session.get() != session {
                    return;
                }
                match res {
                    Ok((cats, chans, favs)) => {
                        let empty = chans.is_empty();
                        c.catalog.borrow_mut().set_favorites(favs);
                        if !empty {
                            c.on_catalog(cats, chans);
                        }
                        let stale = now_secs() - p.last_sync > SYNC_MAX_AGE_SECS;
                        if empty || stale {
                            c.sync();
                        }
                    }
                    Err(e) => {
                        log::error!("lecture du cache : {e}");
                        c.sync();
                    }
                }
            });
        });
    }

    fn sync(self: &Rc<Self>) {
        let Some(p) = self.profile.borrow().clone() else { return };
        let session = self.session.get();
        let first = self.catalog.borrow().channels.is_empty();
        self.with_state(|s| {
            s.set_syncing(true);
            s.set_home_error("".into());
            s.set_first_load(first);
            s.set_sync_text(
                if first { "Téléchargement de la liste des chaînes..." } else { "Synchronisation..." }.into(),
            );
        });
        std::thread::spawn(move || {
            let client = Client::new(p.credentials());
            let result = client.live_categories().and_then(|cats| client.live_streams().map(|ch| (cats, ch)));
            let result = result.and_then(|(cats, chans)| {
                let mut st = Store::open(&db::db_path()).map_err(|e| e.to_string())?;
                st.replace_catalog(p.id, &cats, &chans).map_err(|e| e.to_string())?;
                Ok((cats, chans))
            });
            post(move |c| {
                if c.session.get() != session {
                    return;
                }
                c.with_state(|s| s.set_syncing(false));
                match result {
                    Ok((cats, chans)) => {
                        let n = chans.len();
                        c.on_catalog(cats, chans);
                        if !first {
                            c.toast(&format!("{n} chaînes synchronisées"));
                        }
                    }
                    Err(e) => {
                        log::error!("synchronisation : {e}");
                        c.with_state(|s| {
                            s.set_home_error(e.clone().into());
                            if first {
                                s.set_sync_text(format!("Impossible de récupérer les chaînes. {e}").into());
                            }
                        });
                    }
                }
            });
        });
    }

    fn on_catalog(self: &Rc<Self>, cats: Vec<Category>, chans: Vec<Channel>) {
        let mut counts: HashMap<&str, i32> = HashMap::new();
        for ch in &chans {
            *counts.entry(ch.category_id.as_str()).or_default() += 1;
        }
        let items: Vec<CategoryItem> = cats
            .iter()
            .filter_map(|c| {
                let n = *counts.get(c.id.as_str()).unwrap_or(&0);
                (n > 0).then(|| CategoryItem {
                    id: c.id.clone().into(),
                    name: c.name.clone().into(),
                    count: n,
                })
            })
            .collect();
        let total = chans.len() as i32;
        let valid: HashSet<String> = items.iter().map(|i| i.id.to_string()).collect();

        self.catalog.borrow_mut().set_data(cats, chans);
        let fav_count = self.valid_favorites().len() as i32;

        self.with_state(|s| {
            let sel = s.get_selected_category().to_string();
            if sel != FAV && sel != ALL && !valid.contains(&sel) {
                s.set_selected_category(ALL.into());
            }
            s.set_categories(ModelRc::new(VecModel::from(items)));
            s.set_all_count(total);
            s.set_fav_count(fav_count);
            s.set_first_load(false);
        });
        self.hero_idx.set(None);
        self.apply_filter();
    }

    fn valid_favorites(&self) -> Vec<usize> {
        let cat = self.catalog.borrow();
        cat.favorites.iter().filter_map(|id| cat.index_of(*id)).collect()
    }

    fn select_category(self: &Rc<Self>, id: &str) {
        self.with_state(|s| {
            s.set_selected_category(id.into());
            s.set_search("".into());
        });
        self.hero_idx.set(None);
        self.apply_filter();
    }

    fn schedule_search(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.search_timer.start(TimerMode::SingleShot, Duration::from_millis(140), move || {
            if let Some(c) = w.upgrade() {
                c.apply_filter();
            }
        });
    }

    fn apply_filter(self: &Rc<Self>) {
        let Some(app) = self.ui() else { return };
        let s = app.global::<AppState>();
        let sel = s.get_selected_category().to_string();
        let query = fold(s.get_search().trim());

        let (indices, title): (Vec<usize>, String) = {
            let cat = self.catalog.borrow();
            if !query.is_empty() {
                let terms: Vec<&str> = query.split_whitespace().collect();
                let idx = (0..cat.channels.len())
                    .filter(|i| terms.iter().all(|t| cat.search_keys[*i].contains(t)))
                    .collect();
                (idx, format!("Résultats pour « {} »", s.get_search().trim()))
            } else if sel == FAV {
                drop(cat);
                (self.valid_favorites(), "Mes favoris".into())
            } else if sel == ALL {
                ((0..cat.channels.len()).collect(), "Toutes les chaînes".into())
            } else {
                let name =
                    cat.categories.iter().find(|c| c.id == sel).map(|c| c.name.clone()).unwrap_or_default();
                let idx = cat
                    .channels
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.category_id == sel)
                    .map(|(i, _)| i)
                    .collect();
                (idx, name)
            }
        };

        let indices = Rc::new(indices);
        s.set_visible_count(indices.len() as i32);
        s.set_section_title(title.into());
        *self.filtered.borrow_mut() = indices.clone();
        self.grid.set_indices(indices.clone());

        let keep = self.hero_idx.get().filter(|h| indices.contains(h));
        match keep.or_else(|| indices.first().copied()) {
            Some(idx) => self.set_hero(idx),
            None => {
                self.hero_idx.set(None);
                s.set_has_hero(false);
            }
        }
    }

    // =====================================================================
    // Hero + EPG

    fn hover_channel(self: &Rc<Self>, stream_id: i64) {
        let Some(idx) = self.catalog.borrow().index_of(stream_id) else { return };
        self.hover_idx.set(Some(idx));
        let w = Rc::downgrade(self);
        self.hover_timer.start(TimerMode::SingleShot, Duration::from_millis(320), move || {
            if let Some(c) = w.upgrade() {
                if let Some(i) = c.hover_idx.get() {
                    c.set_hero(i);
                }
            }
        });
    }

    fn set_hero(self: &Rc<Self>, idx: usize) {
        if self.hero_idx.get() == Some(idx) {
            return;
        }
        self.hero_idx.set(Some(idx));
        let item = self.catalog.borrow().item(idx, None);
        self.with_state(|s| {
            s.set_hero(item);
            s.set_has_hero(true);
        });
        self.show_epg_for(idx, false);
    }

    /// Affiche l'EPG (hero ou lecteur) depuis le cache, et le télécharge si besoin.
    fn show_epg_for(self: &Rc<Self>, idx: usize, for_player: bool) {
        let Some(stream_id) = self.catalog.borrow().channels.get(idx).map(|c| c.stream_id) else { return };
        let cached = self.epg_cache.borrow().get(&stream_id).cloned();
        let fresh = cached.as_ref().map(|(t, _)| t.elapsed() < EPG_TTL).unwrap_or(false);
        let entries = cached.map(|(_, p)| epg_entries(&p)).unwrap_or_default();
        let model = ModelRc::new(VecModel::from(entries));
        self.with_state(|s| {
            if for_player {
                s.set_playing_epg(model);
            } else {
                s.set_hero_epg(model);
                s.set_hero_epg_loading(!fresh);
            }
        });
        if !fresh {
            self.fetch_epg(stream_id);
        }
    }

    fn fetch_epg(self: &Rc<Self>, stream_id: i64) {
        if !self.epg_inflight.borrow_mut().insert(stream_id) {
            return;
        }
        let Some(p) = self.profile.borrow().clone() else { return };
        let session = self.session.get();
        std::thread::spawn(move || {
            let res = Client::new(p.credentials()).short_epg(stream_id, 8);
            post(move |c| {
                c.epg_inflight.borrow_mut().remove(&stream_id);
                if c.session.get() != session {
                    return;
                }
                let progs = res.unwrap_or_else(|e| {
                    log::warn!("EPG {stream_id} : {e}");
                    Vec::new()
                });
                c.epg_cache.borrow_mut().insert(stream_id, (Instant::now(), progs));
                let id_of = |i: Option<usize>| {
                    i.and_then(|i| c.catalog.borrow().channels.get(i).map(|ch| ch.stream_id))
                };
                if id_of(c.hero_idx.get()) == Some(stream_id) {
                    c.show_epg_for(c.hero_idx.get().unwrap(), false);
                }
                if id_of(c.playing_idx.get()) == Some(stream_id) {
                    c.show_epg_for(c.playing_idx.get().unwrap(), true);
                }
            });
        });
    }

    // =====================================================================
    // Favoris

    fn toggle_favorite(self: &Rc<Self>, stream_id: i64) {
        let Some(pid) = self.profile.borrow().as_ref().map(|p| p.id) else { return };
        let Some(idx) = self.catalog.borrow().index_of(stream_id) else { return };
        let fav = self.catalog.borrow_mut().toggle_favorite(stream_id);
        if let Err(e) = self.store.borrow().set_favorite(pid, stream_id, fav) {
            log::error!("favori : {e}");
        }
        self.grid.refresh_channel(idx);
        self.zap.refresh_channel(idx);
        let item = self.catalog.borrow().item(idx, None);
        let fav_count = self.valid_favorites().len() as i32;
        let sel_fav =
            self.ui().map(|a| a.global::<AppState>().get_selected_category() == FAV).unwrap_or(false);
        self.with_state(|s| {
            s.set_fav_count(fav_count);
            if self.hero_idx.get() == Some(idx) {
                s.set_hero(item.clone());
            }
            if self.playing_idx.get() == Some(idx) {
                s.set_playing(item.clone());
            }
        });
        if sel_fav {
            self.apply_filter();
        }
        self.toast(if fav { "Ajoutée à vos favoris" } else { "Retirée de vos favoris" });
    }

    // =====================================================================
    // Lecture

    fn play_stream(self: &Rc<Self>, stream_id: i64) {
        let Some(idx) = self.catalog.borrow().index_of(stream_id) else { return };
        // Liste de zapping : la sélection courante si elle contient la chaîne,
        // sinon toute la catégorie de la chaîne.
        let filtered = self.filtered.borrow().clone();
        let in_zap = self.zap.indices().contains(&idx);
        if !in_zap {
            let (list, title) = if filtered.contains(&idx) {
                let t = self
                    .ui()
                    .map(|a| a.global::<AppState>().get_section_title().to_string())
                    .unwrap_or_default();
                (filtered, t)
            } else {
                let cat = self.catalog.borrow();
                let cid = cat.channels[idx].category_id.clone();
                let list: Vec<usize> = cat
                    .channels
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.category_id == cid)
                    .map(|(i, _)| i)
                    .collect();
                let t =
                    cat.categories.iter().find(|c| c.id == cid).map(|c| c.name.clone()).unwrap_or_default();
                (Rc::new(list), t)
            };
            self.zap.set_indices(list);
            self.with_state(|s| s.set_zap_title(title.into()));
        }
        self.play_index(idx);
    }

    fn play_index(self: &Rc<Self>, idx: usize) {
        let Some(p) = self.profile.borrow().clone() else { return };
        let Some(stream_id) = self.catalog.borrow().channels.get(idx).map(|c| c.stream_id) else { return };
        let url = xtream::stream_url(&p.credentials(), stream_id);
        let item = self.catalog.borrow().item(idx, None);
        let zap_pos = self.zap.indices().iter().position(|i| *i == idx).map(|p| p as i32).unwrap_or(-1);

        self.playing_idx.set(Some(idx));
        self.retries.set(0);
        self.file_loaded.set(false);
        self.retry_timer.stop();
        self.with_state(|s| {
            s.set_playing(item);
            s.set_zap_index(zap_pos);
            s.set_player_error("".into());
            s.set_quality("".into());
            s.set_paused(false);
            s.set_buffering(true);
            s.set_screen(Screen::Player);
        });
        self.show_epg_for(idx, true);
        self.activity();
        if let Some(player) = &self.player {
            log::info!("lecture du flux {stream_id}");
            player.play(&url);
        }
        // Le hero suit la dernière chaîne regardée.
        self.set_hero(idx);
    }

    fn zap_by(self: &Rc<Self>, delta: i32) {
        let list = self.zap.indices();
        if list.is_empty() {
            return;
        }
        let cur = self
            .playing_idx
            .get()
            .and_then(|i| list.iter().position(|x| *x == i))
            .map(|p| p as i32)
            .unwrap_or(0);
        let n = list.len() as i32;
        let next = ((cur + delta) % n + n) % n;
        self.play_index(list[next as usize]);
    }

    fn retry(self: &Rc<Self>) {
        if let Some(idx) = self.playing_idx.get() {
            self.play_index(idx);
        }
    }

    fn stop_playback(&self) {
        self.retry_timer.stop();
        if let Some(p) = &self.player {
            if p.is_active() {
                p.stop();
            }
        }
        self.playing_idx.set(None);
    }

    fn back(self: &Rc<Self>) {
        self.stop_playback();
        self.store.borrow().set_setting("volume", &format!("{:.0}", self.volume.get()));
        if let Some(app) = self.ui() {
            if app.window().is_fullscreen() {
                app.window().set_fullscreen(false);
            }
            let s = app.global::<AppState>();
            s.set_fullscreen(false);
            s.set_zap_panel(false);
            s.set_buffering(false);
            s.set_player_error("".into());
            s.set_screen(Screen::Home);
        }
        self.hide_timer.stop();
    }

    fn toggle_pause(&self) {
        let paused = self.ui().map(|a| a.global::<AppState>().get_paused()).unwrap_or(false);
        if let Some(p) = &self.player {
            p.set_pause(!paused);
        }
    }

    fn set_volume(&self, v: f32) {
        let v = v.clamp(0.0, 100.0);
        self.volume.set(v);
        self.with_state(|s| s.set_volume(v));
        if let Some(p) = &self.player {
            p.set_volume(v as f64);
            if v > 0.0 && self.ui().map(|a| a.global::<AppState>().get_muted()).unwrap_or(false) {
                p.set_mute(false);
            }
        }
    }

    fn toggle_mute(&self) {
        let muted = self.ui().map(|a| a.global::<AppState>().get_muted()).unwrap_or(false);
        if let Some(p) = &self.player {
            p.set_mute(!muted);
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(app) = self.ui() {
            let fs = !app.window().is_fullscreen();
            app.window().set_fullscreen(fs);
            app.global::<AppState>().set_fullscreen(fs);
        }
    }

    fn activity(self: &Rc<Self>) {
        self.with_state(|s| {
            if !s.get_controls_visible() {
                s.set_controls_visible(true);
            }
        });
        let w = Rc::downgrade(self);
        self.hide_timer.start(TimerMode::SingleShot, Duration::from_millis(3200), move || {
            if let Some(c) = w.upgrade() {
                c.with_state(|s| s.set_controls_visible(false));
            }
        });
    }

    fn on_player_event(self: &Rc<Self>, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Paused(p) => self.with_state(|s| s.set_paused(p)),
            PlayerEvent::Buffering(b) => {
                if self.file_loaded.get() {
                    self.with_state(|s| s.set_buffering(b));
                }
            }
            PlayerEvent::Volume(v) => {
                self.volume.set(v as f32);
                self.with_state(|s| s.set_volume(v as f32));
            }
            PlayerEvent::Muted(m) => self.with_state(|s| s.set_muted(m)),
            PlayerEvent::VideoHeight(h) => self.with_state(|s| s.set_quality(quality_label(h).into())),
            PlayerEvent::FileLoaded => self.file_loaded.set(true),
            PlayerEvent::PlaybackRestart => {
                self.file_loaded.set(true);
                self.with_state(|s| {
                    s.set_buffering(false);
                    s.set_player_error("".into());
                });
            }
            PlayerEvent::EndFile { reason, error } => {
                use crate::mpv::{MPV_END_FILE_REASON_EOF, MPV_END_FILE_REASON_ERROR};
                if reason != MPV_END_FILE_REASON_EOF && reason != MPV_END_FILE_REASON_ERROR {
                    return;
                }
                let Some(idx) = self.playing_idx.get() else { return };
                if !self.player.as_ref().map(|p| p.is_active()).unwrap_or(false) {
                    return;
                }
                // Flux live coupé : on tente de se reconnecter automatiquement.
                let tries = self.retries.get();
                if tries < MAX_RETRIES {
                    self.retries.set(tries + 1);
                    self.file_loaded.set(false);
                    self.with_state(|s| s.set_buffering(true));
                    let w = Rc::downgrade(self);
                    let delay = Duration::from_millis(1200 * (tries as u64 + 1));
                    self.retry_timer.start(TimerMode::SingleShot, delay, move || {
                        if let Some(c) = w.upgrade() {
                            if c.playing_idx.get() != Some(idx) {
                                return;
                            }
                            let url = {
                                let prof = c.profile.borrow();
                                let cat = c.catalog.borrow();
                                prof.as_ref()
                                    .zip(cat.channels.get(idx))
                                    .map(|(p, ch)| xtream::stream_url(&p.credentials(), ch.stream_id))
                            };
                            if let (Some(url), Some(p)) = (url, &c.player) {
                                log::info!("reconnexion ({}/{MAX_RETRIES})", c.retries.get());
                                p.play(&url);
                            }
                        }
                    });
                } else {
                    let msg = match error {
                        Some(e) => format!("Le flux ne répond pas ({e})."),
                        None => "Le flux s'est interrompu.".to_string(),
                    };
                    self.with_state(|s| {
                        s.set_buffering(false);
                        s.set_player_error(msg.into());
                    });
                }
            }
        }
    }

    /// Sauvegarde avant la fermeture.
    pub fn on_exit(&self) {
        self.store.borrow().set_setting("volume", &format!("{:.0}", self.volume.get()));
    }

    pub fn request_redraw_fn(&self) -> crate::player::RedrawFn {
        let app = self.app.clone();
        std::sync::Arc::new(move || {
            let app = app.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(a) = app.upgrade() {
                    a.window().request_redraw();
                }
            });
        })
    }
}

impl LogoListener for Controller {
    /// Met à jour le hero et le lecteur quand leur logo vient d'arriver.
    fn logo_ready(&self, url: &str) {
        let logo_of =
            |i: Option<usize>| i.and_then(|i| self.catalog.borrow().channels.get(i).map(|c| c.logo.clone()));
        if logo_of(self.hero_idx.get()).as_deref() == Some(url) {
            let item = self.catalog.borrow().item(self.hero_idx.get().unwrap(), None);
            self.with_state(|s| s.set_hero(item));
        }
        if logo_of(self.playing_idx.get()).as_deref() == Some(url) {
            let item = self.catalog.borrow().item(self.playing_idx.get().unwrap(), None);
            self.with_state(|s| s.set_playing(item));
        }
    }
}

pub fn shutdown() -> Option<Rc<Controller>> {
    CONTROLLER.with(|c| c.borrow_mut().take())
}
