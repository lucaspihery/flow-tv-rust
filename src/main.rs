// Pas de console en version release sous Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod db;
mod logos;
mod models;
mod mpv;
mod player;
mod xtream;

use slint::{ComponentHandle, GraphicsAPI, RenderingState};

slint::include_modules!();

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Le rendu OpenGL (femtovg) est nécessaire pour dessiner la vidéo mpv sous l'interface.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let _ = slint::BackendSelector::new().renderer_name("femtovg".into()).select();
    }

    let store = match db::Store::open(&db::db_path()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Impossible d'ouvrir la base de données {} : {e}", db::db_path().display());
            std::process::exit(1);
        }
    };

    let ui = AppWindow::new().expect("création de la fenêtre");
    logos::init();
    let ctl = app::Controller::new(&ui, store);

    // Intégration de la vidéo : mpv dessine dans le framebuffer avant l'interface.
    if ctl.player.is_some() {
        let weak_ctl = std::rc::Rc::downgrade(&ctl);
        let weak_ui = ui.as_weak();
        let redraw = ctl.request_redraw_fn();
        let res = ui.window().set_rendering_notifier(move |state, api| {
            let Some(ctl) = weak_ctl.upgrade() else { return };
            let Some(player) = ctl.player.as_ref() else { return };
            match state {
                RenderingState::RenderingSetup => {
                    if let GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                        player.setup_gl(get_proc_address, redraw.clone());
                    }
                }
                RenderingState::BeforeRendering => {
                    if let Some(ui) = weak_ui.upgrade() {
                        let size = ui.window().size();
                        player.render(size.width, size.height);
                    }
                }
                RenderingState::RenderingTeardown => player.teardown_gl(),
                _ => {}
            }
        });
        if let Err(e) = res {
            log::error!("rendu vidéo indisponible : {e:?}");
            let s = ui.global::<AppState>();
            s.set_mpv_available(false);
            s.set_mpv_error("Le moteur de rendu OpenGL n'est pas disponible.".into());
        }
    }

    // Ouverture en plein écran fenêtré (maximisée).
    ui.window().set_maximized(true);

    ui.run().expect("boucle d'événements");

    ctl.on_exit();
    drop(ui);
    let ctl = app::shutdown().unwrap_or(ctl);
    let clean = ctl.player.as_ref().map(|p| p.shutdown()).unwrap_or(true);
    if !clean {
        // Le contexte GL n'existe plus : on laisse l'OS libérer les ressources.
        std::process::exit(0);
    }
}
