use crate::app::{App, Popup};
use crate::matrix::matrix::format_emojis;
use crate::spawn::pasted_file_paths;
use crate::widgets::EventResult;
use crate::widgets::confirm::{Confirm, ConfirmBehavior};
use crate::widgets::command::Command;
use crate::widgets::error::Error;
use crate::widgets::help::Help;
use crate::widgets::homeserver::Homeserver;
use crate::widgets::oauth::Oauth;
use crate::widgets::progress::Progress;
use crate::widgets::qr::Qr;
use crate::widgets::rooms::{Rooms, sort_rooms};
use crate::widgets::signin::Signin;
use crate::widgets::upload::Upload;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use matrix_sdk::authentication::oauth::qrcode::CheckCodeSender;
use matrix_sdk::ruma::directory::PublicRoomsChunk;
use matrix_sdk::ruma::events::receipt::ReceiptEventContent;
use matrix_sdk::ruma::{OwnedRoomOrAliasId, OwnedUserId};
use std::sync::Arc;
use tokio::sync::Notify;

use crate::event::EventHandler;
use matrix_sdk::encryption::verification::{Emoji, SasVerification};
use matrix_sdk::room::{Room, RoomMember};
use matrix_sdk::ruma::events::AnyTimelineEvent;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum MatuiEvent {
    Confirm(String, String),
    Error(String),
    /// The forever sync loop has failed repeatedly: the homeserver is
    /// unreachable or degraded. Carries the last error, shown once as an
    /// alert; the chat header keeps the persistent indicator until the
    /// next successful sync clears it.
    HomeserverDegraded(String),
    Invited(Room, String),
    LoggedOut,
    LoginComplete,
    LoginRequired,
    LoginStarted,
    PasswordLoginRequired(String),
    /// The OAuth flow is waiting on the browser at the given URL; the
    /// Notify cancels it.
    OauthStarted(String, Arc<Notify>),
    ProgressStarted(String, u64),
    ProgressComplete,
    QrLoginAuth(String),
    QrLoginCode(String),
    QrLoginDone,
    QrLoginScanned(CheckCodeSender),
    Recover(String),
    Receipt(Room, ReceiptEventContent),
    RoomFound(PublicRoomsChunk),
    RoomLeft(Room),
    RoomMember(Room, RoomMember),
    RoomSelected(Room),
    Search(String),
    SyncComplete,
    SyncStarted(SyncType),
    /// A manual reconnect (Ctrl+R) succeeded. Like SyncComplete, but it
    /// keeps the room the user is already looking at.
    ReconnectComplete,
    Timeline(AnyTimelineEvent),
    TimelineBatch(Batch),
    Typing(Room, Vec<OwnedUserId>),
    VerificationStarted(SasVerification, [Emoji; 7]),
    VerificationCompleted,
}

#[derive(Clone, Debug)]
pub enum SyncType {
    Initial,
    Latest,
}

#[derive(Clone, Debug)]
pub struct Batch {
    pub room: Room,
    pub events: Vec<AnyTimelineEvent>,
    pub cursor: Option<String>,
}

/// Drop everything tied to the old session: no valid session means no
/// valid chat, thread, verification, invites, or receipts. Used when the
/// app lands back on sign-in (dead session, logout).
fn reset_for_signin(app: &mut App) {
    app.chat = None;
    app.thread = None;
    app.sas = None;
    app.invites.clear();
    app.invites_seen.clear();
    app.receipts.clear();
}

pub fn handle_app_event(event: MatuiEvent, app: &mut App) {
    match event {
        MatuiEvent::Confirm(header, msg) => {
            app.set_popup(Popup::Error(Error::with_heading(header, msg)));
        }
        MatuiEvent::Error(msg) => {
            app.set_popup(Popup::Error(Error::new(msg)));
        }
        MatuiEvent::HomeserverDegraded(msg) => {
            // One alert on the transition into degraded — the header
            // indicator is the persistent part, so this never repeats
            // while the outage lasts.
            app.set_popup(Popup::Error(Error::with_heading(
                "Homeserver unreachable".to_string(),
                format!(
                    "{msg}\n\nShowing the last messages we have. New messages will appear when the server answers again.\n\nCtrl+R retries the connection."
                ),
            )));
        }
        MatuiEvent::Invited(room, msg) => {
            // only prompt once per session, no matter how many times we hear
            // about the invite
            if app.invites_seen.insert(room.room_id().to_owned()) {
                app.invites.push_back((room, msg));
            }
        }
        MatuiEvent::LoggedOut => {
            // Sign-out lands back on the sign-in form: the session is
            // gone, so the old UI state goes with it. Quitting here used
            // to be the only logout, which left file surgery as the only
            // way back to sign-in.
            reset_for_signin(app);
            app.set_popup(Popup::Homeserver(Homeserver::default()));
        }
        MatuiEvent::LoginRequired => {
            // No valid session means no valid chat, thread, verification,
            // invites, or receipts: drop them so nothing from the old
            // session lingers behind the sign-in form.
            reset_for_signin(app);
            app.set_popup(Popup::Homeserver(Homeserver::default()));
        }
        MatuiEvent::PasswordLoginRequired(homeserver) => {
            app.set_popup(Popup::Signin(Signin::new(homeserver)));
        }
        MatuiEvent::OauthStarted(url, cancel) => {
            app.set_popup(Popup::Oauth(Oauth::new(url, cancel)));
        }
        MatuiEvent::LoginStarted => {
            app.set_popup(Popup::Progress(Progress::new("Logging in", 0)));
        }
        MatuiEvent::LoginComplete => {
            app.popup = None;
        }
        MatuiEvent::ProgressStarted(msg, delay) => {
            app.last_progress = Some(msg.clone());
            app.set_popup(Popup::Progress(Progress::new(&msg, delay)))
        }
        MatuiEvent::ProgressComplete => app.popup = None,
        MatuiEvent::QrLoginAuth(url) => {
            app.set_popup(Popup::Qr(Qr::auth(url)));
        }
        MatuiEvent::QrLoginCode(code) => {
            app.set_popup(Popup::Qr(Qr::code(code)));
        }
        MatuiEvent::QrLoginDone => {
            app.set_popup(Popup::Error(Error::with_heading(
                "QR Login".to_string(),
                "Login granted.".to_string(),
            )));
        }
        MatuiEvent::QrLoginScanned(sender) => {
            app.set_popup(Popup::Qr(Qr::check(sender)));
        }
        MatuiEvent::Recover(key) => {
            app.set_popup(Popup::Progress(Progress::new(
                "Fetching encryption keys.",
                0,
            )));
            app.matrix.recover(&key);
        }

        // if we just left the room we're looking at, move to the top one
        MatuiEvent::RoomLeft(room) => {
            if let Some(t) = &app.thread
                && t.room().room_id() == room.room_id()
            {
                app.thread = None;
            }

            if let Some(c) = &app.chat
                && c.room().room_id() == room.room_id()
            {
                let mut rooms = app.matrix.fetch_rooms();
                sort_rooms(&mut rooms);

                match rooms.first() {
                    Some(next) => app.select_room(next.inner()),
                    None => app.chat = None,
                }
            }
        }

        // Let the chat update when we learn about room membership
        MatuiEvent::RoomMember(room, member) => {
            for c in app.chat.iter_mut().chain(app.thread.iter_mut()) {
                c.room_member_event(room.clone(), member.clone());
            }
        }
        MatuiEvent::RoomFound(chunk) => {
            let name = chunk.name.clone().unwrap_or_else(|| "Unnamed".to_string());
            let target: OwnedRoomOrAliasId = match &chunk.canonical_alias {
                Some(alias) => alias.clone().into(),
                None => chunk.room_id.clone().into(),
            };
            let message = format!(
                "Join {} ({})? {} members.",
                name, target, chunk.num_joined_members
            );

            app.set_popup(Popup::Confirm(Confirm::new(
                "Room Found".to_string(),
                message,
                "Join".to_string(),
                "Cancel".to_string(),
                ConfirmBehavior::JoinPublicRoom(target),
            )));
        }
        MatuiEvent::RoomSelected(room) => app.select_room(room),
        MatuiEvent::Search(search_term) => {
            // only the chat being looked at searches
            if let Some(c) = app.thread.as_mut().or(app.chat.as_mut()) {
                c.search_event(&search_term);
            }
        }
        MatuiEvent::SyncStarted(st) => {
            match st {
                SyncType::Initial => {
                    app.last_progress = Some("Performing initial sync.".to_string());
                    app.set_popup(Popup::Progress(Progress::new(
                        "Performing initial sync.",
                        0,
                    )))
                }
                SyncType::Latest => {
                    app.last_progress = Some("Syncing".to_string());
                    app.set_popup(Popup::Progress(Progress::new("Syncing", 0)))
                }
            };
        }
        MatuiEvent::SyncComplete => {
            app.popup = None;

            // now we can sync forever
            app.matrix.sync();

            // and show the first room
            let mut rooms = app.matrix.fetch_rooms();
            sort_rooms(&mut rooms);

            if let Some(room) = rooms.first() {
                app.select_room(room.inner.clone())
            }
        }
        MatuiEvent::ReconnectComplete => {
            app.popup = None;

            // now we can sync forever (a no-op if the old loop is still
            // running)
            app.matrix.sync();

            // a reconnect from the failed state has no room showing yet:
            // land on the first room, like a fresh startup. A mid-session
            // reconnect keeps the room the user is already looking at.
            if app.chat.is_none() {
                let mut rooms = app.matrix.fetch_rooms();
                sort_rooms(&mut rooms);

                if let Some(room) = rooms.first() {
                    app.select_room(room.inner.clone())
                }
            }
        }
        MatuiEvent::Timeline(event) => {
            for c in app.chat.iter_mut().chain(app.thread.iter_mut()) {
                c.timeline_event(event.clone());
            }

            // is it weird to send events all the way up here, then right
            // back down?
            app.matrix.timeline_event(event)
        }
        MatuiEvent::TimelineBatch(batch) => {
            for c in app.chat.iter_mut().chain(app.thread.iter_mut()) {
                c.batch_event(batch.clone());
            }
        }
        MatuiEvent::Typing(room, ids) => {
            for c in app.chat.iter_mut().chain(app.thread.iter_mut()) {
                c.typing_event(room.clone(), ids.clone());
            }
        }
        MatuiEvent::Receipt(room, content) => {
            for c in app.chat.iter_mut().chain(app.thread.iter_mut()) {
                c.receipt_event(&room, &content);
            }

            app.receipts.push_back((room, content));

            if app.receipts.len() > 500 {
                app.receipts.pop_front();
            }
        }
        MatuiEvent::VerificationStarted(sas, emoji) => {
            app.sas = Some(sas);

            app.set_popup(Popup::Confirm(Confirm::new(
                "Verify".to_string(),
                format!(
                    "Do these emojis match your other session?\n\n{}",
                    format_emojis(emoji)
                ),
                "Yes".to_string(),
                "No".to_string(),
                ConfirmBehavior::Verification,
            )));
        }
        MatuiEvent::VerificationCompleted => {
            app.popup = None;
            app.sas = None;
        }
    }
}

fn handle_chat_key_event(
    key_event: &KeyEvent,
    app: &mut App,
    handler: &EventHandler,
) -> anyhow::Result<()> {
    let Some(chat) = app.thread.as_mut().or(app.chat.as_mut()) else {
        return Ok(());
    };

    let result = match chat.key_event(key_event, handler) {
        Ok(result) => result,
        Err(err) => {
            app.set_popup(Popup::Error(Error::new(err.to_string())));
            return Ok(());
        }
    };

    if let EventResult::Consumed(f) = result {
        f(app);
    }

    Ok(())
}

pub fn handle_key_event(
    key_event: KeyEvent,
    app: &mut App,
    handler: &EventHandler,
) -> anyhow::Result<()> {
    // ctrl-c always quits
    if key_event.modifiers == KeyModifiers::CONTROL && key_event.code == KeyCode::Char('c') {
        app.running = false;
        return Ok(());
    }

    // any key dismisses the startup splash; the keypress itself is consumed,
    // except ':' which also opens the command palette — the palette is
    // available whenever its key is pressed, even over the splash.
    if app.splash.is_some() {
        app.splash = None;
        if key_event.code == KeyCode::Char(':') {
            app.set_popup(Popup::Command(Command::default()));
        }
        return Ok(());
    }

    // ':' over Help opens the palette instead of merely closing Help:
    // Help's key handler would otherwise swallow it as a close, and the
    // palette must win whenever its key is pressed.
    if key_event.code == KeyCode::Char(':')
        && matches!(app.popup, Some(Popup::Help(_)))
    {
        app.set_popup(Popup::Command(Command::default()));
        return Ok(());
    }

    // give the popup first crack at the event
    let result = if let Some(w) = &mut app.popup {
        w.key_event(&key_event, handler)
    } else {
        EventResult::Ignored
    };

    if let EventResult::Consumed(f) = result {
        f(app);
        return Ok(());
    }

    // A reaction dialog is part of Chat rather than Popup, but is still modal.
    if app
        .thread
        .as_ref()
        .or(app.chat.as_ref())
        .is_some_and(|chat| chat.reacting())
    {
        handle_chat_key_event(&key_event, app, handler)?;
        return Ok(());
    }

    // we own a few key events
    match key_event.code {
        KeyCode::Char(' ') => {
            let current = app.chat.as_ref().map(|c| c.room());

            app.set_popup(Popup::Rooms(Rooms::new(app.matrix.clone(), current)));

            return Ok(());
        }
        KeyCode::Char('?') => {
            app.set_popup(Popup::Help(Help::default()));
            return Ok(());
        }
        // ':' opens the command palette from anywhere. This arm runs after
        // popup first-crack, so text inputs (room search, command input
        // itself, compose) still receive ':' as typed text, and only a
        // state with no text input consuming it reaches here — which is
        // exactly the gap that left Reconnecting with no way to reach
        // :logout. (Chat's own ':' arm now never fires first, but it
        // opens the same palette, so nothing changes there.)
        KeyCode::Char(':') => {
            app.set_popup(Popup::Command(Command::default()));
            return Ok(());
        }
        // The modifiers matter here: the match is on the code alone, so
        // an unguarded 'r' would steal every typed 'r' from chat input.
        // Chat uses Ctrl+d and Ctrl+u, never Ctrl+r, so this key is free.
        KeyCode::Char('r') if key_event.modifiers == KeyModifiers::CONTROL => {
            app.matrix.reconnect();
            return Ok(());
        }
        _ => {}
    }

    // and now pass it on to the chat being looked at
    handle_chat_key_event(&key_event, app, handler)
}

pub fn handle_paste_event(value: String, app: &mut App) {
    if let Some(popup) = app.popup.as_mut() {
        if let EventResult::Consumed(f) = popup.paste_event(&value) {
            f(app);
        }
        return;
    }

    if let Some(chat) = app.thread.as_mut().or(app.chat.as_mut())
        && let EventResult::Consumed(f) = chat.paste_event(&value)
    {
        f(app);
        return;
    }

    let paths = pasted_file_paths(&value);

    if paths.is_empty() {
        return;
    }

    let Some(room) = app
        .thread
        .as_ref()
        .or(app.chat.as_ref())
        .map(|chat| chat.room())
    else {
        return;
    };

    app.set_popup(Popup::Upload(Upload::new(app.matrix.clone(), room, paths)));
}

pub fn handle_focus_event(app: &mut App) {
    app.matrix.focus_event();

    // we consider it a room "visit" if you come back to the app and view a
    // room
    if let Some(chat) = &mut app.chat {
        app.matrix.clone().room_visit_event(chat.room());
        chat.focus_event();
    }

    if let Some(thread) = &mut app.thread {
        thread.focus_event();
    }
}

pub fn handle_blur_event(app: &mut App) {
    app.matrix.blur_event();

    for chat in app.chat.iter_mut().chain(app.thread.iter_mut()) {
        chat.blur_event();
    }
}
