//! Интерфейс (eframe/egui).

mod app;
mod engine;
mod link;
#[cfg(windows)]
mod pipe;
mod plan;
mod platform;
#[cfg_attr(not(windows), allow(dead_code))]
mod proto;
#[cfg(windows)]
mod remote;
#[cfg(windows)]
mod server;
mod theme;
mod widgets;

#[cfg(windows)]
pub use server::{autostart, serve};

use eframe::egui;
use std::sync::Arc;

fn window_icon() -> egui::IconData {
    let rgba = include_bytes!("../../assets/icon_64.rgba").to_vec();
    egui::IconData { rgba, width: 64, height: 64 }
}

pub fn run(autostart: bool, demo: bool) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(vramdisk::APP_TITLE)
            .with_app_id("vramdisk")
            .with_inner_size([580.0, 800.0])
            .with_min_inner_size([520.0, 560.0])
            .with_icon(Arc::new(window_icon())),
        renderer: eframe::Renderer::Glow,
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(vramdisk::APP_TITLE, options, Box::new(move |cc| Ok(Box::new(app::VramApp::new(cc, autostart, demo)))))
}
