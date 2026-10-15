use std::{
    num::NonZeroU32,
    sync::{Arc, mpsc},
};

use browser_protocol::{
    Frame, FrameInput, FramePixels, KeyPhase, NavigationCommand, PixelFormat, PointerButton,
    PointerPhase, TabSummary,
};
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

use crate::session::{BrowserSession, NavigationOutcome, NavigationTask, SessionError};

const TAB_STRIP_HEIGHT: u32 = 32;
const TOOLBAR_HEIGHT: u32 = 48;
const CHROME_HEIGHT: u32 = TAB_STRIP_HEIGHT + TOOLBAR_HEIGHT;
const TAB_WIDTH: f64 = 180.0;
const TAB_CONTROL_WIDTH: f64 = 36.0;
const BUTTON_WIDTH: f64 = 44.0;
const ADDRESS_LEFT: f64 = BUTTON_WIDTH * 4.0 + 8.0;

pub enum GuiEvent {
    NavigationFinished {
        generation: u64,
        result: Result<NavigationOutcome, SessionError>,
    },
    PageInputFinished {
        result: Result<NavigationOutcome, SessionError>,
    },
}

pub struct GuiApplication {
    session: BrowserSession,
    event_proxy: EventLoopProxy<GuiEvent>,
    page_input_sender: mpsc::Sender<NavigationTask>,
    page_inputs_pending: usize,
    navigation_in_flight: bool,
    navigation_generation: u64,
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    frame: Option<Frame>,
    tabs: Vec<TabSummary>,
    address: String,
    address_focused: bool,
    address_selection_all: bool,
    modifiers: ModifiersState,
    cursor: PhysicalPosition<f64>,
    status: String,
}

impl GuiApplication {
    pub fn new(session: BrowserSession, event_proxy: EventLoopProxy<GuiEvent>) -> Self {
        let address = session.current_url().to_owned();
        let (page_input_sender, page_input_receiver) = mpsc::channel::<NavigationTask>();
        let page_input_events = event_proxy.clone();
        std::thread::spawn(move || {
            while let Ok(task) = page_input_receiver.recv() {
                if page_input_events
                    .send_event(GuiEvent::PageInputFinished {
                        result: task.execute(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            session,
            event_proxy,
            page_input_sender,
            page_inputs_pending: 0,
            navigation_in_flight: false,
            navigation_generation: 0,
            window: None,
            context: None,
            surface: None,
            frame: None,
            tabs: Vec::new(),
            address,
            address_focused: false,
            address_selection_all: false,
            modifiers: ModifiersState::default(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            status: "connecting".into(),
        }
    }

    fn page_viewport_size(&self) -> (u32, u32) {
        self.window
            .as_ref()
            .map(|window| {
                let size = window.inner_size();
                (
                    size.width.max(1),
                    size.height.saturating_sub(CHROME_HEIGHT).max(1),
                )
            })
            .unwrap_or((800, 600))
    }

    fn refresh(&mut self) {
        self.address = self.session.current_url().to_owned();
        let (viewport_width, viewport_height) = self.page_viewport_size();
        match self.session.render_frame(viewport_width, viewport_height) {
            Ok(frame) => {
                self.frame = Some(frame);
                self.status = "ready".into();
            }
            Err(error) => {
                if std::env::var_os("RWB_GUI_TRACE_INPUT").is_some() {
                    eprintln!("rwb-gui-refresh frame-error={error}");
                }
                self.status = error.to_string();
            }
        }
        match self.session.tabs() {
            Ok(tabs) => self.tabs = tabs,
            Err(error) => {
                if std::env::var_os("RWB_GUI_TRACE_INPUT").is_some() {
                    eprintln!("rwb-gui-refresh tabs-error={error}");
                }
                self.status = error.to_string();
            }
        }
        self.update_title();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            window.set_title(&format!(
                "Rust Agent Browser — {} — {}",
                self.address, self.status
            ));
        }
    }

    fn perform_navigation(&mut self, action: ToolbarAction) {
        if matches!(action, ToolbarAction::Stop) {
            self.navigation_generation = self.navigation_generation.wrapping_add(1);
            self.navigation_in_flight = false;
            match self.session.stop() {
                Ok(()) => self.refresh(),
                Err(error) => {
                    self.refresh();
                    self.status = error.to_string();
                    self.update_title();
                }
            }
            return;
        }
        if self.navigation_in_flight {
            self.status = "navigation already in progress".into();
            self.update_title();
            return;
        }
        let command = match action {
            ToolbarAction::Back => NavigationCommand::Back,
            ToolbarAction::Forward => NavigationCommand::Forward,
            ToolbarAction::Reload => NavigationCommand::Reload,
            ToolbarAction::Navigate => NavigationCommand::Open {
                url: self.address.clone(),
            },
            ToolbarAction::Stop => unreachable!(),
        };
        let task = match self.session.begin_navigation(command) {
            Ok(task) => task,
            Err(error) => {
                self.status = error.to_string();
                self.update_title();
                return;
            }
        };
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
        let generation = self.navigation_generation;
        self.navigation_in_flight = true;
        self.status = "loading".into();
        self.update_title();
        let event_proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            let _ = event_proxy.send_event(GuiEvent::NavigationFinished {
                generation,
                result: task.execute(),
            });
        });
    }

    fn click_tab_strip(&mut self) {
        let tabs_width = self.tabs.len() as f64 * TAB_WIDTH;
        let result = if self.cursor.x < tabs_width {
            let index = (self.cursor.x / TAB_WIDTH) as usize;
            let Some(tab_id) = self.tabs.get(index).map(|tab| tab.tab_id.clone()) else {
                return;
            };
            if &tab_id == self.session.active_tab_id() {
                return;
            }
            self.status = "switching tab".into();
            self.session.activate_tab(tab_id)
        } else if self.cursor.x < tabs_width + TAB_CONTROL_WIDTH {
            self.status = "creating tab".into();
            self.session.new_tab()
        } else if self.cursor.x < tabs_width + TAB_CONTROL_WIDTH * 2.0 {
            self.status = "closing tab".into();
            self.session.close_active_tab()
        } else {
            return;
        };
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
        self.navigation_in_flight = false;
        match result {
            Ok(()) => self.refresh(),
            Err(error) => {
                self.status = error.to_string();
                self.update_title();
            }
        }
    }

    fn click_toolbar(&mut self) {
        let x = self.cursor.x;
        if x < BUTTON_WIDTH {
            self.perform_navigation(ToolbarAction::Back);
        } else if x < BUTTON_WIDTH * 2.0 {
            self.perform_navigation(ToolbarAction::Forward);
        } else if x < BUTTON_WIDTH * 3.0 {
            self.perform_navigation(ToolbarAction::Reload);
        } else if x < BUTTON_WIDTH * 4.0 {
            self.perform_navigation(ToolbarAction::Stop);
        } else {
            if !self.address_focused {
                self.address.clear();
            }
            self.address_focused = true;
            self.address_selection_all = false;
            self.update_title();
        }
    }

    fn handle_key(&mut self, event: winit::event::KeyEvent) {
        if std::env::var_os("RWB_GUI_TRACE_INPUT").is_some() {
            eprintln!(
                "rwb-gui-key state={:?} logical={:?} text={:?}",
                event.state, event.logical_key, event.text
            );
        }
        let key_phase = match event.state {
            ElementState::Pressed => KeyPhase::Down,
            ElementState::Released => KeyPhase::Up,
        };
        if event.state == ElementState::Released && self.address_focused {
            return;
        }
        match event.logical_key {
            Key::Character(ref value)
                if self.address_focused
                    && self.modifiers.control_key()
                    && value.as_ref().eq_ignore_ascii_case("a") =>
            {
                self.address_selection_all = true;
                self.update_title();
            }
            Key::Named(NamedKey::Enter) if self.address_focused => {
                self.address_focused = false;
                self.address_selection_all = false;
                self.perform_navigation(ToolbarAction::Navigate);
            }
            Key::Named(NamedKey::Escape) if self.address_focused => {
                self.address_focused = false;
                self.address_selection_all = false;
                self.update_title();
            }
            Key::Named(NamedKey::Backspace) if self.address_focused => {
                if self.address_selection_all {
                    self.address.clear();
                } else {
                    self.address.pop();
                }
                self.address_selection_all = false;
                self.update_title();
            }
            _ if self.address_focused => {
                if let Some(text) = event.text {
                    let text = text
                        .chars()
                        .filter(|character| !character.is_control())
                        .collect::<String>();
                    if !text.is_empty() {
                        if self.address_selection_all {
                            self.address.clear();
                        }
                        self.address.push_str(&text);
                        self.address_selection_all = false;
                        self.update_title();
                    }
                }
            }
            logical_key => {
                let key = key_name(&logical_key);
                let text = event
                    .text
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .or_else(|| match &logical_key {
                        Key::Character(value) if !value.is_empty() => Some(value.to_string()),
                        _ => None,
                    });
                self.send_page_input(FrameInput::Key {
                    phase: key_phase,
                    key,
                    text,
                });
            }
        }
    }

    fn send_page_input(&mut self, input: FrameInput) {
        let view_id = if let Some(frame) = &self.frame {
            frame.view_id.clone()
        } else {
            let (viewport_width, viewport_height) = self.page_viewport_size();
            match self.session.render_frame(viewport_width, viewport_height) {
                Ok(frame) => {
                    let view_id = frame.view_id.clone();
                    self.frame = Some(frame);
                    view_id
                }
                Err(error) => {
                    self.status = error.to_string();
                    self.update_title();
                    return;
                }
            }
        };
        let task = match self.session.begin_frame_input(view_id, input) {
            Ok(task) => task,
            Err(error) => {
                self.status = error.to_string();
                self.update_title();
                return;
            }
        };
        if self.page_input_sender.send(task).is_err() {
            self.status = "page input worker stopped".into();
            self.update_title();
            return;
        }
        self.page_inputs_pending = self.page_inputs_pending.saturating_add(1);
    }

    fn page_coordinates(&self) -> Option<(f32, f32)> {
        let window = self.window.as_ref()?;
        let frame = self.frame.as_ref()?;
        let size = window.inner_size();
        let page_height = size.height.saturating_sub(CHROME_HEIGHT);
        if size.width == 0 || page_height == 0 || frame.width == 0 || frame.height == 0 {
            return None;
        }
        let page_x = self.cursor.x.clamp(0.0, f64::from(size.width));
        let page_y = (self.cursor.y - f64::from(CHROME_HEIGHT)).clamp(0.0, f64::from(page_height));
        Some((
            (page_x * f64::from(frame.width) / f64::from(size.width)) as f32,
            (page_y * f64::from(frame.height) / f64::from(page_height)) as f32,
        ))
    }

    fn send_page_pointer(&mut self, phase: PointerPhase) {
        let Some((x, y)) = self.page_coordinates() else {
            return;
        };
        let Some(view_id) = self.frame.as_ref().map(|frame| frame.view_id.clone()) else {
            return;
        };
        let input = FrameInput::Pointer {
            phase,
            x,
            y,
            button: Some(PointerButton::Primary),
        };
        if phase == PointerPhase::Up {
            let task = match self.session.begin_frame_input(view_id, input) {
                Ok(task) => task,
                Err(error) => {
                    self.status = error.to_string();
                    self.update_title();
                    return;
                }
            };
            self.navigation_generation = self.navigation_generation.wrapping_add(1);
            let generation = self.navigation_generation;
            self.navigation_in_flight = true;
            self.status = "loading".into();
            self.update_title();
            let event_proxy = self.event_proxy.clone();
            std::thread::spawn(move || {
                let _ = event_proxy.send_event(GuiEvent::NavigationFinished {
                    generation,
                    result: task.execute(),
                });
            });
        } else if let Err(error) = self.session.frame_input(view_id, input) {
            self.status = error.to_string();
            self.update_title();
        }
    }

    fn paint(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let size = window.inner_size();
        let Some(width) = NonZeroU32::new(size.width) else {
            return;
        };
        let Some(height) = NonZeroU32::new(size.height) else {
            return;
        };
        if let Err(error) = surface.resize(width, height) {
            self.status = format!("surface resize failed: {error}");
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            self.status = "surface buffer unavailable".into();
            return;
        };
        buffer.fill(0x001b1e23);
        draw_tab_strip(
            &mut buffer,
            size.width,
            size.height,
            &self.tabs,
            self.session.active_tab_id(),
        );
        draw_toolbar(&mut buffer, size.width, size.height, self.address_focused);
        if let Some(frame) = &self.frame {
            draw_frame(&mut buffer, size.width, size.height, frame);
        }
        if let Err(error) = buffer.present() {
            self.status = format!("frame presentation failed: {error}");
        }
    }
}

impl ApplicationHandler<GuiEvent> for GuiApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Rust Agent Browser")
            .with_inner_size(LogicalSize::new(1100.0, 760.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("failed to create browser window: {error}");
                event_loop.exit();
                return;
            }
        };
        let context = match Context::new(window.clone()) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("failed to create software-rendering context: {error}");
                event_loop.exit();
                return;
            }
        };
        let surface = match Surface::new(&context, window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("failed to create software-rendering surface: {error}");
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.refresh();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: GuiEvent) {
        match event {
            GuiEvent::NavigationFinished { generation, result } => {
                if generation != self.navigation_generation {
                    return;
                }
                self.navigation_in_flight = false;
                match result {
                    Ok(outcome) => {
                        self.session.apply_navigation_outcome(outcome);
                        self.refresh();
                    }
                    Err(error) => {
                        self.refresh();
                        self.status = error.to_string();
                        self.update_title();
                    }
                }
            }
            GuiEvent::PageInputFinished { result } => {
                self.page_inputs_pending = self.page_inputs_pending.saturating_sub(1);
                match result {
                    Ok(outcome) => self.session.apply_navigation_outcome(outcome),
                    Err(error) => {
                        self.status = error.to_string();
                        self.update_title();
                    }
                }
                // Native key events are queued against one immutable frame view. Refreshing
                // after an intermediate completion would replace that view while later events
                // are still pending, causing the remainder of the accepted input sequence to
                // fail as stale. Present the daemon's final committed state once the ordered
                // input queue has drained.
                if self.page_inputs_pending == 0 {
                    self.refresh();
                    // Input completion already runs on the GUI event-loop thread. Present the
                    // freshly rendered daemon frame now so native screenshot consumers cannot
                    // observe the previous surface while a deferred redraw is still pending.
                    self.paint();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) => self.refresh(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                trace_pointer_event("move", self.cursor);
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } if self.cursor.y < f64::from(TAB_STRIP_HEIGHT) => {
                trace_pointer_event("tab-up", self.cursor);
                self.click_tab_strip();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } if self.cursor.y < f64::from(CHROME_HEIGHT) => {
                trace_pointer_event("toolbar-up", self.cursor);
                self.click_toolbar();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.cursor.y >= f64::from(CHROME_HEIGHT) => {
                trace_pointer_event("page-down", self.cursor);
                self.send_page_pointer(PointerPhase::Down);
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                trace_pointer_event("page-up", self.cursor);
                self.send_page_pointer(PointerPhase::Up);
            }
            WindowEvent::MouseWheel { delta, .. } if self.cursor.y >= f64::from(CHROME_HEIGHT) => {
                let (delta_x, delta_y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 48.0, y * 48.0),
                    MouseScrollDelta::PixelDelta(position) => {
                        (position.x as f32, position.y as f32)
                    }
                };
                let Some((x, y)) = self.page_coordinates() else {
                    return;
                };
                self.send_page_input(FrameInput::Scroll {
                    x,
                    y,
                    delta_x,
                    delta_y: -delta_y,
                });
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event),
            _ => {}
        }
    }
}

fn trace_pointer_event(kind: &str, position: PhysicalPosition<f64>) {
    if std::env::var_os("RWB_GUI_TRACE_POINTER").is_some() {
        eprintln!(
            "rwb-gui-pointer {kind} x={:.3} y={:.3}",
            position.x, position.y
        );
    }
}

fn key_name(key: &Key) -> String {
    match key {
        Key::Character(value) => value.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Dead(value) => value.map(|value| value.to_string()).unwrap_or_default(),
        Key::Unidentified(_) => "Unidentified".into(),
    }
}

#[derive(Clone, Copy)]
enum ToolbarAction {
    Back,
    Forward,
    Reload,
    Stop,
    Navigate,
}

fn draw_tab_strip(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    tabs: &[TabSummary],
    active_tab: &browser_protocol::TabId,
) {
    let strip_height = TAB_STRIP_HEIGHT.min(height);
    fill_rect(buffer, width, 0, 0, width, strip_height, 0x0017181d);
    for (index, tab) in tabs.iter().enumerate() {
        let left = (index as u32).saturating_mul(TAB_WIDTH as u32);
        if left >= width {
            break;
        }
        let tab_width = (TAB_WIDTH as u32).min(width - left);
        let active = &tab.tab_id == active_tab;
        fill_rect(
            buffer,
            width,
            left + 2,
            2,
            tab_width.saturating_sub(4),
            strip_height.saturating_sub(4),
            if active { 0x003b414c } else { 0x00272b33 },
        );
        if active {
            fill_rect(
                buffer,
                width,
                left + 2,
                strip_height.saturating_sub(3),
                tab_width.saturating_sub(4),
                3,
                0x005276b8,
            );
        }
    }
    let controls_left = (tabs.len() as u32).saturating_mul(TAB_WIDTH as u32);
    let control_width = TAB_CONTROL_WIDTH as u32;
    for offset in [0, control_width] {
        let left = controls_left.saturating_add(offset);
        if left >= width {
            continue;
        }
        fill_rect(
            buffer,
            width,
            left + 2,
            2,
            control_width.min(width - left).saturating_sub(4),
            strip_height.saturating_sub(4),
            0x00272b33,
        );
    }
    if controls_left + 24 < width {
        fill_rect(buffer, width, controls_left + 11, 15, 14, 2, 0x00f0f3f8);
        fill_rect(buffer, width, controls_left + 17, 9, 2, 14, 0x00f0f3f8);
    }
    let close_left = controls_left.saturating_add(control_width);
    if close_left + 24 < width {
        for offset in 0..12 {
            fill_rect(
                buffer,
                width,
                close_left + 12 + offset,
                10 + offset,
                2,
                2,
                0x00f0f3f8,
            );
            fill_rect(
                buffer,
                width,
                close_left + 23 - offset,
                10 + offset,
                2,
                2,
                0x00f0f3f8,
            );
        }
    }
}

fn draw_toolbar(buffer: &mut [u32], width: u32, height: u32, focused: bool) {
    let top = TAB_STRIP_HEIGHT.min(height);
    let toolbar_height = TOOLBAR_HEIGHT.min(height.saturating_sub(top));
    fill_rect(buffer, width, 0, top, width, toolbar_height, 0x00272b33);
    for index in 0..4 {
        let left = index * BUTTON_WIDTH as u32;
        fill_rect(
            buffer,
            width,
            left + 4,
            top + 6,
            BUTTON_WIDTH as u32 - 8,
            toolbar_height.saturating_sub(12),
            0x003b414c,
        );
    }
    draw_back(buffer, width, 14, top + 15, 0x00f0f3f8);
    draw_forward(buffer, width, 58, top + 15, 0x00f0f3f8);
    draw_reload(buffer, width, 102, top + 15, 0x00f0f3f8);
    draw_stop(buffer, width, 146, top + 16, 0x00f0f3f8);
    let address_left = ADDRESS_LEFT as u32;
    if width > address_left + 8 {
        fill_rect(
            buffer,
            width,
            address_left,
            top + 7,
            width - address_left - 8,
            toolbar_height.saturating_sub(14),
            if focused { 0x005276b8 } else { 0x00d8dee9 },
        );
        fill_rect(
            buffer,
            width,
            address_left + 2,
            top + 9,
            width - address_left - 12,
            toolbar_height.saturating_sub(18),
            0x001b1e23,
        );
    }
}

fn draw_frame(buffer: &mut [u32], output_width: u32, output_height: u32, frame: &Frame) {
    let FramePixels::Owned {
        format,
        stride,
        bytes,
    } = &frame.pixels
    else {
        return;
    };
    let draw_width = output_width;
    let draw_height = output_height.saturating_sub(CHROME_HEIGHT);
    if draw_width == 0 || draw_height == 0 || frame.width == 0 || frame.height == 0 {
        return;
    }
    for y in 0..draw_height {
        let source_y = u64::from(y) * u64::from(frame.height) / u64::from(draw_height);
        for x in 0..draw_width {
            let source_x = u64::from(x) * u64::from(frame.width) / u64::from(draw_width);
            let source = (source_y as usize) * (*stride as usize) + (source_x as usize) * 4;
            if source + 3 >= bytes.len() {
                return;
            }
            let (red, green, blue) = match format {
                PixelFormat::Rgba8Premultiplied => {
                    (bytes[source], bytes[source + 1], bytes[source + 2])
                }
                PixelFormat::Bgra8Premultiplied => {
                    (bytes[source + 2], bytes[source + 1], bytes[source])
                }
            };
            let destination = ((y + CHROME_HEIGHT) * output_width + x) as usize;
            buffer[destination] =
                (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue);
        }
    }
}

fn fill_rect(buffer: &mut [u32], stride: u32, x: u32, y: u32, width: u32, height: u32, color: u32) {
    for row in y..y.saturating_add(height) {
        let start = (row.saturating_mul(stride).saturating_add(x)) as usize;
        let end = start.saturating_add(width as usize).min(buffer.len());
        if start < end {
            buffer[start..end].fill(color);
        }
    }
}

fn draw_back(buffer: &mut [u32], stride: u32, x: u32, y: u32, color: u32) {
    for offset in 0..10 {
        fill_rect(buffer, stride, x + offset, y + 9 - offset, 2, 2, color);
        fill_rect(buffer, stride, x + offset, y + 9 + offset, 2, 2, color);
    }
    fill_rect(buffer, stride, x + 2, y + 9, 18, 2, color);
}

fn draw_forward(buffer: &mut [u32], stride: u32, x: u32, y: u32, color: u32) {
    for offset in 0..10 {
        fill_rect(buffer, stride, x + 9 - offset, y + 9 - offset, 2, 2, color);
        fill_rect(buffer, stride, x + 9 - offset, y + 9 + offset, 2, 2, color);
    }
    fill_rect(buffer, stride, x, y + 9, 18, 2, color);
}

fn draw_reload(buffer: &mut [u32], stride: u32, x: u32, y: u32, color: u32) {
    fill_rect(buffer, stride, x + 3, y, 14, 2, color);
    fill_rect(buffer, stride, x, y + 3, 2, 14, color);
    fill_rect(buffer, stride, x + 3, y + 18, 14, 2, color);
    fill_rect(buffer, stride, x + 18, y + 6, 2, 11, color);
    fill_rect(buffer, stride, x + 15, y + 4, 6, 2, color);
    fill_rect(buffer, stride, x + 18, y + 1, 2, 7, color);
}

fn draw_stop(buffer: &mut [u32], stride: u32, x: u32, y: u32, color: u32) {
    fill_rect(buffer, stride, x, y, 18, 18, color);
}
