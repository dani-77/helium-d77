//! Workspace state over `ext-workspace-v1`, for compositors `helium_wsl` has no
//! backend for (wasp, spitfire, mangowc, ...).
//!
//! A dedicated thread owns this Wayland connection and blocks on its events,
//! publishing a snapshot each time the compositor sends `done`. The bar's tick
//! only copies that snapshot, so it never waits on the compositor.
//!
//! `helium_wsl`'s `set()` writes the same value to the bar on every monitor (its
//! per-monitor variant is still a todo upstream), so every bar shows the same
//! list: the workspaces of the first output.

use helium_wsl::compositors::Workspace;
use std::sync::{Arc, Mutex, OnceLock};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WorkspaceState},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

#[derive(Clone)]
struct Entry {
    handle: ExtWorkspaceHandleV1,
    name: String,
    active: bool,
}

pub struct Client {
    connection: Connection,
    manager: ExtWorkspaceManagerV1,
    snapshot: Arc<Mutex<Vec<Entry>>>,
    first_output_width: Option<u32>,
}

impl Client {
    fn connect() -> Option<Self> {
        let connection = Connection::connect_to_env().ok()?;
        let (globals, mut queue) = registry_queue_init::<Listener>(&connection).ok()?;
        let qh = queue.handle();

        let mut listener = Listener::default();
        let snapshot = listener.snapshot.clone();

        let xdg_manager: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();

        // Bound before the manager so the group's output_enter events have
        // objects to refer to. The list is in advertisement order, so the first
        // entry is the same output layer-shika treats as primary.
        let outputs = globals.contents().clone_list();
        let mut is_first = true;
        for global in outputs.iter().filter(|g| g.interface == "wl_output") {
            let output: wl_output::WlOutput =
                globals
                    .registry()
                    .bind(global.name, global.version.min(4), &qh, ());
            if is_first {
                listener.first_output = Some(output.id());
                if let Some(manager) = &xdg_manager {
                    let _ = manager.get_xdg_output(&output, &qh, ());
                }
                is_first = false;
            }
        }

        let manager: ExtWorkspaceManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        queue.roundtrip(&mut listener).ok()?;
        // xdg_output and workspace handles created by the first roundtrip send
        // their initial state during the second.
        queue.roundtrip(&mut listener).ok()?;
        let first_output_width = listener.first_output_width;

        std::thread::Builder::new()
            .name("ext-workspace".into())
            .spawn(move || while queue.blocking_dispatch(&mut listener).is_ok() {})
            .ok()?;

        Some(Self {
            connection,
            manager,
            snapshot,
            first_output_width,
        })
    }

    /// Workspaces to show, with `id` as the 1-based position `activate` takes.
    pub fn workspaces(&self) -> Vec<Workspace> {
        let snapshot = self.snapshot.lock().unwrap();
        snapshot
            .iter()
            .enumerate()
            .map(|(i, entry)| Workspace {
                id: i as u32 + 1,
                name: entry.name.clone(),
                active: entry.active,
                // ext-workspace-v1 doesn't say whether a workspace has windows.
                occupied: false,
                window_count: 0,
                monitor: String::new(),
            })
            .collect()
    }

    pub fn activate(&self, position: i32) {
        let snapshot = self.snapshot.lock().unwrap();
        let Some(index) = usize::try_from(position - 1).ok() else {
            return;
        };
        if let Some(entry) = snapshot.get(index) {
            entry.handle.activate();
            self.manager.commit();
            let _ = self.connection.flush();
        }
    }

    /// Logical (scale-adjusted) width of the first output.
    pub fn first_output_width(&self) -> Option<u32> {
        self.first_output_width
    }
}

/// The shared connection, or `None` if the compositor doesn't expose the protocol.
pub fn client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT.get_or_init(Client::connect).as_ref()
}

struct WorkspaceInfo {
    handle: ExtWorkspaceHandleV1,
    name: String,
    coordinates: Vec<u32>,
    state: WorkspaceState,
}

#[derive(Default)]
struct GroupInfo {
    outputs: Vec<ObjectId>,
    workspaces: Vec<ObjectId>,
}

#[derive(Default)]
struct Listener {
    snapshot: Arc<Mutex<Vec<Entry>>>,
    first_output: Option<ObjectId>,
    first_output_width: Option<u32>,
    groups: Vec<(ObjectId, GroupInfo)>,
    workspaces: Vec<WorkspaceInfo>,
}

impl Listener {
    fn publish(&self) {
        // Compositors with one group per monitor (wasp) would otherwise repeat
        // every workspace. Without a match, for instance on spitfire, which
        // doesn't send output_enter, use the first group.
        let group = self
            .groups
            .iter()
            .find(|(_, g)| {
                self.first_output
                    .as_ref()
                    .is_some_and(|output| g.outputs.contains(output))
            })
            .or_else(|| self.groups.first());

        let mut visible: Vec<&WorkspaceInfo> = group
            .map(|(_, g)| {
                self.workspaces
                    .iter()
                    .filter(|w| g.workspaces.contains(&w.handle.id()))
                    .filter(|w| !w.state.contains(WorkspaceState::Hidden))
                    .collect()
            })
            .unwrap_or_default();
        visible.sort_by(|a, b| a.coordinates.cmp(&b.coordinates));

        *self.snapshot.lock().unwrap() = visible
            .into_iter()
            .map(|w| Entry {
                handle: w.handle.clone(),
                name: w.name.clone(),
                active: w.state.contains(WorkspaceState::Active),
            })
            .collect();
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Listener {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state
                    .groups
                    .push((workspace_group.id(), GroupInfo::default()));
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.workspaces.push(WorkspaceInfo {
                    handle: workspace,
                    name: String::new(),
                    coordinates: Vec::new(),
                    state: WorkspaceState::empty(),
                });
            }
            ext_workspace_manager_v1::Event::Done => state.publish(),
            ext_workspace_manager_v1::Event::Finished => {
                state.groups.clear();
                state.workspaces.clear();
                state.publish();
            }
            _ => {}
        }
    }

    event_created_child!(Listener, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Listener {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_workspace_group_handle_v1::Event;

        let id = proxy.id();
        if matches!(event, Event::Removed) {
            state.groups.retain(|(group, _)| *group != id);
            proxy.destroy();
            return;
        }
        let Some((_, group)) = state.groups.iter_mut().find(|(group, _)| *group == id) else {
            return;
        };
        match event {
            Event::OutputEnter { output } => group.outputs.push(output.id()),
            Event::OutputLeave { output } => group.outputs.retain(|o| *o != output.id()),
            Event::WorkspaceEnter { workspace } => group.workspaces.push(workspace.id()),
            Event::WorkspaceLeave { workspace } => {
                group.workspaces.retain(|w| *w != workspace.id())
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Listener {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_workspace_handle_v1::Event;

        let id = proxy.id();
        if matches!(event, Event::Removed) {
            state.workspaces.retain(|w| w.handle.id() != id);
            proxy.destroy();
            return;
        }
        let Some(workspace) = state.workspaces.iter_mut().find(|w| w.handle.id() == id) else {
            return;
        };
        match event {
            Event::Name { name } => workspace.name = name,
            Event::Coordinates { coordinates } => {
                workspace.coordinates = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            Event::State {
                state: WEnum::Value(value),
            } => workspace.state = value,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, ()> for Listener {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, .. } = event {
            state.first_output_width = u32::try_from(width).ok();
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Listener {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
