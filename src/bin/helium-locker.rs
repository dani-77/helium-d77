//! Native screen locker for helium-d77.
//!
//! Unlike the "Lock" action in helium-session (which shells out to hyprlock
//! / loginctl), this locks the session natively via the ext-session-lock-v1
//! Wayland protocol — the compositor itself enforces the lock, the same
//! mechanism quickshell-d77 uses through Quickshell's built-in WlSessionLock
//! support, and that fabric-d77 uses through GtkSessionLock.
//!
//! Built directly on `layer_shika::Shell` rather than helium-wsl's
//! `Helium`/`ShellInstance` wrapper: that wrapper doesn't expose
//! `create_session_lock` yet (see Cargo.toml comment), so this is the one
//! binary in the project that talks to layer-shika's raw composition API.
//!
//! Requires a PAM service file at /etc/pam.d/helium-locker, e.g.:
//!     auth    include     login
//! (mirrors /etc/pam.d/hyprlock on this machine). Without it PAM fails
//! closed — the lock screen stays up, it just can't be unlocked.

use std::rc::Rc;
use std::time::Duration;

use layer_shika::prelude::*;
use pam_client2::{Context, Flag, conv_mock::Conversation};

fn verify_password(password: &str) -> bool {
    let Ok(username) = std::env::var("USER") else {
        eprintln!("helium-locker: $USER is not set, refusing to authenticate");
        return false;
    };
    let conv = Conversation::with_credentials(username.clone(), password.to_string());
    let Ok(mut context) = Context::new("helium-locker", Some(&username), conv) else {
        eprintln!("helium-locker: failed to create PAM context");
        return false;
    };
    match context.authenticate(Flag::NONE) {
        Ok(()) => {
            eprintln!("helium-locker: PAM authentication succeeded");
            true
        }
        Err(e) => {
            eprintln!("helium-locker: PAM authentication failed: {e}");
            false
        }
    }
}

fn main() -> Result<()> {
    // layer-shika reports lock-surface configure/render errors via `log`;
    // without a logger installed those diagnostics go nowhere.
    env_logger::init();

    let mut shell = Shell::from_source(include_str!("../../ui/lock.slint")).build()?;

    let lock = Rc::new(shell.create_session_lock("LockScreen")?);
    // Wrapped in Rc so it can be moved into the `unlock_requested` closure
    // below, which `on_callback_with_args` requires to be `Clone`.
    let event_loop = Rc::new(shell.event_loop_handle());

    let lock_clone = Rc::clone(&lock);
    let event_loop_clone = Rc::clone(&event_loop);
    shell.select_lock(Surface::all()).on_callback_with_args(
        "unlock_requested",
        move |args, _ctx| {
            let ok = match args.first() {
                Some(slint_interpreter::Value::String(password)) => verify_password(password),
                _ => false,
            };
            if ok {
                match lock_clone.deactivate() {
                    Ok(()) => eprintln!("helium-locker: session-lock deactivate() queued OK"),
                    Err(e) => eprintln!("helium-locker: session-lock deactivate() FAILED: {e}"),
                }
                // `deactivate()` only queues a command; layer-shika's
                // Shell::run() has no built-in "quit" and would otherwise
                // block forever with the lock surfaces gone. Exit on the
                // next event-loop tick instead of immediately, so the
                // queued unlock/destroy requests actually get flushed to
                // the compositor (they're processed and flushed on the
                // loop iteration right after this callback returns) before
                // the process — and its input grab — goes away.
                event_loop_clone
                    .add_timer(Duration::from_millis(100), |_, _| {
                        eprintln!("helium-locker: exiting now");
                        std::process::exit(0)
                    })
                    .ok();
            }
            ok
        },
    );

    shell
        .select_lock(Surface::all())
        .on_callback("current_time", |_ctx| helium_wsl::services::time::formatted("%H:%M"));
    shell
        .select_lock(Surface::all())
        .on_callback("current_date", |_ctx| helium_wsl::services::time::formatted("%A, %d %B"));

    lock.activate()?;

    shell.run()?;

    // Not normally reached: a successful unlock calls std::process::exit
    // directly above, since Shell::run() has no other way to stop.
    Ok(())
}
