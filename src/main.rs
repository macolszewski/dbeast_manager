use dbeast::ui;

fn main() -> eframe::Result {
    ui::branding::mark_process_start();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DBeast")
            .with_icon(ui::branding::window_icon())
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    // macOS: the main toolbar becomes the title bar.
    #[cfg(target_os = "macos")]
    let options = eframe::NativeOptions {
        viewport: options
            .viewport
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        renderer: eframe::Renderer::Wgpu,
        // A database client gains nothing from the discrete GPU or a second
        // queued frame: integrated GPU, Metal only and one frame in flight.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            surface: eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY,
            wgpu_setup: {
                let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
                setup.instance_descriptor.backends = eframe::wgpu::Backends::METAL;
                setup.power_preference = eframe::wgpu::PowerPreference::LowPower;
                eframe::egui_wgpu::WgpuSetup::CreateNew(setup)
            },
            ..Default::default()
        },
        ..options
    };
    eframe::run_native(
        "DBeast",
        options,
        Box::new(|cc| {
            Ok(Box::new(
                ui::App::new(cc)
                    .with_startup_splash()
                    .with_integrated_titlebar(cfg!(target_os = "macos")),
            ))
        }),
    )
}
