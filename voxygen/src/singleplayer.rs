use common::clock::Clock;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TryRecvError};
use server::{
    persistence::{DatabaseSettings, SqlLogMode},
    Error as ServerError, Event, Input, Server,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::runtime::Runtime;
use tracing::{info, trace, warn};

const TPS: u64 = 30;

/// Used to start and stop the background thread running the server
/// when in singleplayer mode.
pub struct Singleplayer {
    _server_thread: JoinHandle<()>,
    stop_server_s: Sender<()>,
    pub receiver: Receiver<Result<(), ServerError>>,
    // Wether the server is stopped or not
    paused: Arc<AtomicBool>,
    // Settings that the server was started with
    settings: server::Settings,
}

impl Singleplayer {
    pub fn new(runtime: &Arc<Runtime>) -> Self {
        let (stop_server_s, stop_server_r) = unbounded();

        // Determine folder to save server data in
        let server_data_dir = {
            let mut path = common_base::userdata_dir_workspace!();
            path.push("singleplayer");
            path
        };

        // Create server
        let settings = server::Settings::singleplayer(&server_data_dir);
        let editable_settings = server::EditableSettings::singleplayer(&server_data_dir);

        let settings2 = settings.clone();

        // Relative to data_dir
        const PERSISTENCE_DB_DIR: &str = "saves";

        let database_settings = DatabaseSettings {
            db_dir: server_data_dir.join(PERSISTENCE_DB_DIR),
            sql_log_mode: SqlLogMode::Disabled, /* Voxygen doesn't take in command-line arguments
                                                 * so SQL logging can't be enabled for
                                                 * singleplayer without changing this line
                                                 * manually */
        };

        let paused = Arc::new(AtomicBool::new(false));
        let paused1 = Arc::clone(&paused);

        let (result_sender, result_receiver) = bounded(1);

        let builder = thread::Builder::new().name("singleplayer-server-thread".into());
        let runtime = Arc::clone(runtime);
        let thread = builder
            .spawn(move || {
                trace!("starting singleplayer server thread");

                let (server, init_result) = match Server::new(
                    settings2,
                    editable_settings,
                    database_settings,
                    &server_data_dir,
                    runtime,
                ) {
                    Ok(server) => (Some(server), Ok(())),
                    Err(err) => (None, Err(err)),
                };

                match (result_sender.send(init_result), server) {
                    (Err(e), _) => warn!(
                        ?e,
                        "Failed to send singleplayer server initialization result. Most likely \
                         the channel was closed by cancelling server creation. Stopping Server"
                    ),
                    (Ok(()), None) => (),
                    (Ok(()), Some(server)) => run_server(server, stop_server_r, paused1),
                }

                trace!("ending singleplayer server thread");
            })
            .unwrap();

        Singleplayer {
            _server_thread: thread,
            stop_server_s,
            receiver: result_receiver,
            paused,
            settings,
        }
    }

    /// Returns reference to the settings the server was started with
    pub fn settings(&self) -> &server::Settings { &self.settings }

    /// Returns wether or not the server is paused
    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::SeqCst) }

    /// Pauses if true is passed and unpauses if false (Does nothing if in that
    /// state already)
    pub fn pause(&self, state: bool) { self.paused.store(state, Ordering::SeqCst); }
}

impl Drop for Singleplayer {
    fn drop(&mut self) {
        // Ignore the result
        let _ = self.stop_server_s.send(());
    }
}

fn run_server(mut server: Server, stop_server_r: Receiver<()>, paused: Arc<AtomicBool>) {
    info!("Starting server-cli...");

    // Set up an fps clock
    let mut clock = Clock::new(Duration::from_secs_f64(1.0 / TPS as f64));

    loop {
        // Check any event such as stopping and pausing
        match stop_server_r.try_recv() {
            Ok(()) => break,
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => (),
        }

        // Wait for the next tick.
        clock.tick();

        // Skip updating the server if it's paused
        if paused.load(Ordering::SeqCst) && server.number_of_players() < 2 {
            continue;
        } else if server.number_of_players() > 1 {
            paused.store(false, Ordering::SeqCst);
        }

        let events = server
            .tick(Input::default(), clock.dt())
            .expect("Failed to tick server!");

        for event in events {
            match event {
                Event::ClientConnected { .. } => info!("Client connected!"),
                Event::ClientDisconnected { .. } => info!("Client disconnected!"),
                Event::Chat { entity: _, msg } => info!("[Client] {}", msg),
            }
        }

        // Clean up the server after a tick.
        server.cleanup();
    }
}
