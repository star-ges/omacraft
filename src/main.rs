mod auth;
mod cli;
mod config;
mod content_meta;
mod export;
mod fsutil;
mod groups;
mod icons;
mod import;
mod instance;
mod keymap;
mod java;
mod launch;
mod loaders;
mod modrinth;
mod mojang;
mod net;
mod paths;
mod saves;
mod shares;
mod skins;
mod store;
mod task;
mod theme;
mod ui;

fn main() -> eframe::Result {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(rt.block_on(cli::run(args)));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(paths::APP_ID)
            .with_title("Omacraft")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            // Omarchy windows have no title bars; Hyprland draws the border.
            .with_decorations(false),
        ..Default::default()
    };
    let handle = rt.handle().clone();
    eframe::run_native("Omacraft", options, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, handle)))))
}
