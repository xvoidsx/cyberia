use crossterm::{
    event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange},
    execute,
};
use log::LevelFilter;
use cyberia::app::App;
use cyberia::event::{Event, EventHandler};
use cyberia::handler::{
    handle_app_event, handle_blur_event, handle_focus_event, handle_key_event, handle_paste_event,
};
use cyberia::matrix::matrix::Matrix;
use cyberia::settings::watch_settings_forever;
use std::io::stdout;

fn main() -> anyhow::Result<()> {
    // Logging is unconditional, not debug-only: the installed release
    // build is the one that needs diagnosing, and a debug-only log never
    // exists there. The path sits next to the session file so it is
    // always findable; create the directory first, since it may not
    // exist on a first run (log_to_file itself creates no directories).
    let log_path = Matrix::log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    simple_logging::log_to_file(&log_path, LevelFilter::Info)?;
    log_panics::init();

    watch_settings_forever();

    // Initialize the terminal user interface
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableFocusChange, EnableBracketedPaste)?;

    // and the event system.
    let events = EventHandler::new(250);
    let sender = events.sender();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();

    let handle = runtime.handle().clone();

    // Create an application.
    let mut app = App::new(sender, handle);

    // Start the main loop.
    while app.running {
        // Handle events.
        let mut render = true;

        match events.next()? {
            Event::Tick => {
                render = app.tick();
            }
            Event::Key(key_event) => handle_key_event(key_event, &mut app, &events)?,
            Event::Paste(value) => handle_paste_event(value, &mut app),
            Event::Matui(app_event) => handle_app_event(app_event, &mut app),
            Event::Focus => handle_focus_event(&mut app),
            Event::Blur => handle_blur_event(&mut app),
            Event::Redraw => {
                let _ = terminal.clear();
                render = true;
            }
        }

        if render {
            terminal.draw(|f| app.render(f))?;
        }
    }

    // Exit the user interface.
    let _ = execute!(stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();

    Ok(())
}
