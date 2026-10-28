use signal_rusted::app::App;

fn main() -> iced::Result {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::metadata::LevelFilter::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=error".parse().expect("valid directive"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // SIGNAL_RS_DATA lets you run a second, independent profile (e.g. for testing).
    // The data dir (and keyring entry) keep the pre-rename name: renaming would orphan the database.
    let data_dir = std::env::var_os("SIGNAL_RS_DATA").map(Into::into).unwrap_or_else(|| {
        directories::ProjectDirs::from("org", "signal-rs", "signal-rs")
            .expect("home directory")
            .data_dir()
            .to_owned()
    });

    iced::application(move || App::new(data_dir.clone()), App::update, signal_rusted::view::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: (1100.0, 760.0).into(),
            icon: iced::window::icon::from_file_data(include_bytes!("../assets/icon-256.png"), None).ok(),
            // Wayland takes the taskbar icon from the .desktop file with this name (packaging/).
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "signal-rusted".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}
