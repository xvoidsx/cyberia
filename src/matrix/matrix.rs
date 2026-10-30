use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::message::MessageType::File;

use crate::media::get_attachment_info;
use std::{fs, thread};

use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::time::Duration;

use anyhow::{Context, bail};
use futures::stream::StreamExt;
use log::{error, info};
use matrix_sdk::RoomState;
use matrix_sdk::attachment::AttachmentConfig;
use matrix_sdk::authentication::AuthSession;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::qrcode::{GeneratedQrProgress, GrantLoginProgress};
use matrix_sdk::authentication::oauth::registration::{
    ApplicationType, ClientMetadata, Localized, OAuthGrantType,
};
use matrix_sdk::authentication::oauth::{ClientId, OAuthSession, UserSession};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::encryption::EncryptionSettings;
use matrix_sdk::encryption::verification::{
    Emoji, SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::reqwest::Url;
use matrix_sdk::room::{MessagesOptions, Receipts, Room};
use matrix_sdk::ruma::api::Direction;
use matrix_sdk::ruma::api::client::directory::get_public_rooms_filtered;
use matrix_sdk::ruma::api::client::filter::{
    FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter,
};
use matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset;
use matrix_sdk::ruma::api::client::room::{Visibility, create_room};
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::member::{MembershipState, StrippedRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use matrix_sdk::ruma::exports::serde_json;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::utils::local_server::{LocalServerBuilder, LocalServerResponse};
use matrix_sdk::{Client, LoopCtrl, ServerName, SessionChange};
use rand::rng;
use rand::{RngExt, distr::Alphanumeric};

use matrix_sdk::ruma::events::relation::Annotation;
use matrix_sdk::ruma::events::room::message::MessageType::Audio;
use matrix_sdk::ruma::events::room::message::MessageType::Image;
use matrix_sdk::ruma::events::room::message::MessageType::Video;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, ForwardThread, ReplyWithinThread, RoomMessageEventContent, TextMessageEventContent,
};
use matrix_sdk::ruma::events::{
    AnyMessageLikeEvent, AnySyncEphemeralRoomEvent, AnySyncTimelineEvent, AnyTimelineEvent,
    EmptyStateKey, InitialStateEvent, MessageLikeEvent, SyncEphemeralRoomEvent,
};
use matrix_sdk::ruma::{
    OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedUserId, RoomOrAliasId, UInt,
};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::event::Event;
use crate::event::Event::Matui;
use crate::handler::MatuiEvent::{
    Error, ProgressComplete, ProgressStarted, VerificationCompleted, VerificationStarted,
};
use crate::handler::{Batch, MatuiEvent, SyncType};
use crate::matrix::roomcache::{DecoratedRoom, RoomCache};
use crate::spawn::{save_file, view_file};

use super::mime::mime_from_path;
use super::notify::Notify;

/// A Matrix client that maintains it's own Tokio runtime
#[derive(Clone)]
pub struct Matrix {
    /// The current client, if one is set. A plain slot (not a OnceCell):
    /// signing out and back in replaces the client in the same process,
    /// and a naive re-`init()` on a OnceCell would panic.
    client: Arc<Mutex<Option<Client>>>,
    room_cache: Arc<RoomCache>,
    notify: Arc<Notify>,
    /// Bumped whenever the session ends (logout, dead token). Background
    /// tasks from the old session (sync loop, session watcher) check this
    /// and stop instead of acting on a client that is no longer current.
    generation: Arc<AtomicU64>,
    /// True while a startup sync (init, fresh login, manual reconnect) is
    /// running, so a second one cannot start on top of it.
    startup_sync_in_flight: Arc<AtomicBool>,
    /// True while the forever sync loop is running, so a reconnect cannot
    /// start a second loop on top of it.
    sync_loop_running: Arc<AtomicBool>,
}

/// What should we do with the file after we download it?
pub enum AfterDownload {
    View,
    Save,
}

#[derive(Clone, Copy)]
pub enum Moderation {
    Ban,
    Unban,
    Kick,
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix {
            client: Arc::new(Mutex::new(None)),
            room_cache: Arc::new(RoomCache::default()),
            notify: Arc::new(Notify::default()),
            generation: Arc::new(AtomicU64::new(0)),
            startup_sync_in_flight: Arc::new(AtomicBool::new(false)),
            sync_loop_running: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Clears a Matrix in-flight flag when the task holding it ends, on every
/// path (including early returns).
struct FlagGuard(Arc<AtomicBool>);

impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Matrix {
    fn dirs() -> (PathBuf, PathBuf) {
        // cyberia is the new home; fall back to the old matui dir so existing
        // sessions (and their E2EE keys) migrate seamlessly.
        let base = dirs::data_dir().expect("no data directory found");
        let data_dir = base.join("cyberia");
        let data_dir = if !data_dir.exists() && base.join("matui").exists() {
            base.join("matui")
        } else {
            data_dir
        };

        let session_file = data_dir.join("session");
        (data_dir, session_file)
    }

    /// Where the log file lives: next to the session, so it is always
    /// findable. Mirrors dirs() (including the matui fallback) so the log
    /// and the session never disagree about the home.
    pub fn log_path() -> PathBuf {
        let (data_dir, _) = Self::dirs();
        data_dir.join("cyberia.log")
    }

    pub(crate) fn client_opt(&self) -> Option<Client> {
        self.client.lock().expect("to lock client").clone()
    }

    /// True while a startup sync (init, fresh login, manual reconnect) is
    /// running. Lets the UI keep a truthful state on screen when the last
    /// popup closes mid-sync instead of going blank.
    pub(crate) fn startup_sync_in_flight(&self) -> bool {
        self.startup_sync_in_flight.load(Ordering::SeqCst)
    }

    pub(crate) fn client(&self) -> Client {
        self.client_opt().expect("client expected but not set")
    }

    fn set_client(&self, client: Client) {
        *self.client.lock().expect("to lock client") = Some(client);
    }

    fn clear_client(&self) {
        *self.client.lock().expect("to lock client") = None;
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// End the current session's background work: the sync loop and the
    /// session watcher check this and stop on their next step.
    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Raw bytes for a media URI, for inline rendering. Uses the SDK cache.
    pub(crate) async fn media_bytes(&self, source: &MediaSource) -> Option<Vec<u8>> {
        let request = MediaRequestParameters {
            source: source.to_owned(),
            format: MediaFormat::File,
        };
        self.client()
            .media()
            .get_media_content(&request, true)
            .await
            .ok()
    }

    pub fn wrap_room(&self, room: &Room) -> Option<DecoratedRoom> {
        self.room_cache.wrap(room)
    }

    pub fn init(&self) {
        info!("initializing matrix");

        let matrix = self.clone();

        // Only one startup sync at a time: a second init (or a reconnect
        // pressed while init is still running) is a no-op.
        if matrix
            .startup_sync_in_flight
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        App::spawn(async move {
            let _guard = FlagGuard(matrix.startup_sync_in_flight.clone());
            matrix.init_inner().await;
        });
    }

    /// The body of init(), minus the in-flight guard: safe to call from the
    /// manual reconnect path too, when no client is set yet.
    async fn init_inner(&self) {
        let (_, session_file) = Matrix::dirs();

        if !session_file.exists() {
            App::send(MatuiEvent::LoginRequired);
            return;
        }

        App::send(MatuiEvent::SyncStarted(SyncType::Latest));

        let (client, token) = match restore_session(session_file.as_path()).await {
            Ok(tuple) => tuple,
            Err(err) => {
                App::send(Error(err.to_string()));
                return;
            }
        };

        info!("session restored");

        self.set_client(client.clone());

        self.watch_session(client.clone());

        info!("syncing with stored token: {}", token.is_some());

        if startup_sync(&client, token, &session_file).await {
            self.room_cache.populate(client).await;

            App::send(MatuiEvent::SyncComplete);
        }
    }

    /// Retry the connection on demand (Ctrl+R): re-run the startup sync on
    /// the existing client, or the whole init when no client is set yet
    /// (e.g. session restore failed). A no-op while a startup sync is
    /// already running.
    pub fn reconnect(&self) {
        let matrix = self.clone();

        if matrix
            .startup_sync_in_flight
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        App::spawn(async move {
            let _guard = FlagGuard(matrix.startup_sync_in_flight.clone());

            match matrix.client_opt() {
                Some(client) => {
                    let (_, session_file) = Matrix::dirs();
                    let token = read_sync_token(&session_file);

                    if startup_sync(&client, token, &session_file).await {
                        matrix.room_cache.populate(client.clone()).await;
                        App::send(MatuiEvent::ReconnectComplete);
                    }
                }
                // No client yet (restore failed): full init is safe here,
                // the slot it would fill is still empty.
                None => matrix.init_inner().await,
            }
        });
    }

    pub fn login(&self, homeserver: &str, username: &str, password: &str) {
        let (data_dir, session_file) = Matrix::dirs();
        let homeserver = homeserver.to_string();
        let user = username.to_string();
        let pass = password.to_string();
        let matrix = self.clone();

        App::spawn(async move {
            App::send(MatuiEvent::LoginStarted);

            match login(&data_dir, &session_file, &homeserver, &user, &pass).await {
                Ok(client) => matrix.finish_signin(client, session_file).await,
                Err(err) => App::send(Error(err.to_string())),
            }
        });
    }

    /// Start the login flow by deciding whether this homeserver supports
    /// OIDC. OIDC homeservers go straight to browser login; everything else
    /// asks for username/password.
    pub fn start_login(&self, homeserver: String) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Checking homeserver.".to_string(), 0));

            match supports_oidc(&homeserver).await {
                Ok(true) => matrix.login_oauth(homeserver),
                Ok(false) => App::send(MatuiEvent::PasswordLoginRequired(homeserver)),
                Err(err) => App::send(Error(err.to_string())),
            }
        });
    }

    pub fn grant_login_with_qr(&self) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Preparing QR login.".to_string(), 0));

            let client = matrix.client();
            let oauth = client.oauth();
            let grant = oauth.grant_login_with_qr_code().generate();
            let mut progress = grant.subscribe_to_progress();

            let progress_task = tokio::spawn(async move {
                while let Some(state) = progress.next().await {
                    match state {
                        GrantLoginProgress::Starting => {}
                        GrantLoginProgress::EstablishingSecureChannel(
                            GeneratedQrProgress::QrReady(data),
                        ) => match render_qr_code(data.to_bytes()) {
                            Ok(code) => App::send(MatuiEvent::QrLoginCode(code)),
                            Err(err) => App::send(Error(err.to_string())),
                        },
                        GrantLoginProgress::EstablishingSecureChannel(
                            GeneratedQrProgress::QrScanned(sender),
                        ) => App::send(MatuiEvent::QrLoginScanned(sender)),
                        GrantLoginProgress::WaitingForAuth {
                            verification_uri,
                            continuation_sender,
                        } => {
                            let url = verification_uri.to_string();
                            let _ = open::that_detached(&url);
                            App::send(MatuiEvent::QrLoginAuth(url));

                            if let Err(err) = continuation_sender.confirm().await {
                                App::send(Error(err.to_string()));
                            }
                        }
                        GrantLoginProgress::SyncingSecrets => {
                            App::send(ProgressStarted("Syncing secrets.".to_string(), 0));
                        }
                        GrantLoginProgress::Done => {}
                    }
                }
            });

            match grant.await {
                Ok(()) => App::send(MatuiEvent::QrLoginDone),
                Err(err) => App::send(Error(err.to_string())),
            }

            progress_task.abort();
        });
    }

    pub fn login_oauth(&self, homeserver: String) {
        let (data_dir, session_file) = Matrix::dirs();
        let matrix = self.clone();

        App::spawn(async move {
            App::send(MatuiEvent::LoginStarted);

            match login_oauth_flow(&data_dir, &session_file, &homeserver).await {
                Ok(Some(client)) => matrix.finish_signin(client, session_file).await,
                // cancelled from the waiting popup; back to the form
                Ok(None) => App::send(MatuiEvent::LoginRequired),
                Err(err) => App::send(Error(err.to_string())),
            }
        });
    }

    /// Everything that happens after a fresh login, no matter the flow.
    async fn finish_signin(&self, client: Client, session_file: PathBuf) {
        self.set_client(client.clone());

        self.watch_session(client.clone());

        App::send(MatuiEvent::LoginComplete);
        App::send(MatuiEvent::SyncStarted(SyncType::Initial));

        if !startup_sync(&client, None, &session_file).await {
            return;
        }

        self.room_cache.populate(client.clone()).await;
        App::send(MatuiEvent::SyncComplete);
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        self.verify();
    }

    /// Keep the session file up to date as tokens refresh (OAuth access
    /// tokens are short-lived).
    fn watch_session(&self, client: Client) {
        let matrix = self.clone();
        let generation = self.generation();

        App::spawn(async move {
            let mut changes = client.subscribe_to_session_changes();

            while let Ok(change) = changes.recv().await {
                // The session ended (logout, dead token) while this watcher
                // was running: stop, the new session has its own watcher.
                if matrix.generation() != generation {
                    break;
                }

                match change {
                    SessionChange::TokensRefreshed => {
                        let (_, session_file) = Matrix::dirs();

                        if let Err(err) = persist_auth(&session_file, &client) {
                            error!("could not persist refreshed tokens: {}", err);
                        }
                    }
                    // The server no longer knows this token: the session is
                    // dead. Set it aside and route to sign-in, never a
                    // blank window with a dead client behind it.
                    SessionChange::UnknownToken(_) => {
                        info!("session expired (unknown token)");

                        matrix.bump_generation();
                        matrix.clear_client();
                        matrix.room_cache.clear();

                        let (_, session_file) = Matrix::dirs();
                        recover_dead_session(&session_file);
                        break;
                    }
                }
            }
        });
    }

    pub fn verify(&self) {
        let matrix = self.clone();

        App::spawn(async move {
            let client = matrix.client();

            if let Some(user_id) = client.user_id() {
                match client.encryption().get_user_identity(user_id).await {
                    Ok(Some(identity)) => {
                        match identity
                            .request_verification_with_methods(vec![VerificationMethod::SasV1])
                            .await
                        {
                            Err(err) => error!("could not request verification: {}", err),
                            Ok(request) => matrix.wait_on_verify_request(request),
                        }
                    }
                    Ok(None) => error!("no user identity"),
                    Err(err) => error!("could not get user identity: {}", err),
                };
            };
        });
    }

    fn wait_on_verify_request(&self, request: VerificationRequest) {
        info!("verification requested");

        App::spawn(async move {
            let mut stream = request.changes();

            while let Some(state) = stream.next().await {
                if let VerificationRequestState::Transitioned {
                    verification: Verification::SasV1(s),
                } = state
                {
                    App::spawn(sas_verification_handler(s, App::get_sender()));
                    break;
                }
            }
        });
    }

    pub fn recover(&self, key: &str) {
        let matrix = self.clone();
        let key = key.to_string();

        App::spawn(async move {
            match matrix.client().encryption().recovery().recover(&key).await {
                Ok(()) => App::send(MatuiEvent::ProgressComplete),
                Err(_) => App::send(MatuiEvent::Error(
                    "Could not recover encryption keys.".to_string(),
                )),
            }
        });
    }

    pub fn sync(&self) {
        // Never run two forever loops: a reconnect that lands while the
        // old loop is still alive just refreshes the rooms, it must not
        // double the timeline stream.
        if self.sync_loop_running.swap(true, Ordering::SeqCst) {
            return;
        }

        let client = self.client();

        add_default_handlers(client.clone());
        add_verification_handlers(client.clone());

        // apparently we only need the token for sync_once
        let sync_settings = build_sync_settings(None);

        let generation_flag = self.generation.clone();
        let generation = self.generation();
        let loop_flag = self.sync_loop_running.clone();

        App::spawn(async move {
            let _guard = FlagGuard(loop_flag);

            // prompt for any invites that arrived while we weren't running
            for room in client.invited_rooms() {
                send_invite_event(room).await;
            }

            let result = client
                .sync_with_result_callback(sync_settings, |sync_result| async {
                    // The session ended (logout, dead token) while this
                    // loop was running: stop instead of polling the server
                    // forever with a token it no longer knows.
                    if generation_flag.load(Ordering::SeqCst) != generation {
                        return Ok(LoopCtrl::Break);
                    }

                    let response = match sync_result {
                        Ok(resp) => resp,
                        Err(err) => {
                            error!("no sync result: {}", err);
                            return Ok(LoopCtrl::Continue);
                        }
                    };

                    let (_, session_file) = Matrix::dirs();

                    // We persist the token each time to keep the disk up-to-date
                    if let Err(err) = persist_sync_token(&session_file, response.next_batch) {
                        error!("could not persist sync token {}", err)
                    }

                    Ok(LoopCtrl::Continue)
                })
                .await;

            // A loop superseded by logout or re-login stays silent: its
            // error is expected, and a popup over the sign-in form would
            // be a lie about the new session.
            if let Err(err) = result
                && generation_flag.load(Ordering::SeqCst) == generation
            {
                error!("sync loop ended: {}", err);
                App::send(Error(format!("Sync failed: {}", err)));
            }
        });
    }

    pub fn confirm_verification(&self, sas: SasVerification) {
        App::spawn(async move {
            if let Err(err) = sas.confirm().await {
                error!("could not verify: {}", err);
                App::send(Error(format!("Could not verify: {}", err)));
            }
        });
    }

    pub fn mismatched_verification(&self, sas: SasVerification) {
        App::spawn(async move {
            if let Err(err) = sas.mismatch().await {
                error!("could not cancel SAS verification: {}", err)
            } else {
                info!("verification has been cancelled")
            }
        });
    }

    pub fn fetch_rooms(&self) -> Vec<DecoratedRoom> {
        self.room_cache.get_rooms()
    }

    pub fn fetch_messages(&self, room: Room, cursor: Option<String>, limit: usize) {
        App::spawn(async move {
            // fetch the actual messages
            let mut options = MessagesOptions::new(Direction::Backward);
            options.limit =
                UInt::new(limit.try_into().unwrap_or(25)).expect("invalid paging limit");
            options.from = cursor;

            let messages = match room.messages(options).await {
                Ok(msg) => msg,
                Err(err) => {
                    App::send(Error(err.to_string()));
                    return;
                }
            };

            let unpacked: Vec<AnyTimelineEvent> = messages
                .chunk
                .iter()
                .filter_map(|te| {
                    Matrix::deserialize_event(te, room.room_id().into())
                        .map_err(|err| error!("could not deserialize event: {}", err))
                        .ok()
                })
                .collect();

            let batch = Batch {
                room: room.clone(),
                events: unpacked,
                cursor: messages.end,
            };

            App::send(MatuiEvent::TimelineBatch(batch));
        });
    }

    pub fn fetch_room_member(&self, room: Room, id: OwnedUserId) {
        App::spawn(async move {
            match room.get_member(&id).await {
                Ok(Some(member)) => App::send(MatuiEvent::RoomMember(room, member)),
                Ok(None) => info!("no such room member: {}", id),
                Err(err) => error!("could not fetch room member {}: {}", id, err),
            }
        });
    }

    pub fn download_content(&self, message: MessageType, after: AfterDownload) {
        let matrix = self.clone();
        let octets = "application/octet-stream".to_string();

        App::spawn(async move {
            App::send(ProgressStarted("Downloading file.".to_string(), 250));

            let (content_type, request, file_name) = match message {
                Image(content) => {
                    let file_name = content.filename().to_string();
                    (
                        content
                            .info
                            .and_then(|info| info.mimetype)
                            .unwrap_or(octets),
                        MediaRequestParameters {
                            source: content.source,
                            format: MediaFormat::File,
                        },
                        file_name,
                    )
                }
                Video(content) => {
                    let file_name = content.filename().to_string();
                    (
                        content
                            .info
                            .and_then(|info| info.mimetype)
                            .unwrap_or(octets),
                        MediaRequestParameters {
                            source: content.source,
                            format: MediaFormat::File,
                        },
                        file_name,
                    )
                }
                Audio(content) => {
                    let file_name = content.filename().to_string();
                    (
                        content
                            .info
                            .and_then(|info| info.mimetype)
                            .unwrap_or(octets),
                        MediaRequestParameters {
                            source: content.source,
                            format: MediaFormat::File,
                        },
                        file_name,
                    )
                }
                File(content) => {
                    let file_name = content.filename().to_string();
                    (
                        content
                            .info
                            .and_then(|info| info.mimetype)
                            .unwrap_or(octets),
                        MediaRequestParameters {
                            source: content.source,
                            format: MediaFormat::File,
                        },
                        file_name,
                    )
                }
                _ => {
                    App::send(Error("Unknown file type.".to_string()));
                    return;
                }
            };

            let handle = match matrix
                .client()
                .media()
                .get_media_file(&request, None, &content_type.parse().unwrap(), true, None)
                .await
            {
                Err(err) => {
                    App::send(Error(err.to_string()));
                    return;
                }
                Ok(mfh) => mfh,
            };

            App::send(ProgressComplete);

            match after {
                AfterDownload::View => {
                    tokio::task::spawn_blocking(move || view_file(handle));
                }
                AfterDownload::Save => match save_file(handle, &file_name).await {
                    Err(err) => App::send(Error(err.to_string())),
                    Ok(None) => {}
                    Ok(Some(path)) => App::send(MatuiEvent::Confirm(
                        "Download Complete".to_string(),
                        format!("Saved to {}", path.to_str().unwrap()),
                    )),
                },
            };
        });
    }

    pub fn send_text_message(&self, room: Room, message: String) {
        App::spawn(async move {
            App::send(ProgressStarted("Sending message.".to_string(), 500));

            if let Err(err) = room
                .send(RoomMessageEventContent::text_markdown(message))
                .await
            {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn send_reply(&self, room: Room, message: String, in_reply_to: OwnedEventId) {
        App::spawn(async move {
            App::send(ProgressStarted("Sending message.".to_string(), 500));

            let in_reply_to = match Matrix::get_room_event(&room, &in_reply_to).await {
                Some(e) => e,
                None => {
                    App::send(Error("Could not find reply event.".to_string()));
                    return;
                }
            };

            let Some(og_in_reply_to) = in_reply_to.as_original() else {
                return;
            };

            let reply = RoomMessageEventContent::text_markdown(message).make_reply_to(
                og_in_reply_to,
                ForwardThread::Yes,
                AddMentions::No,
            );

            if let Err(err) = room.send(reply).await {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn send_thread_message(
        &self,
        room: Room,
        message: String,
        target: OwnedEventId,
        is_reply: ReplyWithinThread,
    ) {
        App::spawn(async move {
            App::send(ProgressStarted("Sending message.".to_string(), 500));

            let target = match Matrix::get_room_event(&room, &target).await {
                Some(e) => e,
                None => {
                    App::send(Error("Could not find thread event.".to_string()));
                    return;
                }
            };

            let Some(og_target) = target.as_original() else {
                return;
            };

            let reply = RoomMessageEventContent::text_markdown(message).make_for_thread(
                og_target,
                is_reply,
                AddMentions::No,
            );

            if let Err(err) = room.send(reply).await {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn send_attachments(&self, room: Room, attachments: Vec<(PathBuf, Option<String>)>) {
        let total = attachments.len();

        App::spawn(async move {
            for (i, (path, caption)) in attachments.into_iter().enumerate() {
                App::send(ProgressStarted(
                    format!("Uploading {} of {}.", i + 1, total),
                    0,
                ));

                let content_type = mime_from_path(&path);

                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default()
                    .to_string();

                let data = match fs::read(path.to_str().unwrap()) {
                    Ok(d) => d,
                    Err(err) => {
                        App::send(Error(err.to_string()));
                        return;
                    }
                };

                // try to grab a thumbnail
                let config = match get_attachment_info(&path, &content_type) {
                    Ok((thumbnail, info)) => {
                        AttachmentConfig::new().thumbnail(thumbnail).info(info)
                    }
                    _ => AttachmentConfig::new(),
                }
                .caption(caption.map(TextMessageEventContent::plain));

                if let Err(err) = room
                    .send_attachment(&name, &content_type, data, config)
                    .await
                {
                    App::send(Error(err.to_string()));
                }

                App::send(ProgressComplete);
            }
        });
    }

    pub fn send_reaction(&self, room: Room, event_id: OwnedEventId, key: String) {
        App::spawn(async move {
            App::send(ProgressStarted("Sending reaction.".to_string(), 500));

            if let Err(err) = room
                .send(ReactionEventContent::new(Annotation::new(event_id, key)))
                .await
            {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn redact_event(&self, room: Room, event_id: OwnedEventId) {
        App::spawn(async move {
            App::send(ProgressStarted("Removing.".to_string(), 500));

            if let Err(err) = room.redact(&event_id, None, None).await {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn join_room(&self, room: Room) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Joining.".to_string(), 500));

            match room.join().await {
                Ok(_) => {
                    matrix.room_cache.add_room(room.clone()).await;
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::RoomSelected(room));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn join_room_by_target(&self, target: OwnedRoomOrAliasId) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Joining.".to_string(), 500));

            match matrix.client().join_room_by_id_or_alias(&target, &[]).await {
                Ok(room) => {
                    matrix.room_cache.add_room(room.clone()).await;
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::RoomSelected(room));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn join_room_by_name(&self, name: String) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Joining.".to_string(), 500));

            // a single token might be an alias on our own server
            if !name.contains(char::is_whitespace)
                && let Ok(alias) =
                    RoomOrAliasId::parse(format!("#{}:{}", name, matrix.me().server_name()))
                && let Ok(room) = matrix.client().join_room_by_id_or_alias(&alias, &[]).await
            {
                matrix.room_cache.add_room(room.clone()).await;
                App::send(ProgressComplete);
                App::send(MatuiEvent::RoomSelected(room));
                return;
            }

            // otherwise, let the server search its public room directory
            let mut request = get_public_rooms_filtered::v3::Request::new();
            request.limit = Some(1u32.into());
            request.filter.generic_search_term = Some(name.clone());

            match matrix.client().public_rooms_filtered(request).await {
                Ok(response) => match response.chunk.into_iter().next() {
                    Some(chunk) => {
                        App::send(ProgressComplete);
                        App::send(MatuiEvent::RoomFound(chunk));
                    }
                    None => {
                        App::send(ProgressComplete);
                        App::send(Error(format!(
                            "No public room found matching \"{}\".",
                            name
                        )));
                    }
                },
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn leave_room(&self, room: Room, forget: bool) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Leaving.".to_string(), 500));

            match room.leave().await {
                Ok(_) => {
                    matrix.room_cache.remove_room(&room);
                    App::send(ProgressComplete);

                    if forget && let Err(err) = room.forget().await {
                        App::send(Error(err.to_string()));
                    }

                    App::send(MatuiEvent::RoomLeft(room));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    /// Log out on the server (invalidating this device, like Element's
    /// "remove session") and wipe all local state, then return to the
    /// sign-in form instead of quitting.
    pub fn logout(&self) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Logging out.".to_string(), 500));

            // A logout with no client (nothing to invalidate server-side)
            // still wipes local state and returns to sign-in.
            if let Some(client) = matrix.client_opt()
                && let Err(err) = client.logout().await
            {
                App::send(ProgressComplete);
                App::send(Error(err.to_string()));
                return;
            }

            let (data_dir, _) = Matrix::dirs();

            // A failed wipe leaves the session alive: say so and stay.
            if let Err(err) = fs::remove_dir_all(&data_dir) {
                App::send(ProgressComplete);
                App::send(Error(err.to_string()));
                return;
            }

            // Stop the old session's background work now that its files
            // are gone, and drop the client so a fresh sign-in can set a
            // new one. No await between the wipe and this, so the loop
            // cannot recreate anything in between.
            matrix.bump_generation();
            matrix.clear_client();
            matrix.room_cache.clear();

            App::send(ProgressComplete);
            App::send(MatuiEvent::LoggedOut);
        });
    }

    pub fn create_room(
        &self,
        name: Option<String>,
        topic: Option<String>,
        alias: Option<String>,
        encrypted: bool,
        private: bool,
    ) {
        let matrix = self.clone();

        App::spawn(async move {
            App::send(ProgressStarted("Creating.".to_string(), 500));

            let mut request = create_room::v3::Request::new();
            request.name = name;
            request.topic = topic;
            request.room_alias_name = alias;

            if private {
                request.preset = Some(RoomPreset::PrivateChat);
            } else {
                request.preset = Some(RoomPreset::PublicChat);
                request.visibility = Visibility::Public;
            }

            if encrypted {
                request.initial_state = vec![
                    InitialStateEvent::new(
                        EmptyStateKey,
                        RoomEncryptionEventContent::with_recommended_defaults(),
                    )
                    .to_raw_any(),
                ];
            }

            match matrix.client().create_room(request).await {
                Ok(room) => {
                    matrix.room_cache.add_room(room.clone()).await;
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::RoomSelected(room));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn create_dm(&self, user_id: OwnedUserId, encrypted: bool) {
        let matrix = self.clone();

        App::spawn(async move {
            // if we already have a DM with this user, just open it
            if let Some(room) = matrix.client().get_dm_room(&user_id) {
                App::send(MatuiEvent::RoomSelected(room));
                return;
            }

            App::send(ProgressStarted("Creating.".to_string(), 500));

            let result = if encrypted {
                matrix.client().create_dm(&user_id).await
            } else {
                // create_dm, minus the encryption state event
                let mut request = create_room::v3::Request::new();
                request.invite = vec![user_id.clone()];
                request.is_direct = true;
                request.preset = Some(RoomPreset::TrustedPrivateChat);

                matrix.client().create_room(request).await
            };

            match result {
                Ok(room) => {
                    matrix.room_cache.add_room(room.clone()).await;
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::RoomSelected(room));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn invite_user(&self, room: Room, user_id: OwnedUserId) {
        App::spawn(async move {
            App::send(ProgressStarted("Inviting.".to_string(), 500));

            match room.invite_user_by_id(&user_id).await {
                Ok(_) => {
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::Confirm(
                        "Invited".to_string(),
                        format!("{} has been invited.", user_id),
                    ));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn moderate(
        &self,
        room: Room,
        action: Moderation,
        user_id: OwnedUserId,
        reason: Option<String>,
    ) {
        App::spawn(async move {
            let (verb, past) = match action {
                Moderation::Ban => ("Banning", "Banned"),
                Moderation::Unban => ("Unbanning", "Unbanned"),
                Moderation::Kick => ("Kicking", "Kicked"),
            };

            App::send(ProgressStarted(format!("{}.", verb), 500));

            let result = match action {
                Moderation::Ban => room.ban_user(&user_id, reason.as_deref()).await,
                Moderation::Unban => room.unban_user(&user_id, reason.as_deref()).await,
                Moderation::Kick => room.kick_user(&user_id, reason.as_deref()).await,
            };

            match result {
                Ok(_) => {
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::Confirm(
                        past.to_string(),
                        format!("{} has been {}.", user_id, past.to_lowercase()),
                    ));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    pub fn ignore_user(&self, user_id: OwnedUserId, ignore: bool) {
        let matrix = self.clone();

        App::spawn(async move {
            let (verb, past) = if ignore {
                ("Ignoring", "Ignored")
            } else {
                ("Unignoring", "Unignored")
            };

            App::send(ProgressStarted(format!("{}.", verb), 500));

            let account = matrix.client().account();

            let result = if ignore {
                account.ignore_user(&user_id).await
            } else {
                account.unignore_user(&user_id).await
            };

            match result {
                Ok(_) => {
                    App::send(ProgressComplete);
                    App::send(MatuiEvent::Confirm(
                        past.to_string(),
                        format!("{} has been {}.", user_id, past.to_lowercase()),
                    ));
                }
                Err(err) => {
                    App::send(ProgressComplete);
                    App::send(Error(err.to_string()));
                }
            }
        });
    }

    async fn get_room_event(
        room: &Room,
        id: &OwnedEventId,
    ) -> Option<MessageLikeEvent<RoomMessageEventContent>> {
        match room.event(id, Option::None).await {
            Ok(event) => Matrix::get_room_message_event(room, &event),
            Err(err) => {
                error!("could not get room event: {}", err);
                None
            }
        }
    }

    pub fn deserialize_event(
        event: &TimelineEvent,
        room_id: OwnedRoomId,
    ) -> anyhow::Result<AnyTimelineEvent> {
        match &event.kind {
            TimelineEventKind::Decrypted(decrypted) => Ok(decrypted.event.deserialize()?),
            TimelineEventKind::PlainText { event } => {
                Ok(event.deserialize()?.into_full_event(room_id))
            }
            TimelineEventKind::UnableToDecrypt { event, .. } => {
                Ok(event.deserialize()?.into_full_event(room_id))
            }
        }
    }

    pub fn get_room_message_event(
        room: &Room,
        event: &TimelineEvent,
    ) -> Option<MessageLikeEvent<RoomMessageEventContent>> {
        let Ok(event) = Matrix::deserialize_event(event, room.room_id().to_owned()) else {
            return None;
        };

        let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(event)) = event else {
            return None;
        };

        Some(event)
    }

    pub fn replace_event(
        &self,
        room: Room,
        id: OwnedEventId,
        message: String,
        in_reply_to: Option<OwnedEventId>,
    ) {
        App::spawn(async move {
            App::send(ProgressStarted("Editing message.".to_string(), 500));

            let Some(event) = Matrix::get_room_event(&room, &id).await else {
                return;
            };

            let Some(event) = event.as_original() else {
                return;
            };

            let reply_event = match in_reply_to {
                Some(id) => Matrix::get_room_event(&room, &id).await,
                None => None,
            };

            let reply_event = match reply_event {
                Some(e) => e.as_original().cloned(),
                None => None,
            };

            info!("reply event: {:?}", reply_event);

            if let Err(err) = room
                .send(RoomMessageEventContent::text_markdown(message).make_replacement(event))
                .await
            {
                App::send(Error(err.to_string()));
            }

            App::send(ProgressComplete);
        });
    }

    pub fn me(&self) -> OwnedUserId {
        self.client().user_id().unwrap().to_owned()
    }

    pub fn timeline_event(&self, event: AnyTimelineEvent) {
        let matrix = self.clone();

        App::spawn(async move {
            matrix
                .room_cache
                .timeline_event(matrix.client(), &event)
                .await;

            if let Err(e) = matrix.notify.timeline_event(matrix.client(), event).await {
                error!("could not send notification: {}", e);
            }
        });
    }

    pub fn focus_event(&self) {
        self.notify.focus_event();
    }

    pub fn blur_event(&self) {
        self.notify.blur_event();
    }

    pub fn room_visit_event(&self, room: Room) {
        self.notify.room_visit_event(room.clone());
        self.room_cache.room_visit_event(room);
    }

    pub fn read_to(&self, room: Room, to: OwnedEventId) {
        let receipts = Receipts::new()
            .fully_read_marker(Some(to.clone()))
            .public_read_receipt(Some(to));

        App::spawn(async move {
            if let Err(e) = room.send_multiple_receipts(receipts).await {
                error!("could not send read receipt: {}", e);
            }
        });
    }

    pub fn typing_notification(&self, room: Room, typing: bool) {
        App::spawn(async move {
            if let Err(e) = room.typing_notice(typing).await {
                error!("could not send typing notice: {}", e);
            }
        });
    }

    pub fn begin_typing(&self, room: Room) -> Sender<()> {
        let (send, recv) = channel();
        let matrix = self.clone();

        thread::spawn(move || {
            while let Err(TryRecvError::Empty) = recv.try_recv() {
                matrix.typing_notification(room.clone(), true);
                thread::sleep(Duration::from_millis(1000));
            }
        });

        send
    }

    pub fn end_typing(&self, room: Room, send: Sender<()>) {
        send.send(()).expect("could not stop thread");
        self.typing_notification(room, false);
    }
}

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    homeserver: String,
    db_path: PathBuf,
    passphrase: String,
}

/// The user's auth info, for either kind of login.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredAuth {
    // OAuth must come first: untagged deserializing tries variants in
    // order, and only OAuth sessions have a client_id
    OAuth {
        client_id: ClientId,
        user: UserSession,
    },
    Matrix(MatrixSession),
}

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    client_session: ClientSession,
    user_session: StoredAuth,
    sync_token: Option<String>,
}

fn stored_auth(client: &Client) -> anyhow::Result<StoredAuth> {
    match client.session().context("Your user has no session.")? {
        AuthSession::Matrix(session) => Ok(StoredAuth::Matrix(session)),
        AuthSession::OAuth(session) => Ok(StoredAuth::OAuth {
            client_id: session.client_id,
            user: session.user,
        }),
        _ => bail!("Unknown session type."),
    }
}

fn persist_session(
    session_file: &Path,
    client_session: ClientSession,
    client: &Client,
) -> anyhow::Result<()> {
    let serialized_session = serde_json::to_string(&FullSession {
        client_session,
        user_session: stored_auth(client)?,
        sync_token: None,
    })?;

    fs::write(session_file, serialized_session)?;

    Ok(())
}

/// Update just the auth portion of an existing session file.
fn persist_auth(session_file: &Path, client: &Client) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file)?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.user_session = stored_auth(client)?;
    fs::write(session_file, serde_json::to_string(&full_session)?)?;

    Ok(())
}

async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, Option<String>)> {
    let serialized_session = fs::read_to_string(session_file)?;

    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized_session)?;

    let homeserver = <&ServerName>::try_from(client_session.homeserver.as_str())?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .server_name(homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .with_encryption_settings(build_encryption_settings())
        .handle_refresh_tokens()
        .build()
        .await?;

    // Restore the user session.
    let session: AuthSession = match user_session {
        StoredAuth::OAuth { client_id, user } => OAuthSession { client_id, user }.into(),
        StoredAuth::Matrix(session) => session.into(),
    };

    client.restore_session(session).await?;

    Ok((client, sync_token))
}

async fn login(
    data_dir: &Path,
    session_file: &Path,
    homeserver: &str,
    username: &str,
    password: &str,
) -> anyhow::Result<Client> {
    let server = ServerName::parse(homeserver)?;

    // the username can be bare, or a full matrix ID
    let username = username
        .strip_prefix('@')
        .unwrap_or(username)
        .split(':')
        .next()
        .unwrap_or_default();

    let (client, client_session) = build_client(data_dir, &server).await?;

    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("Cyberia")
        .send()
        .await?;

    persist_session(session_file, client_session, &client)?;

    Ok(client)
}

fn render_qr_code(data: Vec<u8>) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(data)?;

    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

async fn supports_oidc(homeserver: &str) -> anyhow::Result<bool> {
    let server = ServerName::parse(homeserver)?;
    let client = Client::builder()
        .server_name(&server)
        .build()
        .await
        .with_context(|| format!("could not connect to {homeserver}"))?;

    match client.oauth().server_metadata().await {
        Ok(_) => Ok(true),
        Err(err) if err.is_not_supported() => Ok(false),
        Err(err) => bail!("could not check {homeserver} for OIDC: {err}"),
    }
}

/// Run the browser-based OAuth login flow. Returns None if the user
/// cancelled from the waiting popup.
async fn login_oauth_flow(
    data_dir: &Path,
    session_file: &Path,
    homeserver: &str,
) -> anyhow::Result<Option<Client>> {
    let server = ServerName::parse(homeserver)?;
    let (client, client_session) = build_client(data_dir, &server).await?;

    let (redirect_url, redirect) = LocalServerBuilder::new()
        .response(LocalServerResponse::Html(
            "<h3>You're signed in! You can close this tab and return to your terminal.</h3>"
                .to_string(),
        ))
        .spawn()
        .await?;

    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode {
            redirect_uris: vec![redirect_url.clone()],
        }],
        Localized::new(Url::parse("https://github.com/xvoidsx/cyberia-matui")?, []),
    );

    metadata.client_name = Some(Localized::new("Cyberia".to_string(), []));

    let oauth = client.oauth();

    let auth_data = oauth
        .login(redirect_url, None, Some(Raw::new(&metadata)?.into()), None)
        .build()
        .await?;

    // best effort: the waiting popup has fallbacks if this doesn't work
    let _ = open::that_detached(auth_data.url.as_str());

    let cancel = Arc::new(tokio::sync::Notify::new());

    App::send(MatuiEvent::OauthStarted(
        auth_data.url.to_string(),
        cancel.clone(),
    ));

    let query = tokio::select! {
        query = redirect.into_future() => query,
        _ = cancel.notified() => {
            oauth.abort_login(&auth_data.state).await;
            return Ok(None);
        }
    };

    let query = query.context("The local redirect server was shut down.")?;

    oauth.finish_login(query.into()).await?;

    persist_session(session_file, client_session, &client)?;

    Ok(Some(client))
}

async fn build_client(
    data_dir: &Path,
    server: &ServerName,
) -> anyhow::Result<(Client, ClientSession)> {
    let db_subfolder: String = (&mut rng())
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();

    let db_path = data_dir.join(db_subfolder.as_str());

    // Generate a random passphrase.
    let passphrase: String = (&mut rng())
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    let client = Client::builder()
        .server_name(server)
        .sqlite_store(&db_path, Some(passphrase.as_str()))
        .with_encryption_settings(build_encryption_settings())
        .handle_refresh_tokens()
        .build()
        .await?;

    Ok((
        client,
        ClientSession {
            homeserver: server.as_str().to_string(),
            db_path,
            passphrase,
        },
    ))
}

fn build_sync_settings(sync_token: Option<String>) -> SyncSettings {
    let mut state_filter = RoomEventFilter::empty();
    state_filter.lazy_load_options = LazyLoadOptions::Enabled {
        include_redundant_members: false,
    };

    let mut room_filter = RoomFilter::empty();
    room_filter.state = state_filter;

    let mut filter = FilterDefinition::empty();
    filter.room = room_filter;

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    if let Some(token) = sync_token {
        sync_settings = sync_settings.token(token);
    }

    sync_settings
}

fn build_encryption_settings() -> EncryptionSettings {
    EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        backup_download_strategy: matrix_sdk::encryption::BackupDownloadStrategy::OneShot,
    }
}

/// Why an initial sync failed, classified so the caller can recover
/// instead of showing a dead-end popup.
enum InitialSyncError {
    /// The session is dead (unknown/expired token). The only recovery is
    /// signing in again.
    Auth(anyhow::Error),
    /// Anything else, after every retry. The server may come back, so the
    /// user can retry.
    Transient(anyhow::Error),
}

impl std::fmt::Display for InitialSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitialSyncError::Auth(err) | InitialSyncError::Transient(err) => err.fmt(f),
        }
    }
}

async fn sync_once(
    client: Client,
    sync_token: Option<String>,
    session_file: &Path,
) -> Result<String, InitialSyncError> {
    let sync_settings = build_sync_settings(sync_token);

    let mut last_error: Option<matrix_sdk::Error> = None;

    for _ in 0..10 {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                persist_sync_token(session_file, response.next_batch.clone())
                    .map_err(InitialSyncError::Transient)?;
                return Ok(response.next_batch);
            }
            Err(error) => {
                // A dead token will never come back: stop retrying at once
                // and let the caller route to sign-in.
                if matches!(
                    error.client_api_error_kind(),
                    Some(matrix_sdk::ruma::api::error::ErrorKind::UnknownToken(_))
                ) {
                    return Err(InitialSyncError::Auth(error.into()));
                }

                info!("An error occurred during initial sync: {error}");
                info!("Trying again…");
                last_error = Some(error);
            }
        }
    }

    Err(InitialSyncError::Transient(anyhow::anyhow!(
        "Sync failed after 10 attempts: {}",
        last_error.map(|e| e.to_string()).unwrap_or_else(|| "no sync attempt made".to_string())
    )))
}

/// Run the initial sync with recovery, sending progress and recovery
/// events as it goes. Returns true on success.
///
/// - Auth failure: the dead session file is renamed to `.bak` and the
///   sign-in popup is shown, never a blank window.
/// - Transient failure: retries with growing backoff (1s, doubling to a
///   30s cap) behind a visible "Reconnecting…" state, until the server
///   comes back or the session dies. Recovery is automatic; no restart
///   is needed.
async fn startup_sync(
    client: &Client,
    token: Option<String>,
    session_file: &Path,
) -> bool {
    match sync_once(client.clone(), token, session_file).await {
        Ok(_) => return true,
        Err(InitialSyncError::Auth(err)) => {
            info!("session is dead (unknown token): {err}");
            recover_dead_session(session_file);
            return false;
        }
        Err(InitialSyncError::Transient(err)) => {
            info!("initial sync failed, will retry without the stored token: {err}");
        }
    }

    // The stored sync token is the prime suspect for a persistent failure
    // (a stale next_batch bricks every attempt): one full sync without it.
    App::send(ProgressStarted("Reconnecting…".to_string(), 0));

    match sync_once(client.clone(), None, session_file).await {
        Ok(_) => return true,
        Err(InitialSyncError::Auth(err)) => {
            info!("session is dead (unknown token): {err}");
            recover_dead_session(session_file);
            return false;
        }
        Err(InitialSyncError::Transient(err)) => {
            info!("full sync failed too, retrying with backoff: {err}");
        }
    }

    // Keep retrying, visibly, until the server comes back or the session
    // dies. Backoff grows so a dead server is not hammered.
    let mut delay = Duration::from_secs(1);

    loop {
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_secs(30));

        match sync_once(client.clone(), None, session_file).await {
            Ok(_) => return true,
            Err(InitialSyncError::Auth(err)) => {
                info!("session died during reconnect (unknown token): {err}");
                recover_dead_session(session_file);
                return false;
            }
            Err(InitialSyncError::Transient(err)) => {
                info!("still failing, backing off: {err}");
            }
        }
    }
}

/// The session is dead: move the file aside (never delete, so the failure
/// stays reversible and diagnosable) and route to the sign-in popup.
fn recover_dead_session(session_file: &Path) {
    let backup = session_file.with_extension("bak");

    if let Err(err) = fs::rename(session_file, &backup) {
        error!(
            "could not set dead session aside ({}): {}",
            session_file.display(),
            err
        );
    } else {
        info!("dead session moved aside to {}", backup.display());
    }

    App::send(MatuiEvent::LoginRequired);
}

/// The stored sync token, if the session file is still readable. A missing
/// or unreadable token just means the next sync is a full one.
fn read_sync_token(session_file: &Path) -> Option<String> {
    let serialized = fs::read_to_string(session_file).ok()?;
    let full_session: FullSession = serde_json::from_str(&serialized).ok()?;
    full_session.sync_token
}

fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file)?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session)?;

    Ok(())
}

fn add_default_handlers(client: Client) {
    client.add_event_handler(|event: AnySyncTimelineEvent, room: Room| async move {
        App::get_sender()
            .send(Matui(MatuiEvent::Timeline(
                event.into_full_event(room.room_id().into()),
            )))
            .expect("could not send timeline event");
    });

    client.add_event_handler(|ev: StrippedRoomMemberEvent, room: Room| async move {
        if ev.content.membership == MembershipState::Invite
            && ev.state_key == room.own_user_id()
            && room.state() == RoomState::Invited
        {
            send_invite_event(room).await;
        }
    });

    client.add_event_handler(|event: AnySyncEphemeralRoomEvent, room: Room| async move {
        if room.state() != RoomState::Joined {
            return;
        }

        match event {
            AnySyncEphemeralRoomEvent::Typing(SyncEphemeralRoomEvent { content: c, .. }) => {
                App::get_sender()
                    .send(Matui(MatuiEvent::Typing(room, c.user_ids)))
                    .expect("could not send typing event");
            }
            AnySyncEphemeralRoomEvent::Receipt(SyncEphemeralRoomEvent { content: c, .. }) => {
                App::get_sender()
                    .send(Matui(MatuiEvent::Receipt(room, c)))
                    .expect("could not send typing event");
            }
            _ => {}
        };
    });
}

async fn send_invite_event(room: Room) {
    let name = match room.display_name().await {
        Ok(name) => name.to_string(),
        Err(_) => room.room_id().to_string(),
    };

    let inviter = match room.invite_details().await {
        Ok(invite) => invite
            .inviter
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| invite.inviter_id.to_string()),
        Err(_) => "Someone".to_string(),
    };

    App::send(MatuiEvent::Invited(
        room,
        format!("{} invited you to {}. Join?", inviter, name),
    ));
}

fn add_verification_handlers(client: Client) {
    client.add_event_handler(
        |ev: ToDeviceKeyVerificationRequestEvent, client: Client| async move {
            let request = match client
                .encryption()
                .get_verification_request(&ev.sender, &ev.content.transaction_id)
                .await
            {
                Some(req) => req,
                None => {
                    error!("could not create request");
                    return;
                }
            };

            App::spawn(request_verification_handler(request));
        },
    );

    client.add_event_handler(
        |ev: OriginalSyncRoomMessageEvent, client: Client| async move {
            if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                let request = match client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.event_id)
                    .await
                {
                    Some(req) => req,
                    None => {
                        error!("could not create request");
                        return;
                    }
                };

                App::spawn(request_verification_handler(request));
            }
        },
    );
}

async fn request_verification_handler(request: VerificationRequest) {
    info!(
        "Accepting verification request from {}",
        request.other_user_id()
    );

    request
        .accept()
        .await
        .expect("Can't accept verification request");

    let mut stream = request.changes();

    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Created { .. }
            | VerificationRequestState::Requested { .. }
            | VerificationRequestState::Ready { .. } => (),
            VerificationRequestState::Transitioned { verification } => {
                // We only support SAS verification.
                if let Verification::SasV1(s) = verification {
                    App::spawn(sas_verification_handler(s, App::get_sender()));
                    break;
                }
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => break,
        }
    }
}

async fn sas_verification_handler(sas: SasVerification, sender: Sender<Event>) {
    sas.accept().await.unwrap();

    let mut stream = sas.changes();

    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged {
                emojis,
                decimals: _,
            } => {
                info!("verification keys exchanged");

                let emoji_slice = emojis.expect("only emoji verification is supported").emojis;

                sender
                    .send(Matui(VerificationStarted(sas.clone(), emoji_slice)))
                    .expect("could not send sas started event");
            }
            SasState::Done { .. } => {
                info!("verification done");

                sender
                    .send(Matui(VerificationCompleted))
                    .expect("could not send sas completed event");
            }
            SasState::Started { .. } => info!("verification started"),
            SasState::Accepted { .. } => info!("verification accepted"),
            SasState::Confirmed => info!("verification confirmed"),
            SasState::Cancelled(_) => info!("verification cancelled"),
            SasState::Created { .. } => info!("verification created"),
        }
    }
}

pub fn pad_emoji(emoji: &str) -> String {
    // These are emojis that need VARIATION-SELECTOR-16 (U+FE0F) so that they are
    // rendered with coloured glyphs. For these, we need to add an extra
    // space after them so that they are rendered properly in terminals.
    const VARIATION_SELECTOR_EMOJIS: [&str; 8] = ["☁️", "❤️", "☂️", "✏️", "✂️", "☎️", "✈️", "‼️"];

    // Hack to make terminals behave properly when one of the above is printed.
    if VARIATION_SELECTOR_EMOJIS.contains(&emoji) {
        format!("{emoji} ")
    } else {
        emoji.to_owned()
    }
}

pub fn center_emoji(emoji: &str) -> String {
    let emoji = pad_emoji(emoji);

    // This is a trick to account for the fact that emojis are wider than other
    // monospace characters.
    let placeholder = ".".repeat(2);

    format!("{placeholder:^6}").replace(&placeholder, &emoji)
}

pub fn format_emojis(emojis: [Emoji; 7]) -> String {
    let emojis: Vec<_> = emojis.iter().map(|e| e.symbol).collect();

    emojis
        .iter()
        .map(|e| center_emoji(e))
        .collect::<Vec<_>>()
        .join("")
}
