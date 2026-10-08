use crate::{client::AppClient, config, state::SharedState};
use anyhow::Context;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::config::DeviceType;
use librespot_core::{spotify_uri, Session, SpotifyUri};
use librespot_playback::audio_backend::Sink;
use librespot_playback::mixer::MixerConfig;
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, PlayerConfig},
    mixer::{self, Mixer},
    player,
};
use parking_lot::Mutex;
use rspotify::model::{EpisodeId, Id, PlayableId, TrackId};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether the next streaming connection is the first one of the process.
///
/// Used to scope `pause_on_startup` to application startup only, so that
/// reconnecting mid-session (e.g. via `RestartIntegratedClient`) does not
/// pause an intentionally playing track.
static IS_FIRST_CONNECTION: AtomicBool = AtomicBool::new(true);

#[derive(Default)]
struct PauseGuard {
    playing: bool,
    local_pause: bool,
    shutting_down: bool,
    pause_generation: u64,
}

impl PauseGuard {
    fn allow_pause(&mut self) -> u64 {
        self.pause_generation = self.pause_generation.wrapping_add(1);
        self.local_pause = self.playing;
        self.pause_generation
    }

    fn cancel_pause(&mut self, generation: u64) {
        if self.pause_generation == generation {
            self.local_pause = false;
        }
    }

    fn paused(&mut self) -> bool {
        if self.shutting_down || self.local_pause || !self.playing {
            self.local_pause = false;
            self.playing = false;
            false
        } else {
            true
        }
    }

    fn reset(&mut self) {
        self.playing = false;
        self.local_pause = false;
    }

    fn handle_event(&mut self, event: &player::PlayerEvent) -> bool {
        if self.shutting_down {
            return false;
        }
        match event {
            player::PlayerEvent::Playing { .. } => self.playing = true,
            player::PlayerEvent::Paused { .. } => return self.paused(),
            player::PlayerEvent::Loading { .. }
            | player::PlayerEvent::Stopped { .. }
            | player::PlayerEvent::EndOfTrack { .. }
            | player::PlayerEvent::Unavailable { .. } => self.reset(),
            _ => {}
        }
        false
    }
}

pub struct StreamingConnection {
    spirc: Arc<Spirc>,
    pause_guard: Mutex<PauseGuard>,
}

impl StreamingConnection {
    pub fn pause(&self) -> anyhow::Result<()> {
        let mut guard = self.pause_guard.lock();
        self.spirc.pause().context("pause integrated player")?;
        guard.allow_pause();
        Ok(())
    }

    pub fn allow_pause(&self) -> u64 {
        self.pause_guard.lock().allow_pause()
    }

    pub fn cancel_pause(&self, generation: u64) {
        self.pause_guard.lock().cancel_pause(generation);
    }

    pub fn shutdown(&self) -> anyhow::Result<()> {
        let mut guard = self.pause_guard.lock();
        self.spirc
            .shutdown()
            .context("shut down integrated player")?;
        guard.reset();
        guard.shutting_down = true;
        Ok(())
    }
}

#[cfg(not(any(
    feature = "rodio-backend",
    feature = "alsa-backend",
    feature = "pulseaudio-backend",
    feature = "portaudio-backend",
    feature = "jackaudio-backend",
    feature = "rodiojack-backend",
    feature = "sdl-backend",
    feature = "gstreamer-backend"
)))]
compile_error!("Streaming feature is enabled but no audio backend has been selected. Consider adding one of the following features:
    rodio-backend,
    alsa-backend,
    pulseaudio-backend,
    portaudio-backend,
    jackaudio-backend,
    rodiojack-backend,
    sdl-backend,
    gstreamer-backend
For more information, visit https://github.com/aome510/spotify-player?tab=readme-ov-file#streaming
");

#[derive(Debug, Serialize)]
enum PlayerEvent {
    Changed {
        playable_id: PlayableId<'static>,
    },
    Playing {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    Paused {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    EndOfTrack {
        playable_id: PlayableId<'static>,
    },
}

impl PlayerEvent {
    /// gets the event's arguments
    pub fn args(&self) -> Vec<String> {
        match self {
            PlayerEvent::Changed { playable_id } => {
                vec!["Changed".to_string(), playable_id.uri()]
            }
            PlayerEvent::Playing {
                playable_id,
                position_ms,
            } => vec![
                "Playing".to_string(),
                playable_id.uri(),
                position_ms.to_string(),
            ],
            PlayerEvent::Paused {
                playable_id,
                position_ms,
            } => vec![
                "Paused".to_string(),
                playable_id.uri(),
                position_ms.to_string(),
            ],
            PlayerEvent::EndOfTrack { playable_id } => {
                vec!["EndOfTrack".to_string(), playable_id.uri()]
            }
        }
    }
}

/// Converts a percentage volume value (0-100) into `librespot`'s internal volume scale (0-65535).
pub fn percent_to_volume(percent: u8) -> u16 {
    (f64::from(std::cmp::min(percent, 100_u8)) / 100.0 * 65535.0).round() as u16
}

/// Converts a `librespot`-scale volume value (0-65535) back into a percentage (0-100).
fn volume_to_percent(volume: u16) -> u8 {
    (f64::from(volume) / 65535.0 * 100.0).round() as u8
}

fn spotify_id_to_playable_id(uri: &spotify_uri::SpotifyUri) -> anyhow::Result<PlayableId<'static>> {
    match uri {
        SpotifyUri::Track { .. } => {
            let uri = uri.to_uri()?;
            Ok(TrackId::from_uri(&uri)?.into_static().into())
        }
        SpotifyUri::Episode { .. } => {
            let uri = uri.to_uri()?;
            Ok(EpisodeId::from_uri(&uri)?.into_static().into())
        }
        _ => anyhow::bail!("unexpected spotify_id {uri:?}"),
    }
}

impl PlayerEvent {
    pub fn from_librespot_player_event(e: player::PlayerEvent) -> anyhow::Result<Option<Self>> {
        Ok(match e {
            player::PlayerEvent::TrackChanged { audio_item } => Some(PlayerEvent::Changed {
                playable_id: spotify_id_to_playable_id(&audio_item.track_id)?,
            }),
            player::PlayerEvent::Playing {
                track_id,
                position_ms,
                ..
            } => Some(PlayerEvent::Playing {
                playable_id: spotify_id_to_playable_id(&track_id)?,
                position_ms,
            }),
            player::PlayerEvent::Paused {
                track_id,
                position_ms,
                ..
            } => Some(PlayerEvent::Paused {
                playable_id: spotify_id_to_playable_id(&track_id)?,
                position_ms,
            }),
            player::PlayerEvent::EndOfTrack { track_id, .. } => Some(PlayerEvent::EndOfTrack {
                playable_id: spotify_id_to_playable_id(&track_id)?,
            }),
            _ => None,
        })
    }
}

fn execute_player_event_hook_command(
    cmd: &config::Command,
    event: &PlayerEvent,
) -> anyhow::Result<()> {
    cmd.execute(Some(event.args()))?;

    Ok(())
}

/// Create a new streaming connection
pub async fn new_connection(
    client: AppClient,
    state: SharedState,
    session: Session,
    creds: Credentials,
) -> anyhow::Result<Arc<StreamingConnection>> {
    let configs = config::get_config();
    let device = &configs.app_config.device;

    // `librespot` volume is a u16 number ranging from 0 to 65535,
    // while a percentage volume value (from 0 to 100) is used for the device configuration.
    // So we need to convert from one format to another
    let volume = percent_to_volume(device.volume);

    // Establish our own baseline for this connection's volume so that any later
    // `VolumeChanged` event that doesn't match a value we intentionally set can be
    // recognized as an unsolicited remote Spotify Connect command (see the
    // `player_event_task` below).
    *state.expected_volume.lock() = Some(volume);

    let connect_config = ConnectConfig {
        name: device.name.clone(),
        device_type: device.device_type.parse::<DeviceType>().unwrap_or_default(),
        initial_volume: volume,

        // non-configurable fields, use default values.
        // We may allow users to configure these fields in a future release
        is_group: false,
        disable_volume: false,
        volume_steps: 64,
    };

    tracing::info!("Application's connect configurations: {:?}", connect_config);

    let mixer = Arc::new(
        mixer::softmixer::SoftMixer::open(MixerConfig::default()).context("opening softmixer")?,
    );
    mixer.set_volume(volume);

    let backend = audio_backend::find(None).expect("should be able to find an audio backend");
    let player_config = PlayerConfig {
        bitrate: device
            .bitrate
            .to_string()
            .parse::<Bitrate>()
            .unwrap_or_default(),
        normalisation: device.normalization,
        ..Default::default()
    };

    tracing::info!(
        "Initializing a new integrated player with device_id={}",
        session.device_id()
    );

    let player = {
        // Clone the Option<Arc<...>> so the factory closure can move it.
        // vis_bands is Some iff enable_audio_visualization is true.
        let vis_bands = state.vis_bands.as_ref().map(Arc::clone);
        player::Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || -> Box<dyn Sink> {
                let real = backend(None, AudioFormat::default());
                if let Some(ref bands) = vis_bands {
                    Box::new(crate::ui::streaming::VisualizationSink::new(
                        real,
                        Arc::clone(bands),
                        // librespot defaults to 44100 Hz; adjust here if
                        // PlayerConfig::sample_rate is changed in the future.
                        44_100.0,
                    ))
                } else {
                    real
                }
            },
        )
    };

    let player_event_channel = player.get_player_event_channel();

    // When `pause_on_startup` is enabled, suppress Spotify's auto-resume of the
    // previous session by pausing the first auto-started playback. Scoped to the
    // first connection of the process so mid-session reconnects are unaffected.
    let pause_on_startup =
        configs.app_config.pause_on_startup && IS_FIRST_CONNECTION.swap(false, Ordering::SeqCst);

    tracing::info!("Starting an integrated Spotify player using librespot's spirc protocol");

    // Created before spawning the player event task below so that the task can hold
    // a handle to `spirc` and revert unsolicited remote volume commands (see the
    // `VolumeChanged` handling below).
    let (spirc, spirc_task) = Spirc::new(connect_config, session.clone(), creds, player, mixer)
        .await
        .context("initialize spirc")?;
    let spirc = Arc::new(spirc);
    let connection = Arc::new(StreamingConnection {
        spirc: Arc::clone(&spirc),
        pause_guard: Mutex::new(PauseGuard::default()),
    });

    let mut player_event_task = tokio::task::spawn({
        let mut channel = player_event_channel;
        let spirc = Arc::clone(&spirc);
        let connection = Arc::clone(&connection);
        async move {
            let mut pause_armed = pause_on_startup;
            while let Some(event) = channel.recv().await {
                let unexpected_pause = connection.pause_guard.lock().handle_event(&event);
                // Suppress Spotify's auto-resume of the previous session on
                // startup. The `librespot` connect transfer finalizes the
                // play state asynchronously, so a single reactive pause is not
                // reliable on its own:
                if pause_armed {
                    match &event {
                        // Best-effort: pause as the track starts loading, before
                        // the audio sink starts, so no audible blip occurs. This
                        // is a no-op if the transfer has not set the play state
                        // yet, so we do NOT disarm here.
                        player::PlayerEvent::Loading { .. } => {
                            if let Err(err) = connection.pause() {
                                tracing::warn!(
                                    "Failed to pause integrated client on startup: {err:#}"
                                );
                            }
                        }
                        // Authoritative: playback actually started (the transfer
                        // finalized into "playing"). Pause and stop interfering.
                        player::PlayerEvent::Playing { .. } => match connection.pause() {
                            Ok(()) => pause_armed = false,
                            Err(err) => tracing::warn!(
                                "Failed to pause integrated client on startup: {err:#}"
                            ),
                        },
                        // The track finished loading already paused, i.e. the
                        // `Loading` pause above took effect and no audio played.
                        player::PlayerEvent::Paused { .. } => {
                            pause_armed = false;
                        }
                        _ => {}
                    }
                }

                if unexpected_pause {
                    // Librespot does not expose the command sender and has already paused.
                    tracing::warn!(
                        "Detected an unsolicited pause of the integrated player; resuming playback"
                    );
                    match spirc.play() {
                        Ok(()) => continue,
                        Err(err) => tracing::error!("Failed to undo unsolicited pause: {err:#}"),
                    }
                }

                // Detect and revert unsolicited remote Spotify Connect volume commands.
                //
                // `librespot` applies a remote `SetVolumeCommand` (issued by *any*
                // device signed into the account, e.g. a TV or phone) to the mixer
                // before this event is observable, so it cannot be blocked
                // pre-emptively. Instead, we compare the reported volume against the
                // last volume this app intentionally set (`state.expected_volume`,
                // updated on connect and on local `PlayerRequest::Volume`/`ToggleMute`
                // requests). A mismatch means some other device changed our volume;
                // we revert it and log the event so it's visible/confirmable.
                if let player::PlayerEvent::VolumeChanged { volume: reported } = &event {
                    let reported = *reported;
                    let mut expected = state.expected_volume.lock();
                    match *expected {
                        Some(exp)
                            if volume_to_percent(exp).abs_diff(volume_to_percent(reported))
                                <= 1 => {}
                        Some(exp) => {
                            tracing::warn!(
                                "Detected an unsolicited remote volume change via Spotify Connect \
                                 ({}% -> {}%); ignoring it and reverting to {}%",
                                volume_to_percent(exp),
                                volume_to_percent(reported),
                                volume_to_percent(exp)
                            );
                            match spirc.set_volume(exp) {
                                Ok(()) => tracing::info!(
                                    "Reverted volume back to {}% after an unsolicited remote change",
                                    volume_to_percent(exp)
                                ),
                                Err(err) => tracing::warn!(
                                    "Failed to revert an unsolicited remote volume change: {err:#}"
                                ),
                            }
                        }
                        None => {
                            *expected = Some(reported);
                        }
                    }
                }

                match PlayerEvent::from_librespot_player_event(event) {
                    Err(err) => {
                        tracing::warn!("Failed to convert a `librespot` player event into `spotify_player` player event: {err:#}");
                    }
                    Ok(Some(event)) => {
                        tracing::info!("Got a new player event: {event:?}");
                        match event {
                            PlayerEvent::Playing { .. } => {
                                let mut player = state.player.write();
                                if let Some(playback) = player.buffered_playback.as_mut() {
                                    playback.is_playing = true;
                                }
                                if let Some(ref bands) = state.vis_bands {
                                    bands.lock().is_active = true;
                                }
                            }
                            PlayerEvent::Paused { .. } => {
                                let mut player = state.player.write();
                                if let Some(playback) = player.buffered_playback.as_mut() {
                                    playback.is_playing = false;
                                }
                                if let Some(ref bands) = state.vis_bands {
                                    bands.lock().is_active = false;
                                }
                            }
                            _ => {}
                        }
                        client.update_playback_non_blocking(&state);

                        // execute a player event hook command
                        if let Some(ref cmd) = configs.app_config.player_event_hook_command {
                            if let Err(err) = execute_player_event_hook_command(cmd, &event) {
                                tracing::warn!(
                                    "Failed to execute player event hook command: {err:#}"
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                }
            }
        }
    });

    tokio::task::spawn({
        let spirc = Arc::clone(&spirc);
        async move {
            tokio::select! {
                () = spirc_task => {
                    tracing::warn!("Integrated Spotify Connect task stopped; reconnecting the session");
                    player_event_task.abort();
                },
                result = &mut player_event_task => {
                    match result {
                        Ok(()) => tracing::warn!("Integrated player event channel closed; reconnecting the session"),
                        Err(err) => tracing::error!("Integrated player event task failed: {err:#}"),
                    }
                    if let Err(err) = spirc.shutdown() {
                        tracing::warn!("Failed to shut down stopped integrated player: {err:#}");
                    }
                }
            }
            // A stopped Connect task can leave a session valid but its device unavailable.
            session.shutdown();
        }
    });

    tracing::info!("New streaming connection has been established!");

    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::PauseGuard;
    use librespot_core::SpotifyUri;
    use librespot_playback::player::PlayerEvent;

    fn playing() -> PlayerEvent {
        PlayerEvent::Playing {
            play_request_id: 1,
            track_id: SpotifyUri::from_uri("spotify:track:3n3Ppam7vgaVa1iaRUc9Lp").unwrap(),
            position_ms: 1000,
        }
    }

    fn paused() -> PlayerEvent {
        PlayerEvent::Paused {
            play_request_id: 1,
            track_id: SpotifyUri::from_uri("spotify:track:3n3Ppam7vgaVa1iaRUc9Lp").unwrap(),
            position_ms: 1000,
        }
    }

    #[test]
    fn unsolicited_pause_is_reverted_even_when_repeated() {
        let mut guard = PauseGuard::default();
        assert!(!guard.handle_event(&playing()));
        assert!(guard.handle_event(&paused()));
        assert!(guard.handle_event(&paused()));
    }

    #[test]
    fn local_pause_is_accepted_and_does_not_allow_a_later_remote_pause() {
        let mut guard = PauseGuard::default();
        guard.handle_event(&playing());
        guard.allow_pause();
        assert!(!guard.handle_event(&paused()));
        guard.handle_event(&playing());
        assert!(guard.handle_event(&paused()));
    }

    #[test]
    fn pause_without_playback_is_not_resumed() {
        assert!(!PauseGuard::default().paused());
    }

    #[test]
    fn loading_and_shutdown_clear_previous_playback_and_pause_intent() {
        let mut guard = PauseGuard {
            playing: true,
            local_pause: true,
            ..Default::default()
        };
        guard.handle_event(&PlayerEvent::Loading {
            play_request_id: 2,
            track_id: SpotifyUri::from_uri("spotify:track:3n3Ppam7vgaVa1iaRUc9Lp").unwrap(),
            position_ms: 0,
        });
        assert!(!guard.handle_event(&paused()));
        guard.handle_event(&playing());
        assert!(guard.handle_event(&paused()));
    }

    #[test]
    fn queued_playing_event_does_not_erase_local_pause_intent() {
        let mut guard = PauseGuard {
            playing: true,
            ..Default::default()
        };
        guard.allow_pause();
        guard.handle_event(&playing());
        assert!(!guard.handle_event(&paused()));
    }

    #[test]
    fn no_op_local_pause_does_not_authorize_a_future_remote_pause() {
        let mut guard = PauseGuard::default();
        guard.allow_pause();
        guard.playing = true;
        assert!(guard.paused());
    }

    #[test]
    fn shutdown_pause_is_not_reverted() {
        let mut guard = PauseGuard {
            playing: true,
            shutting_down: true,
            ..Default::default()
        };
        guard.handle_event(&playing());
        assert!(!guard.handle_event(&paused()));
    }

    #[test]
    fn failed_transfer_does_not_cancel_a_newer_local_pause() {
        let mut guard = PauseGuard {
            playing: true,
            ..Default::default()
        };
        let transfer = guard.allow_pause();
        guard.allow_pause();
        guard.cancel_pause(transfer);
        assert!(!guard.handle_event(&paused()));
    }

    #[test]
    fn failed_transfer_does_not_authorize_a_remote_pause() {
        let mut guard = PauseGuard {
            playing: true,
            ..Default::default()
        };
        let transfer = guard.allow_pause();
        guard.cancel_pause(transfer);
        assert!(guard.handle_event(&paused()));
    }
}
