mod auth;
mod cli;
mod client;
mod command;
mod config;
mod event;
mod key;
mod log_layer;
#[cfg(feature = "media-control")]
mod media_control;
mod playlist_folders;
mod state;
#[cfg(feature = "streaming")]
mod streaming;
mod token;
mod ui;
mod utils;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{collections::VecDeque, io::Write, sync::Arc};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::apply_config_override;

fn open_current_log(
    log_folder: &std::path::Path,
    now: chrono::DateTime<chrono::Local>,
) -> Result<std::fs::File> {
    use std::io::BufRead;

    let path = log_folder.join("spotify-player.log");
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut first_line = String::new();
            std::io::BufReader::new(&file)
                .read_line(&mut first_line)
                .context("read current log timestamp")?;
            // Use the first entry, not mtime: ongoing writes must not postpone rotation.
            let started = if let Some(timestamp) = first_line.split_whitespace().next() {
                chrono::DateTime::parse_from_rfc3339(timestamp)
                    .context("parse current log timestamp")?
                    .with_timezone(&chrono::Local)
            } else {
                let metadata = file.metadata().context("read current log metadata")?;
                chrono::DateTime::<chrono::Local>::from(
                    metadata
                        .created()
                        .or_else(|_| metadata.modified())
                        .context("read empty log file age")?,
                )
            };
            drop(file);
            if started.date_naive() < now.date_naive() {
                let prefix = format!("spotify-player-{}", started.format("%Y-%m-%d-%H-%M-%S"));
                let mut backup = log_folder.join(format!("{prefix}.log"));
                let mut suffix = 1_u32;
                while backup.try_exists().context("check log backup path")? {
                    backup = log_folder.join(format!("{prefix}-{suffix}.log"));
                    suffix += 1;
                }
                std::fs::rename(&path, &backup).context("archive previous day's log")?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("open current log for rotation"),
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("open current log for appending")
}

fn init_logging(
    log_folder: &std::path::Path,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
) -> Result<()> {
    if std::env::var_os("RUST_LOG").is_some_and(|x| x == "off") {
        // Don't create log files if logging is disabled.
        return Ok(());
    }

    let log_prefix = format!(
        "spotify-player-{}",
        chrono::Local::now().format("%y-%m-%d-%H-%M")
    );

    // initialize the application's logging
    if std::env::var("RUST_LOG").is_err() {
        // default to log the current crate and librespot crates
        std::env::set_var("RUST_LOG", "spotify_player=info,librespot=info");
    }
    if !log_folder.exists() {
        std::fs::create_dir_all(log_folder)?;
    }
    let log_file = open_current_log(log_folder, chrono::Local::now())?;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file));

    let buffer_layer = crate::log_layer::BufferLayer::new(log_buffer, 1000);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(fmt_layer)
        .with(buffer_layer)
        .init();

    // initialize the application's panic backtrace
    let backtrace_file = std::fs::File::create(log_folder.join(format!("{log_prefix}.backtrace")))
        .context("failed to create backtrace file")?;
    let backtrace_file = std::sync::Mutex::new(backtrace_file);
    std::panic::set_hook(Box::new(move |info| {
        let mut file = backtrace_file.lock().unwrap();
        let backtrace = backtrace::Backtrace::new();
        writeln!(&mut file, "Got a panic: {info:#?}\n").unwrap();
        writeln!(&mut file, "Stack backtrace:\n{backtrace:?}").unwrap();
    }));

    Ok(())
}

#[tokio::main]
async fn start_app(state: &state::SharedState) -> Result<()> {
    // client channels
    let (client_pub, client_sub) = flume::unbounded::<client::ClientRequest>();

    #[cfg(feature = "pulseaudio-backend")]
    {
        // set environment variables for PulseAudio
        if std::env::var("PULSE_PROP_application.name").is_err() {
            std::env::set_var("PULSE_PROP_application.name", "spotify-player");
        }
        if std::env::var("PULSE_PROP_application.icon_name").is_err() {
            std::env::set_var("PULSE_PROP_application.icon_name", "spotify");
        }
        if std::env::var("PULSE_PROP_stream.description").is_err() {
            let configs = config::get_config();
            std::env::set_var(
                "PULSE_PROP_stream.description",
                format!(
                    "Spotify Connect endpoint ({})",
                    configs.app_config.device.name
                ),
            );
        }
        if std::env::var("PULSE_PROP_media.software").is_err() {
            std::env::set_var("PULSE_PROP_media.software", "Spotify");
        }
        if std::env::var("PULSE_PROP_media.role").is_err() {
            std::env::set_var("PULSE_PROP_media.role", "music");
        }
    }

    // create a Spotify API client
    let client = client::AppClient::new()
        .await
        .context("construct app client")?;
    client
        .new_session(Some(state), true)
        .await
        .context("initialize new Spotify session")?;

    // request user data
    client_pub.send(client::ClientRequest::GetCurrentUser)?;
    client_pub.send(client::ClientRequest::GetUserPlaylists)?;
    client_pub.send(client::ClientRequest::GetUserFollowedArtists)?;
    client_pub.send(client::ClientRequest::GetUserSavedAlbums)?;
    client_pub.send(client::ClientRequest::GetContext(state::ContextId::Tracks(
        state::USER_LIKED_TRACKS_ID.to_owned(),
    )))?;
    client_pub.send(client::ClientRequest::GetUserSavedShows)?;

    // client socket task (for handling CLI commands)
    tokio::task::spawn({
        let client = client.clone();
        let state = state.clone();
        async move {
            cli::start_socket(&client, Some(&state), None).await;
        }
    });

    // client event handler task
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_client_handler(&state, &client, &client_sub).await;
        }
    });

    // background task that detects an invalidated session and reconnects,
    // independent of any incoming client request
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_session_watcher(state, client).await;
        }
    });

    // player event watcher task
    std::thread::Builder::new()
        .name("player-event-watcher".to_string())
        .spawn({
            let state = state.clone();
            let client_pub = client_pub.clone();
            move || {
                client::start_player_event_watcher(&state, &client_pub);
            }
        })?;

    if !state.is_daemon {
        #[cfg(feature = "image")]
        ui::init_image_picker(state).context("initialize image picker")?;
        let terminal = ui::init_terminal().context("initialize terminal")?;

        // terminal event handler task
        std::thread::Builder::new()
            .name("terminal-event-handler".to_string())
            .spawn({
                let client_pub = client_pub.clone();
                let state = state.clone();
                move || {
                    event::start_event_handler(&state, &client_pub);
                }
            })?;

        // application UI task
        std::thread::Builder::new().name("ui".to_string()).spawn({
            let state = state.clone();
            move || ui::run(&state, terminal)
        })?;
    }

    #[cfg(feature = "media-control")]
    if config::get_config().app_config.enable_media_control {
        // media control task
        std::thread::Builder::new()
            .name("media-control".to_string())
            .spawn({
                let state = state.clone();
                move || {
                    if let Err(err) = media_control::start_event_watcher(&state, client_pub) {
                        tracing::error!(
                            "Failed to start the application's media control event watcher: err={err:#?}"
                        );
                    }
                }
            })?;

        // the winit's event loop must be run in the main thread
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Start an event loop that listens to OS window events.
            //
            // MacOS and Windows require an open window to be able to listen to media
            // control events. The below code will create an invisible window on startup
            // to listen to such events.
            let event_loop = winit::event_loop::EventLoop::new()?;
            #[allow(deprecated)]
            event_loop.run(move |_, _| {})?;
        }
    }

    // infinite loop to keep the main thread alive
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() -> Result<()> {
    // librespot depends on hyper-rustls which requires a crypto provider to be set up.
    // TODO: see if this can be fixed upstream
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap();

    // parse command line arguments
    let args = cli::init_cli()?.get_matches();

    // initialize the application's cache and config folders
    let config_folder: std::path::PathBuf = args
        .get_one::<String>("config-folder")
        .expect("config-folder should have default value")
        .into();
    if !config_folder.exists() {
        std::fs::create_dir_all(&config_folder)?;
    }

    let cache_folder: std::path::PathBuf = args
        .get_one::<String>("cache-folder")
        .expect("cache-folder should have a default value")
        .into();
    let cache_audio_folder = cache_folder.join("audio");
    if !cache_audio_folder.exists() {
        std::fs::create_dir_all(&cache_audio_folder)?;
    }
    let cache_image_folder = cache_folder.join("image");
    if !cache_image_folder.exists() {
        std::fs::create_dir_all(&cache_image_folder)?;
    }

    // initialize the application configs
    {
        let mut configs = config::Configs::new(&config_folder, &cache_folder)?;
        if configs.app_config.log_folder.is_none() {
            // set the log folder to be the cache folder if it is not set
            configs.app_config.log_folder = Some(cache_folder);
        }
        if let Some(overrides) = args.get_many::<String>("config-override") {
            for override_str in overrides {
                let (key, value) = override_str.split_once('=').context(format!(
                    "Invalid override format: '{override_str}'. Expected KEY=VALUE"
                ))?;

                apply_config_override(&mut configs.app_config, key, value)?;
            }
        }
        config::set_config(configs);
    }

    match args.subcommand() {
        None => {
            // initialize the application's log
            let log_folder = config::get_config()
                .app_config
                .log_folder
                .as_deref()
                .expect("log_folder is set");

            let log_buffer: Arc<Mutex<VecDeque<String>>> =
                Arc::new(Mutex::new(VecDeque::with_capacity(1000)));

            init_logging(log_folder, log_buffer.clone())
                .context("failed to initialize application's logging")?;

            // log the application's configurations
            tracing::info!("Configurations: {:?}", config::get_config());

            let is_daemon;

            #[cfg(feature = "daemon")]
            {
                is_daemon = args.get_flag("daemon");
                if is_daemon {
                    if cfg!(any(target_os = "macos", target_os = "windows"))
                        && cfg!(feature = "media-control")
                    {
                        eprintln!("Running the application as a daemon on windows/macos with `media-control` feature enabled is not supported!");
                        std::process::exit(1);
                    }

                    tracing::info!("Starting the application as a daemon...");
                    let daemonize = daemonize::Daemonize::new();
                    daemonize.start()?;
                }
            }

            #[cfg(not(feature = "daemon"))]
            {
                is_daemon = false;
            }

            let state = std::sync::Arc::new(state::State::new(is_daemon, log_buffer));
            start_app(&state)
        }
        Some((cmd, args)) => cli::handle_cli_subcommand(cmd, args),
    }
}

#[cfg(test)]
mod logging_tests {
    use super::open_current_log;
    use chrono::{Local, TimeZone};
    use std::{io::Write, path::PathBuf};

    struct LogDirectory(PathBuf);

    impl LogDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "spotify-player-log-tests-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn files(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect()
        }
    }

    impl Drop for LogDirectory {
        fn drop(&mut self) {
            for path in self.files() {
                std::fs::remove_file(path).unwrap();
            }
            std::fs::remove_dir(&self.0).unwrap();
        }
    }

    fn now() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 8, 0, 0, 1)
            .single()
            .unwrap()
    }

    #[test]
    fn new_log_uses_stable_name_and_same_day_launches_append() {
        let directory = LogDirectory::new();
        let entry = format!("{} INFO first launch\n", now().to_rfc3339());
        {
            let mut file = open_current_log(&directory.0, now()).unwrap();
            file.write_all(entry.as_bytes()).unwrap();
        }
        {
            let mut file = open_current_log(&directory.0, now()).unwrap();
            writeln!(file, "second launch").unwrap();
        }
        assert_eq!(directory.files().len(), 1);
        assert_eq!(
            std::fs::read_to_string(directory.0.join("spotify-player.log")).unwrap(),
            format!("{entry}second launch\n")
        );
    }

    #[test]
    fn new_calendar_day_archives_log_even_if_recently_modified() {
        let directory = LogDirectory::new();
        let started = now() - chrono::Duration::seconds(2);
        let entry = format!("{} INFO previous day\n", started.to_rfc3339());
        let current = directory.0.join("spotify-player.log");
        std::fs::write(&current, &entry).unwrap();
        drop(open_current_log(&directory.0, now()).unwrap());
        let backup = directory.0.join(format!(
            "spotify-player-{}.log",
            started.format("%Y-%m-%d-%H-%M-%S")
        ));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), entry);
        assert_eq!(std::fs::read_to_string(current).unwrap(), "");
        assert_eq!(directory.files().len(), 2);
    }

    #[test]
    fn rotation_does_not_overwrite_existing_backups() {
        let directory = LogDirectory::new();
        let started = now() - chrono::Duration::days(1);
        let prefix = format!("spotify-player-{}", started.format("%Y-%m-%d-%H-%M-%S"));
        let backup = directory.0.join(format!("{prefix}.log"));
        let next_backup = directory.0.join(format!("{prefix}-1.log"));
        std::fs::write(&backup, "existing backup").unwrap();
        std::fs::write(&next_backup, "another backup").unwrap();
        let entry = format!("{} INFO old log\n", started.to_rfc3339());
        std::fs::write(directory.0.join("spotify-player.log"), &entry).unwrap();
        drop(open_current_log(&directory.0, now()).unwrap());
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "existing backup");
        assert_eq!(
            std::fs::read_to_string(next_backup).unwrap(),
            "another backup"
        );
        assert_eq!(
            std::fs::read_to_string(directory.0.join(format!("{prefix}-2.log"))).unwrap(),
            entry
        );
    }

    #[test]
    fn empty_log_can_be_reopened() {
        let directory = LogDirectory::new();
        drop(open_current_log(&directory.0, Local::now()).unwrap());
        drop(open_current_log(&directory.0, Local::now()).unwrap());
        assert_eq!(directory.files().len(), 1);
    }

    #[test]
    fn invalid_log_timestamp_reports_error_without_changing_file() {
        let directory = LogDirectory::new();
        let current = directory.0.join("spotify-player.log");
        std::fs::write(&current, "invalid timestamp\n").unwrap();
        let err = open_current_log(&directory.0, now()).unwrap_err();
        assert_eq!(err.to_string(), "parse current log timestamp");
        assert_eq!(
            std::fs::read_to_string(current).unwrap(),
            "invalid timestamp\n"
        );
        assert_eq!(directory.files().len(), 1);
    }
}
