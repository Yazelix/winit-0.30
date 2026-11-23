//! The pointer events.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::warn;

use sctk::reexports::client::delegate_dispatch;
use sctk::reexports::client::protocol::wl_pointer::{AxisSource, WlPointer};
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle, Dispatch};
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1};
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::csd_frame::FrameClick;
use sctk::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;

use sctk::compositor::SurfaceData;
use sctk::globals::GlobalData;
use sctk::seat::pointer::{
    AxisScroll, PointerData, PointerDataExt, PointerEvent, PointerEventKind, PointerHandler,
    ThemedPointer,
};
use sctk::seat::SeatState;

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};

use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

pub mod relative_pointer;

const DEVICE_ID: crate::event::DeviceId =
    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));

impl PointerHandler for WinitState {
    fn pointer_frame(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let seat = pointer.winit_data().seat();
        let seat_state = match self.seats.get(&seat.id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received pointer event without seat");
                return;
            },
        };

        let themed_pointer = match seat_state.pointer.clone() {
            Some(pointer) => pointer,
            None => {
                warn!("Received pointer event without pointer");
                return;
            },
        };

        if themed_pointer.pointer() != pointer {
            return;
        }

        let device_id = DEVICE_ID;
        let mut next_axis = 0;
        for (index, event) in events.iter().enumerate() {
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.pointer_left(&themed_pointer);
                },
                PointerEventKind::Leave { .. } if self.pointer_left(&themed_pointer) => continue,
                _ => (),
            }
            let surface = &event.surface;

            // The parent surface.
            let parent_surface = match event.surface.data::<SurfaceData>() {
                Some(data) => data.parent_surface().unwrap_or(surface),
                None => continue,
            };

            let window_id = wayland::make_wid(parent_surface);

            // Ensure that window exists.
            let mut window = match self.windows.get_mut().get_mut(&window_id) {
                Some(window) => window.lock().unwrap(),
                None => continue,
            };

            let scale_factor = window.scale_factor();
            let position: PhysicalPosition<f64> =
                LogicalPosition::new(event.position.0, event.position.1).to_physical(scale_factor);

            match event.kind {
                // Pointer movements on decorations.
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. }
                    if parent_surface != surface =>
                {
                    if let Some(icon) = window.frame_point_moved(
                        seat,
                        surface,
                        Duration::ZERO,
                        event.position.0,
                        event.position.1,
                    ) {
                        let _ = themed_pointer.set_cursor(connection, icon);
                    }
                },
                PointerEventKind::Leave { .. } if parent_surface != surface => {
                    window.frame_point_left();
                },
                ref kind @ PointerEventKind::Press { button, serial, time }
                | ref kind @ PointerEventKind::Release { button, serial, time }
                    if parent_surface != surface =>
                {
                    let click = match wayland_button_to_winit(button) {
                        MouseButton::Left => FrameClick::Normal,
                        MouseButton::Right => FrameClick::Alternate,
                        _ => continue,
                    };
                    let pressed = matches!(kind, PointerEventKind::Press { .. });

                    // Emulate click on the frame.
                    window.frame_click(
                        click,
                        pressed,
                        seat,
                        serial,
                        Duration::from_millis(time as u64),
                        window_id,
                        &mut self.window_compositor_updates,
                    );
                },
                // Regular events on the main surface.
                PointerEventKind::Enter { .. } => {
                    self.events_sink
                        .push_window_event(WindowEvent::CursorEntered { device_id }, window_id);

                    window.pointer_entered(Arc::downgrade(&themed_pointer));

                    // Set the currently focused surface.
                    pointer.winit_data().inner.lock().unwrap().surface = Some(window_id);

                    self.events_sink.push_window_event(
                        WindowEvent::CursorMoved { device_id, position },
                        window_id,
                    );
                },
                PointerEventKind::Leave { .. } => (),
                PointerEventKind::Motion { .. } => {
                    self.events_sink.push_window_event(
                        WindowEvent::CursorMoved { device_id, position },
                        window_id,
                    );
                },
                ref kind @ PointerEventKind::Press { button, serial, .. }
                | ref kind @ PointerEventKind::Release { button, serial, .. } => {
                    // Update the last button serial.
                    pointer.winit_data().inner.lock().unwrap().latest_button_serial = serial;

                    let button = wayland_button_to_winit(button);
                    let state = if matches!(kind, PointerEventKind::Press { .. }) {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    };
                    self.events_sink.push_window_event(
                        WindowEvent::MouseInput { device_id, state, button },
                        window_id,
                    );
                },
                PointerEventKind::Axis { .. } if index < next_axis => (),
                PointerEventKind::Axis { .. } => {
                    // SCTK only merges adjacent axis entries within the Wayland frame.
                    let (frame, last_axis) = ScrollFrame::collect(
                        events[index..]
                            .iter()
                            .take_while(|event| &event.surface == surface)
                            .map(|event| &event.kind),
                    );
                    next_axis = index + last_axis + 1;
                    let mut pointer_data = pointer.winit_data().inner.lock().unwrap();
                    pointer_data.scroll(frame, scale_factor, |delta, phase| {
                        self.events_sink.push_window_event(
                            WindowEvent::MouseWheel { device_id, delta, phase },
                            window_id,
                        );
                    });
                },
            }
        }
    }
}

impl WinitState {
    pub(super) fn pointer_left(&mut self, pointer: &Arc<ThemedPointer<WinitPointerData>>) -> bool {
        let departure = pointer.pointer().winit_data().inner.lock().unwrap().leave();
        let Some((window_id, event)) = departure else { return false };

        // A destroyed surface's address may have been reused by another window.
        if self
            .windows
            .get_mut()
            .get(&window_id)
            .is_some_and(|window| window.lock().unwrap().pointer_left(Arc::downgrade(pointer)))
        {
            self.events_sink.push_window_event(event, window_id);
        }
        true
    }
}

#[derive(Debug)]
pub struct WinitPointerData {
    /// The inner winit data associated with the pointer.
    inner: Mutex<WinitPointerDataInner>,

    /// The data required by the sctk.
    sctk_data: PointerData,

    /// Viewport for fractional cursor.
    viewport: Option<WpViewport>,
}

impl WinitPointerData {
    pub fn new(seat: WlSeat, viewport: Option<WpViewport>) -> Self {
        Self {
            inner: Mutex::new(WinitPointerDataInner::default()),
            sctk_data: PointerData::new(seat),
            viewport,
        }
    }

    pub fn lock_pointer(
        &self,
        pointer_constraints: &PointerConstraintsState,
        surface: &WlSurface,
        pointer: &WlPointer,
        queue_handle: &QueueHandle<WinitState>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.locked_pointer.is_none() {
            inner.locked_pointer = Some(pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                queue_handle,
                GlobalData,
            ));
        }
    }

    pub fn unlock_pointer(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(locked_pointer) = inner.locked_pointer.take() {
            locked_pointer.destroy();
        }
    }

    pub fn confine_pointer(
        &self,
        pointer_constraints: &PointerConstraintsState,
        surface: &WlSurface,
        pointer: &WlPointer,
        queue_handle: &QueueHandle<WinitState>,
    ) {
        self.inner.lock().unwrap().confined_pointer = Some(pointer_constraints.confine_pointer(
            surface,
            pointer,
            None,
            Lifetime::Persistent,
            queue_handle,
            GlobalData,
        ));
    }

    pub fn unconfine_pointer(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(confined_pointer) = inner.confined_pointer.as_ref() {
            confined_pointer.destroy();
        }
    }

    /// Seat associated with this pointer.
    pub fn seat(&self) -> &WlSeat {
        self.sctk_data.seat()
    }

    /// Active window.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.inner.lock().unwrap().surface
    }

    /// Last button serial.
    pub fn latest_button_serial(&self) -> u32 {
        self.sctk_data.latest_button_serial().unwrap_or_default()
    }

    /// Last enter serial.
    pub fn latest_enter_serial(&self) -> u32 {
        self.sctk_data.latest_enter_serial().unwrap_or_default()
    }

    pub fn set_locked_cursor_position(&self, surface_x: f64, surface_y: f64) {
        let inner = self.inner.lock().unwrap();
        if let Some(locked_pointer) = inner.locked_pointer.as_ref() {
            locked_pointer.set_cursor_position_hint(surface_x, surface_y);
        }
    }

    pub fn viewport(&self) -> Option<&WpViewport> {
        self.viewport.as_ref()
    }
}

impl Drop for WinitPointerData {
    fn drop(&mut self) {
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
    }
}

impl PointerDataExt for WinitPointerData {
    fn pointer_data(&self) -> &PointerData {
        &self.sctk_data
    }
}

#[derive(Debug, Default)]
pub struct WinitPointerDataInner {
    /// The associated locked pointer.
    locked_pointer: Option<ZwpLockedPointerV1>,

    /// The associated confined pointer.
    confined_pointer: Option<ZwpConfinedPointerV1>,

    /// Serial of the last button event.
    latest_button_serial: u32,

    /// Currently focused window.
    surface: Option<WindowId>,

    /// Active precision axes for this pointer's focused surface.
    scroll: PrecisionScroll,
}

impl Drop for WinitPointerDataInner {
    fn drop(&mut self) {
        if let Some(locked_pointer) = self.locked_pointer.take() {
            locked_pointer.destroy();
        }

        if let Some(confined_pointer) = self.confined_pointer.take() {
            confined_pointer.destroy();
        }
    }
}

impl WinitPointerDataInner {
    fn leave(&mut self) -> Option<(WindowId, WindowEvent)> {
        self.scroll = PrecisionScroll::default();
        self.surface
            .take()
            .map(|window_id| (window_id, WindowEvent::CursorLeft { device_id: DEVICE_ID }))
    }

    fn scroll(
        &mut self,
        frame: ScrollFrame,
        scale_factor: f64,
        mut emit: impl FnMut(MouseScrollDelta, TouchPhase),
    ) {
        let ScrollFrame { horizontal, vertical, source } = frame;
        // Wayland's sign convention is the inverse of winit's.
        if horizontal.discrete != 0 || vertical.discrete != 0 {
            emit(
                MouseScrollDelta::LineDelta(
                    -(horizontal.discrete as f32),
                    -(vertical.discrete as f32),
                ),
                TouchPhase::Moved,
            );
            return;
        }

        let delta = MouseScrollDelta::PixelDelta(
            LogicalPosition::new(-horizontal.absolute, -vertical.absolute)
                .to_physical(scale_factor),
        );
        let moving = horizontal.absolute != 0.0 || vertical.absolute != 0.0;
        if matches!(source, Some(AxisSource::Wheel | AxisSource::WheelTilt)) {
            if moving {
                emit(delta, TouchPhase::Moved);
            }
            return;
        }

        // A previous source may never send its stop, especially for continuous scroll.
        let source_changed =
            self.scroll.source.zip(source).is_some_and(|(previous, source)| previous != source);
        if source_changed {
            if !moving {
                return;
            }
            self.scroll = PrecisionScroll::default();
        }

        let was_active = self.scroll.horizontal || self.scroll.vertical;
        if moving {
            self.scroll.source = source.or(self.scroll.source);
        }
        self.scroll.horizontal =
            (self.scroll.horizontal || horizontal.absolute != 0.0) && !horizontal.stop;
        self.scroll.vertical = (self.scroll.vertical || vertical.absolute != 0.0) && !vertical.stop;
        let active = self.scroll.horizontal || self.scroll.vertical;

        if !active {
            if source_changed {
                // A complete gesture from the new source must not release the previous one.
                emit(
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                    TouchPhase::Cancelled,
                );
            }
            if was_active || moving {
                // A single frame supplies no time interval for a start and end.
                emit(delta, TouchPhase::Ended);
            }
            self.scroll = PrecisionScroll::default();
        } else if moving {
            emit(delta, if was_active { TouchPhase::Moved } else { TouchPhase::Started });
        }
    }
}

#[derive(Debug, Default)]
struct PrecisionScroll {
    horizontal: bool,
    vertical: bool,
    source: Option<AxisSource>,
}

#[derive(Default)]
struct ScrollFrame {
    horizontal: AxisScroll,
    vertical: AxisScroll,
    source: Option<AxisSource>,
}

impl ScrollFrame {
    fn collect<'a>(events: impl Iterator<Item = &'a PointerEventKind>) -> (Self, usize) {
        let mut frame = Self::default();
        let mut last_axis = 0;
        for (index, event) in events.enumerate() {
            match *event {
                PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. } => break,
                PointerEventKind::Axis { horizontal, vertical, source, .. } => {
                    for (axis, other) in
                        [(&mut frame.horizontal, horizontal), (&mut frame.vertical, vertical)]
                    {
                        axis.absolute += other.absolute;
                        axis.discrete = axis.discrete.saturating_add(other.discrete);
                        axis.stop |= other.stop;
                    }
                    frame.source = frame.source.or(source);
                    last_axis = index;
                },
                _ => (),
            }
        }
        (frame, last_axis)
    }
}

/// Convert the Wayland button into winit.
fn wayland_button_to_winit(button: u32) -> MouseButton {
    // These values are coming from <linux/input-event-codes.h>.
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_EXTRA: u32 = 0x114;
    const BTN_FORWARD: u32 = 0x115;
    const BTN_BACK: u32 = 0x116;

    match button {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_BACK | BTN_SIDE => MouseButton::Back,
        BTN_FORWARD | BTN_EXTRA => MouseButton::Forward,
        button => MouseButton::Other(button as u16),
    }
}

pub trait WinitPointerDataExt {
    fn winit_data(&self) -> &WinitPointerData;
}

impl WinitPointerDataExt for WlPointer {
    fn winit_data(&self) -> &WinitPointerData {
        self.data::<WinitPointerData>().expect("failed to get pointer data.")
    }
}

pub struct PointerConstraintsState {
    pointer_constraints: ZwpPointerConstraintsV1,
}

impl PointerConstraintsState {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let pointer_constraints = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { pointer_constraints })
    }
}

impl Deref for PointerConstraintsState {
    type Target = ZwpPointerConstraintsV1;

    fn deref(&self) -> &Self::Target {
        &self.pointer_constraints
    }
}

impl Dispatch<ZwpPointerConstraintsV1, GlobalData, WinitState> for PointerConstraintsState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpPointerConstraintsV1,
        _event: <ZwpPointerConstraintsV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpLockedPointerV1, GlobalData, WinitState> for PointerConstraintsState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpLockedPointerV1,
        _event: <ZwpLockedPointerV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpConfinedPointerV1, GlobalData, WinitState> for PointerConstraintsState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpConfinedPointerV1,
        _event: <ZwpConfinedPointerV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, GlobalData, WinitState> for SeatState {
    fn event(
        _: &mut WinitState,
        _: &WpCursorShapeDeviceV1,
        _: <WpCursorShapeDeviceV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("wp_cursor_shape_manager has no events")
    }
}

impl Dispatch<WpCursorShapeManagerV1, GlobalData, WinitState> for SeatState {
    fn event(
        _: &mut WinitState,
        _: &WpCursorShapeManagerV1,
        _: <WpCursorShapeManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("wp_cursor_device_manager has no events")
    }
}

delegate_dispatch!(WinitState: [ WlPointer: WinitPointerData] => SeatState);
delegate_dispatch!(WinitState: [ WpCursorShapeManagerV1: GlobalData] => SeatState);
delegate_dispatch!(WinitState: [ WpCursorShapeDeviceV1: GlobalData] => SeatState);
delegate_dispatch!(WinitState: [ZwpPointerConstraintsV1: GlobalData] => PointerConstraintsState);
delegate_dispatch!(WinitState: [ZwpLockedPointerV1: GlobalData] => PointerConstraintsState);
delegate_dispatch!(WinitState: [ZwpConfinedPointerV1: GlobalData] => PointerConstraintsState);

#[cfg(test)]
mod tests {
    use super::*;

    fn motion(absolute: f64) -> AxisScroll {
        AxisScroll { absolute, ..AxisScroll::default() }
    }

    fn stop() -> AxisScroll {
        AxisScroll { stop: true, ..AxisScroll::default() }
    }

    fn pixel(x: f64, y: f64, phase: TouchPhase) -> (MouseScrollDelta, TouchPhase) {
        (MouseScrollDelta::PixelDelta(PhysicalPosition::new(x, y)), phase)
    }

    fn scroll(
        pointer: &mut WinitPointerDataInner,
        horizontal: AxisScroll,
        vertical: AxisScroll,
    ) -> Vec<(MouseScrollDelta, TouchPhase)> {
        scroll_frame(pointer, ScrollFrame { horizontal, vertical, source: None })
    }

    fn scroll_frame(
        pointer: &mut WinitPointerDataInner,
        frame: ScrollFrame,
    ) -> Vec<(MouseScrollDelta, TouchPhase)> {
        let mut events = Vec::new();
        pointer.scroll(frame, 2.0, |delta, phase| events.push((delta, phase)));
        events
    }

    #[test]
    fn horizontal_stop_keeps_vertical_motion_until_its_own_stop() {
        for horizontal in [0.0, 1.5] {
            let mut pointer = WinitPointerDataInner::default();
            assert_eq!(scroll(&mut pointer, motion(horizontal), motion(-2.0)), [pixel(
                -2.0 * horizontal,
                4.0,
                TouchPhase::Started
            )],);
            assert_eq!(scroll(&mut pointer, stop(), motion(-3.0)), [pixel(
                0.0,
                6.0,
                TouchPhase::Moved
            )],);
            assert_eq!(scroll(&mut pointer, AxisScroll::default(), stop()), [pixel(
                0.0,
                0.0,
                TouchPhase::Ended
            )],);
            assert!(scroll(&mut pointer, stop(), stop()).is_empty());
        }
    }

    #[test]
    fn partial_stop_is_silent_and_the_axis_can_restart() {
        let mut pointer = WinitPointerDataInner::default();
        let idle = AxisScroll::default();
        assert_eq!(scroll(&mut pointer, motion(1.0), motion(2.0)), [pixel(
            -2.0,
            -4.0,
            TouchPhase::Started
        )],);
        assert!(scroll(&mut pointer, stop(), idle).is_empty());
        assert_eq!(scroll(&mut pointer, motion(3.0), idle), [pixel(-6.0, 0.0, TouchPhase::Moved)],);
        assert!(scroll(&mut pointer, idle, stop()).is_empty());
        assert_eq!(scroll(&mut pointer, stop(), idle), [pixel(0.0, 0.0, TouchPhase::Ended)]);
        assert_eq!(scroll(&mut pointer, motion(3.0), idle), [pixel(
            -6.0,
            0.0,
            TouchPhase::Started
        )],);
    }

    #[test]
    fn split_frame_merges_both_stops_and_source_up_to_focus_boundary() {
        let idle = AxisScroll::default();
        let axis = |horizontal, vertical, source| PointerEventKind::Axis {
            time: 0,
            horizontal,
            vertical,
            source,
        };
        let events = [
            axis(stop(), idle, None),
            PointerEventKind::Motion { time: 0 },
            axis(idle, stop(), Some(AxisSource::Finger)),
            PointerEventKind::Press { time: 0, button: 0, serial: 0 },
            PointerEventKind::Leave { serial: 0 },
            PointerEventKind::Enter { serial: 0 },
            axis(motion(7.0), idle, None),
        ];
        let (frame, last_axis) = ScrollFrame::collect(events.iter());
        assert_eq!(last_axis, 2);
        assert_eq!(
            (frame.horizontal, frame.vertical, frame.source),
            (stop(), stop(), Some(AxisSource::Finger))
        );

        let mut pointer = WinitPointerDataInner::default();
        scroll(&mut pointer, motion(1.0), motion(2.0));
        assert_eq!(scroll_frame(&mut pointer, frame), [pixel(0.0, 0.0, TouchPhase::Ended)]);
        assert!(scroll(&mut pointer, stop(), stop()).is_empty());
    }

    #[test]
    fn same_axis_motion_and_stop_finishes_without_losing_motion() {
        let idle = AxisScroll::default();
        let mut pointer = WinitPointerDataInner::default();
        let flick = AxisScroll { absolute: -3.0, ..stop() };
        assert_eq!(scroll(&mut pointer, idle, flick), [pixel(0.0, 6.0, TouchPhase::Ended)]);
        assert_eq!(scroll(&mut pointer, idle, motion(-1.0)), [pixel(
            0.0,
            2.0,
            TouchPhase::Started
        )],);
        assert_eq!(scroll(&mut pointer, idle, flick), [pixel(0.0, 6.0, TouchPhase::Ended)]);
        assert!(scroll(&mut pointer, idle, stop()).is_empty());
    }

    #[test]
    fn same_axis_motion_and_stop_after_a_source_switch_cancels_the_previous_gesture() {
        let mut pointer = WinitPointerDataInner::default();
        assert_eq!(
            scroll_frame(&mut pointer, ScrollFrame {
                vertical: motion(-1.0),
                source: Some(AxisSource::Continuous),
                ..ScrollFrame::default()
            },),
            [pixel(0.0, 2.0, TouchPhase::Started)],
        );
        assert_eq!(
            scroll_frame(&mut pointer, ScrollFrame {
                vertical: AxisScroll { absolute: -3.0, ..stop() },
                source: Some(AxisSource::Finger),
                ..ScrollFrame::default()
            },),
            [pixel(0.0, 0.0, TouchPhase::Cancelled), pixel(0.0, 6.0, TouchPhase::Ended),],
        );
    }

    #[test]
    fn wheel_frames_do_not_change_precision_axes() {
        let mut pointer = WinitPointerDataInner::default();
        let wheel = || ScrollFrame {
            horizontal: AxisScroll { absolute: 4.0, discrete: 2, stop: false },
            vertical: AxisScroll { absolute: -8.0, discrete: -1, stop: true },
            source: Some(AxisSource::Wheel),
        };
        let line_event = (MouseScrollDelta::LineDelta(-2.0, 1.0), TouchPhase::Moved);
        let idle = AxisScroll::default();
        assert_eq!(scroll_frame(&mut pointer, wheel()), [line_event]);
        assert_eq!(scroll(&mut pointer, idle, motion(-1.0)), [pixel(
            0.0,
            2.0,
            TouchPhase::Started
        )],);
        assert_eq!(scroll_frame(&mut pointer, wheel()), [line_event]);
        let tilt = |horizontal| ScrollFrame {
            horizontal,
            source: Some(AxisSource::WheelTilt),
            ..ScrollFrame::default()
        };
        assert_eq!(scroll_frame(&mut pointer, tilt(motion(1.0))), [pixel(
            -2.0,
            0.0,
            TouchPhase::Moved
        )]);
        assert!(scroll_frame(&mut pointer, tilt(stop())).is_empty());
        assert_eq!(scroll(&mut pointer, idle, stop()), [pixel(0.0, 0.0, TouchPhase::Ended)]);
    }

    #[test]
    fn source_switch_does_not_carry_unterminated_axes_forward() {
        let mut pointer = WinitPointerDataInner::default();
        let idle = AxisScroll::default();
        let continuous = |horizontal, vertical| ScrollFrame {
            horizontal,
            vertical,
            source: Some(AxisSource::Continuous),
        };
        assert_eq!(scroll_frame(&mut pointer, continuous(motion(1.0), idle)), [pixel(
            -2.0,
            0.0,
            TouchPhase::Started
        )],);
        assert_eq!(
            scroll_frame(&mut pointer, ScrollFrame {
                vertical: motion(-2.0),
                source: Some(AxisSource::Finger),
                ..ScrollFrame::default()
            }),
            [pixel(0.0, 4.0, TouchPhase::Started)],
        );
        assert!(scroll_frame(&mut pointer, continuous(stop(), stop())).is_empty());
        assert!(scroll_frame(&mut pointer, continuous(idle, idle)).is_empty());
        assert_eq!(scroll(&mut pointer, idle, stop()), [pixel(0.0, 0.0, TouchPhase::Ended)]);
    }

    #[test]
    fn focus_departure_resets_scroll_even_without_a_window_to_notify() {
        let mut pointer = WinitPointerDataInner::default();
        let first_window = WindowId::from(1);
        let second_window = WindowId::from(2);
        let idle = AxisScroll::default();
        pointer.surface = Some(first_window);
        scroll(&mut pointer, motion(1.0), idle);
        let left = WindowEvent::CursorLeft { device_id: DEVICE_ID };
        assert_eq!(pointer.leave(), Some((first_window, left.clone())));
        assert_eq!(pointer.leave(), None);

        pointer.surface = Some(second_window);
        assert_eq!(scroll(&mut pointer, idle, motion(-2.0)), [pixel(
            0.0,
            4.0,
            TouchPhase::Started
        )],);
        assert_eq!(pointer.leave(), Some((second_window, left)));
        assert!(scroll(&mut pointer, idle, stop()).is_empty());

        // Decorations can receive axis events without a tracked content focus.
        scroll(&mut pointer, motion(1.0), idle);
        assert_eq!(pointer.leave(), None);
        assert_eq!(scroll(&mut pointer, idle, motion(-2.0)), [pixel(
            0.0,
            4.0,
            TouchPhase::Started
        )],);
    }
}
