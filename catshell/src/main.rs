//! catshell: a terminal and file explorer that share one connection.

mod app;
mod config;
mod debug;
mod explorer;
mod font;
mod input;
mod pane;
mod render;
mod ssh;

use config::Config;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "catshell=info".into()),
        )
        .init();

    let config = Config::load();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("catshell")
            .with_inner_size([1024.0, 640.0])
            .with_min_inner_size([320.0, 200.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "catshell",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, config)?))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
}
