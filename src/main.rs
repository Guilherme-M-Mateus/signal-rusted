use signal_rs::app::App;

fn main() -> iced::Result {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::metadata::LevelFilter::INFO.into())
        .from_env_lossy()
        .add_directive("libsignal=error".parse().expect("valid directive"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // SIGNAL_RS_DATA lets you run a second, independent profile (e.g. for testing).
    let data_dir = std::env::var_os("SIGNAL_RS_DATA").map(Into::into).unwrap_or_else(|| {
        directories::ProjectDirs::from("org", "signal-rs", "signal-rs")
            .expect("home directory")
            .data_dir()
            .to_owned()
    });

    iced::application(move || App::new(data_dir.clone()), App::update, signal_rs::view::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size((1100.0, 760.0))
        .run()
}
