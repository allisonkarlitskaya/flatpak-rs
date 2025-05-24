use std::{
    env,
    os::unix::net::{UnixListener, UnixStream},
};

use anyhow::{Context, Result};
use rustix::{
    fd::{AsFd, OwnedFd},
    fs::OFlags,
    pipe::{PipeFlags, pipe_with},
};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::{self, WpSecurityContextManagerV1},
    wp_security_context_v1::{self, WpSecurityContextV1},
};

use super::{
    dirbuilder::DirBuilder,
    util::{nameat, open_path},
};

#[derive(Debug, Default)]
struct ClientState {
    wp_security_context_v1_name: Option<u32>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            ref interface,
            version: 1,
            name,
        } = event
        {
            log::debug!("Got registry event: {event:?}");
            if interface == "wp_security_context_manager_v1" {
                state.wp_security_context_v1_name = Some(name);
            }
        }
    }
}

impl Dispatch<WpSecurityContextManagerV1, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &WpSecurityContextManagerV1,
        event: wp_security_context_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::warn!("Got unexpected wayland event: {event:?}");
    }
}

impl Dispatch<WpSecurityContextV1, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &WpSecurityContextV1,
        event: wp_security_context_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::warn!("Got unexpected wayland event: {event:?}");
    }
}

fn try_secure_listener(
    wayland_socket: &OwnedFd,
    runtime_dir: impl AsFd,
    name: &str,
    app_id: &str,
    instance_id: &str,
) -> Result<Option<OwnedFd>> {
    let stream = UnixStream::connect(nameat(wayland_socket, ""))
        .context("Unable to connect to host wayland socket")?;
    let conn = Connection::from_socket(stream)?;
    let mut event_queue = conn.new_event_queue();
    let qhandle = event_queue.handle();
    let registry = conn.display().get_registry(&qhandle, ());

    // get the name of the wp_security_context_v1_name interface, if available
    let mut state = ClientState::default();
    event_queue.roundtrip(&mut state)?;

    // No security extension?  Clean exit so we can do a fallback.
    let Some(ext_name) = state.wp_security_context_v1_name else {
        return Ok(None);
    };
    // ...but if we do have the extension, we must use it, or fail.

    // We're going to create and bind a new restricted socket
    let listener = UnixListener::bind(nameat(runtime_dir, name))
        .context("Unable to bind secure wayland listener in sandbox")?;
    let (close_fd, close_fd_write) = pipe_with(PipeFlags::CLOEXEC)?;

    let manager: WpSecurityContextManagerV1 = registry.bind(ext_name, 1, &qhandle, ());
    let context = manager.create_listener(listener.as_fd(), close_fd.as_fd(), &qhandle, ());
    context.set_sandbox_engine("org.flatpak.rs".to_string());
    context.set_app_id(app_id.into());
    context.set_instance_id(instance_id.into());
    context.commit();

    // make sure our commit is successful
    event_queue.roundtrip(&mut state)?;

    Ok(Some(close_fd_write))
}

/// Binds the wayland socket inside of the sandbox.  This attempts to use the
/// wp_security_context_manager_v1 extension to create a sandboxed listener, but if that fails, it
/// will just fall back to bind mounting the socket from the host.
///
/// If there is no WAYLAND_DISPLAY set on the host, this returns None.  Otherwise, it returns the
/// name of the WAYLAND_DISPLAY environment variable inside the sandbox plus an optional fd that
/// should be held open for as long as the sandbox is running.  If WAYLAND_DISPLAY is set on the
/// host then this function will bind a wayland socket in the sandbox, or return a failure.
pub(super) fn bind_wayland_socket(
    runtime_dir: &DirBuilder,
    hostdir: &OwnedFd,
    app_id: &str,
    instance_id: &str,
) -> Result<Option<(String, Option<OwnedFd>)>> {
    // No WAYLAND_DISPLAY?  Do nothing.
    let Some(host_display) = env::var_os("WAYLAND_DISPLAY") else {
        return Ok(None);
    };

    // WAYLAND_DISPLAY is evaluated relative to the XDG_RUNTIME_DIR but it can also be an absolute
    // path.  This use of openat() will work in both cases (absolute or relative).
    let socket = open_path(hostdir, &host_display, OFlags::empty())
        .with_context(|| format!("Cannot open host wayland socket {host_display:?}"))?;

    // We always create our internal socket as "wayland-0"
    let sandbox_display = "wayland-0".to_string();

    // First try to use the wp_security_context_manager_v1 extension, fall back to bind mount.
    if let Some(close_fd) =
        try_secure_listener(&socket, runtime_dir, &sandbox_display, app_id, instance_id)?
    {
        Ok(Some((sandbox_display, Some(close_fd))))
    } else {
        runtime_dir.bind_file(&sandbox_display, socket, "")?;
        Ok(Some((sandbox_display, None)))
    }
}
