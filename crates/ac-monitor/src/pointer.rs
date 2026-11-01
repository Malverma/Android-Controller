//! The PC's pointer on the phone's monitor, through `zwlr_virtual_pointer_v1`.
//! It needs no root and no `/dev/uinput`, and is bound to one output, so the
//! phone can reach nothing outside its own monitor.

use anyhow::{Context, Result, anyhow};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1 as manager_v1, zwlr_virtual_pointer_v1 as pointer_v1};

use crate::touch::{Button, PointerAction, TouchAction, TouchEvent, TouchMapper};

/// Linux input codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// Precision of absolute positions.
const EXTENT: u32 = 1 << 16;

struct State {
    names: Vec<Option<String>>,
}

pub struct VirtualPointer {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: pointer_v1::ZwlrVirtualPointerV1,
    started: std::time::Instant,
}

impl VirtualPointer {
    /// Creates a pointer for the output called `output`.
    pub fn connect(output: &str) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let manager = globals
            .bind::<manager_v1::ZwlrVirtualPointerManagerV1, _, _>(&qh, 2..=2, ())
            .context("this compositor has no bound virtual pointer")?;
        let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ()).context("no Wayland seat")?;
        let outputs: Vec<wl_output::WlOutput> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .enumerate()
                .map(|(i, g)| globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(4), &qh, i))
                .collect()
        });
        let mut state = State { names: vec![None; outputs.len()] };
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        let at = state.names.iter().position(|n| n.as_deref() == Some(output)).ok_or_else(|| anyhow!("no Wayland output named {output}"))?;
        let pointer = manager.create_virtual_pointer_with_output(Some(&seat), Some(&outputs[at]), &qh, ());
        conn.flush().context("Wayland flush")?;
        Ok(Self { conn, queue, state, pointer, started: std::time::Instant::now() })
    }

    pub fn apply(&mut self, action: PointerAction) -> Result<()> {
        let time = self.started.elapsed().as_millis() as u32;
        match action {
            PointerAction::Move { x, y } => {
                let at = |v: f64| (v.clamp(0.0, 1.0) * f64::from(EXTENT - 1)) as u32;
                self.pointer.motion_absolute(time, at(x), at(y), EXTENT, EXTENT);
            }
            PointerAction::Button { button, pressed } => {
                let code = match button {
                    Button::Left => BTN_LEFT,
                    Button::Right => BTN_RIGHT,
                };
                let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
                self.pointer.button(time, code, state);
            }
            PointerAction::Scroll { dy } => {
                self.pointer.axis_source(wl_pointer::AxisSource::Continuous);
                self.pointer.axis(time, wl_pointer::Axis::VerticalScroll, dy);
            }
        }
        self.pointer.frame();
        self.conn.flush().context("Wayland flush")?;
        self.queue.dispatch_pending(&mut self.state).context("Wayland events")?;
        Ok(())
    }

    /// Waits until the compositor has processed everything sent so far.
    pub fn sync(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state).context("Wayland roundtrip")?;
        Ok(())
    }
}

/// Runs a pointer for `output` on its own thread. Dropping the sender ends it,
/// releasing any button a gesture still holds.
pub fn spawn(output: &str) -> Result<std::sync::mpsc::Sender<TouchEvent>> {
    let (tx, rx) = std::sync::mpsc::channel::<TouchEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let name = output.to_owned();
    std::thread::Builder::new().name(format!("pointer-{output}")).spawn(move || {
        let mut pointer = match VirtualPointer::connect(&name) {
            Ok(p) => {
                let _ = ready_tx.send(Ok(()));
                p
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let mut mapper = TouchMapper::default();
        let release = TouchEvent { action: TouchAction::Cancel, time_ms: 0, points: Vec::new() };
        let events = rx.iter().chain(std::iter::once(release));
        for event in events {
            for action in mapper.handle(&event) {
                if let Err(e) = pointer.apply(action) {
                    tracing::warn!("pointer for {name}: {e:#}");
                    return;
                }
            }
        }
    })?;
    ready_rx.recv().map_err(|_| anyhow!("the pointer thread ended"))??;
    Ok(tx)
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        self.pointer.destroy();
        let _ = self.conn.flush();
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event
            && let Some(slot) = state.names.get_mut(*i)
        {
            *slot = Some(name);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(wl_seat::WlSeat, manager_v1::ZwlrVirtualPointerManagerV1, pointer_v1::ZwlrVirtualPointerV1);
