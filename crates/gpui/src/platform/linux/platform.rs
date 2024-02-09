#![allow(unused)]

use crate::{
    button_from_state, button_of_key, modifiers_from_state, point, Action, AnyWindowHandle,
    BackgroundExecutor, Bounds, ClipboardItem, CursorStyle, DisplayId, ForegroundExecutor, Keymap,
    LinuxDispatcher, LinuxDisplay, LinuxTextSystem, LinuxWindow, LinuxWindowState, Menu, Modifiers,
    MouseButton, PathPromptOptions, Platform, PlatformDisplay, PlatformInput, PlatformTextSystem,
    PlatformWindow, Point, Result, SemanticVersion, Size, Task, WindowOptions,
};

use async_task::Runnable;
use collections::{HashMap, HashSet};
use futures::channel::oneshot;
use parking_lot::Mutex;

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use time::UtcOffset;
use xcb::{x, Xid as _};
use xkbcommon::xkb;

xcb::atoms_struct! {
    #[derive(Debug)]
    pub(crate) struct XcbAtoms {
        pub wm_protocols    => b"WM_PROTOCOLS",
        pub wm_del_window   => b"WM_DELETE_WINDOW",
        wm_state        => b"_NET_WM_STATE",
        wm_state_maxv   => b"_NET_WM_STATE_MAXIMIZED_VERT",
        wm_state_maxh   => b"_NET_WM_STATE_MAXIMIZED_HORZ",
    }
}

#[derive(Default)]
struct Callbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    become_active: Option<Box<dyn FnMut()>>,
    resign_active: Option<Box<dyn FnMut()>>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    event: Option<Box<dyn FnMut(PlatformInput) -> bool>>,
    app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
}

pub(crate) struct LinuxPlatform {
    xcb_connection: Arc<xcb::Connection>,
    keymap: xkbcommon::xkb::Keymap,
    x_root_index: i32,
    atoms: XcbAtoms,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    main_receiver: flume::Receiver<Runnable>,
    text_system: Arc<LinuxTextSystem>,
    callbacks: Mutex<Callbacks>,
    state: Mutex<LinuxPlatformState>,
}

pub(crate) struct LinuxPlatformState {
    quit_requested: bool,
    windows: HashMap<x::Window, Arc<LinuxWindowState>>,
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxPlatform {
    pub(crate) fn new() -> Self {
        let (xcb_connection, x_root_index) = xcb::Connection::connect(None).unwrap();
        let atoms = XcbAtoms::intern_all(&xcb_connection).unwrap();

        let xcb_connection = Arc::new(xcb_connection);
        let (main_sender, main_receiver) = flume::unbounded::<Runnable>();
        let dispatcher = Arc::new(LinuxDispatcher::new(
            main_sender,
            &xcb_connection,
            x_root_index,
        ));
        {
            let xkb_ver = xcb_connection
                .wait_for_reply(xcb_connection.send_request(&xcb::xkb::UseExtension {
                    wanted_major: xkb::x11::MIN_MAJOR_XKB_VERSION,
                    wanted_minor: xkb::x11::MIN_MINOR_XKB_VERSION,
                }))
                .unwrap();

            assert!(
                xkb_ver.supported(),
                "required xcb-xkb-{}-{} is not supported",
                xkb::x11::MIN_MAJOR_XKB_VERSION,
                xkb::x11::MIN_MINOR_XKB_VERSION
            );

            let events = xcb::xkb::EventType::NEW_KEYBOARD_NOTIFY
                | xcb::xkb::EventType::MAP_NOTIFY
                | xcb::xkb::EventType::STATE_NOTIFY;
            let map_parts = xcb::xkb::MapPart::KEY_TYPES
                | xcb::xkb::MapPart::KEY_SYMS
                | xcb::xkb::MapPart::MODIFIER_MAP
                | xcb::xkb::MapPart::EXPLICIT_COMPONENTS
                | xcb::xkb::MapPart::KEY_ACTIONS
                | xcb::xkb::MapPart::KEY_BEHAVIORS
                | xcb::xkb::MapPart::VIRTUAL_MODS
                | xcb::xkb::MapPart::VIRTUAL_MOD_MAP;

            xcb_connection
                .check_request(
                    xcb_connection.send_request_checked(&xcb::xkb::SelectEvents {
                        device_spec: unsafe {
                            std::mem::transmute::<_, u32>(xcb::xkb::Id::UseCoreKbd)
                        } as xcb::xkb::DeviceSpec,
                        affect_which: events,
                        clear: xcb::xkb::EventType::empty(),
                        select_all: events,
                        affect_map: map_parts,
                        map: map_parts,
                        details: &[],
                    }),
                )
                .unwrap();
        }

        xcb_connection.send_request(&xcb::sync::Initialize {
            desired_major_version: xcb::sync::MAJOR_VERSION as _,
            desired_minor_version: xcb::sync::MINOR_VERSION as _,
        });

        let keymap = {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let device_id = xkb::x11::get_core_keyboard_device_id(&xcb_connection);
            xkb::x11::keymap_new_from_device(
                &context,
                &xcb_connection,
                device_id,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };

        Self {
            xcb_connection,
            x_root_index,
            atoms,
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher.clone()),
            main_receiver,
            keymap,
            text_system: Arc::new(LinuxTextSystem::new()),
            callbacks: Mutex::new(Callbacks::default()),
            state: Mutex::new(LinuxPlatformState {
                quit_requested: false,
                windows: HashMap::default(),
            }),
        }
    }
}

impl Platform for LinuxPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        on_finish_launching();
        let mut scrolling = false;
        //Note: here and below, don't keep the lock() open when calling
        // into window functions as they may invoke callbacks that need
        // to immediately access the platform (self).
        while !self.state.lock().quit_requested {
            let event = self.xcb_connection.wait_for_event().unwrap();
            match event {
                xcb::Event::X(x::Event::ClientMessage(ev)) => {
                    if let x::ClientMessageData::Data32([atom, ..]) = ev.data() {
                        if atom == self.atoms.wm_del_window.resource_id() {
                            // window "x" button clicked by user, we gracefully exit
                            let window = self.state.lock().windows.remove(&ev.window()).unwrap();
                            window.destroy();
                            let mut state = self.state.lock();
                            state.quit_requested |= state.windows.is_empty();
                        }
                    }
                }
                xcb::Event::X(x::Event::Expose(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.window()])
                    };
                    window.refresh();
                }
                xcb::Event::X(x::Event::ConfigureNotify(ev)) => {
                    let bounds = Bounds {
                        origin: Point {
                            x: ev.x().into(),
                            y: ev.y().into(),
                        },
                        size: Size {
                            width: ev.width().into(),
                            height: ev.height().into(),
                        },
                    };
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.window()])
                    };
                    window.configure(bounds)
                }
                xcb::Event::X(x::Event::ButtonPress(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let modifiers = modifiers_from_state(ev.state());
                    if let Some(button) = button_of_key(ev.detail()) {
                        window.handle_event(PlatformInput::MouseDown(crate::MouseDownEvent {
                            button,
                            position: point(
                                (ev.event_x() as f32).into(),
                                (ev.event_y() as f32).into(),
                            ),
                            modifiers,
                            click_count: 1,
                        }))
                    } else if ev.detail() == 4 || ev.detail() == 5 {
                        let touch_phase = if scrolling {
                            crate::TouchPhase::Moved
                        } else {
                            crate::TouchPhase::Started
                        };
                        window.handle_event(PlatformInput::ScrollWheel(crate::ScrollWheelEvent {
                            position: point(
                                (ev.event_x() as f32).into(),
                                (ev.event_y() as f32).into(),
                            ),
                            delta: crate::ScrollDelta::Lines(point(
                                0.,
                                if ev.detail() == 4 { 1. } else { -1.0 },
                            )),
                            modifiers,
                            touch_phase,
                        }));
                        scrolling = true;
                    }
                }
                xcb::Event::X(x::Event::ButtonRelease(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let modifiers = modifiers_from_state(ev.state());
                    if let Some(button) = button_of_key(ev.detail()) {
                        window.handle_event(PlatformInput::MouseUp(crate::MouseUpEvent {
                            button,
                            position: point(
                                (ev.event_x() as f32).into(),
                                (ev.event_y() as f32).into(),
                            ),
                            modifiers,
                            click_count: 1,
                        }))
                    } else if ev.detail() == 4 || ev.detail() == 5 {
                        window.handle_event(PlatformInput::ScrollWheel(crate::ScrollWheelEvent {
                            position: point(
                                (ev.event_x() as f32).into(),
                                (ev.event_y() as f32).into(),
                            ),
                            delta: crate::ScrollDelta::Lines(point(
                                0.,
                                if ev.detail() == 4 { 1. } else { -1.0 },
                            )),
                            modifiers,
                            touch_phase: crate::TouchPhase::Ended,
                        }));
                        scrolling = false;
                    }
                }
                xcb::Event::X(x::Event::KeyPress(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let modifiers = modifiers_from_state(ev.state());
                    let key_code = xkb::Keycode::from(ev.detail());
                    let key =
                        xkb::keysym_get_name(self.keymap.key_get_syms_by_level(key_code, 0, 0)[0])
                            .to_lowercase();
                    let full_key = std::char::from_u32(xkb::keysym_to_utf32(
                        self.keymap.key_get_syms_by_level(
                            key_code,
                            0,
                            if modifiers.shift { 1 } else { 0 },
                        )[0],
                    ))
                    .unwrap()
                    .to_string();
                    if key.starts_with("shift")
                        || key.starts_with("control")
                        || key.starts_with("super")
                        || key.starts_with("alt")
                    {
                        window.handle_event(PlatformInput::ModifiersChanged(
                            crate::ModifiersChangedEvent { modifiers },
                        ))
                    } else {
                        let key = if key == "return" {
                            "enter".to_string()
                        } else {
                            key
                        };
                        window.handle_key(
                            crate::KeyDownEvent {
                                keystroke: crate::Keystroke {
                                    modifiers,
                                    key,
                                    ime_key: None,
                                },
                                is_held: false,
                            },
                            &full_key,
                        )
                    }
                }
                xcb::Event::X(x::Event::KeyRelease(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let modifiers = modifiers_from_state(ev.state());
                    let key_code = xkb::Keycode::from(ev.detail());
                    let key =
                        xkb::keysym_get_name(self.keymap.key_get_syms_by_level(key_code, 0, 0)[0])
                            .to_lowercase();
                    if key.starts_with("shift")
                        || key.starts_with("control")
                        || key.starts_with("super")
                        || key.starts_with("alt")
                    {
                        window.handle_event(PlatformInput::ModifiersChanged(
                            crate::ModifiersChangedEvent { modifiers },
                        ))
                    } else {
                        let key = if key == "return" {
                            "enter".to_string()
                        } else {
                            key
                        };
                        window.handle_event(PlatformInput::KeyUp(crate::KeyUpEvent {
                            keystroke: crate::Keystroke {
                                modifiers,
                                key,
                                ime_key: None,
                            },
                        }))
                    }
                }
                xcb::Event::X(x::Event::MotionNotify(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let pressed_button = button_from_state(ev.state());
                    let modifiers = modifiers_from_state(ev.state());
                    window.handle_event(PlatformInput::MouseMove(crate::MouseMoveEvent {
                        pressed_button,
                        position: point((ev.event_x() as f32).into(), (ev.event_y() as f32).into()),
                        modifiers,
                    }))
                }
                xcb::Event::X(x::Event::LeaveNotify(ev)) => {
                    let window = {
                        let state = self.state.lock();
                        Arc::clone(&state.windows[&ev.event()])
                    };
                    let pressed_button = button_from_state(ev.state());
                    let modifiers = modifiers_from_state(ev.state());
                    window.handle_event(PlatformInput::MouseExited(crate::MouseExitEvent {
                        pressed_button,
                        position: point((ev.event_x() as f32).into(), (ev.event_y() as f32).into()),
                        modifiers,
                    }))
                }
                xcb::Event::Sync(xcb::sync::Event::AlarmNotify(ev)) => {
                    println!("Alarm");
                    let mut target_window = None;
                    for window in self.state.lock().windows.values() {
                        if window.xcb_alarm() == ev.alarm() {
                            target_window = Some(Arc::clone(window));
                            break;
                        }
                    }
                    if let Some(window) = target_window {
                        window.refresh();
                    }
                }
                ev => {}
            }

            if let Ok(runnable) = self.main_receiver.try_recv() {
                runnable.run();
            }
        }

        if let Some(ref mut fun) = self.callbacks.lock().quit {
            fun();
        }
    }

    fn quit(&self) {
        self.state.lock().quit_requested = true;
    }

    //todo!(linux)
    fn restart(&self) {}

    //todo!(linux)
    fn activate(&self, ignoring_other_apps: bool) {}

    //todo!(linux)
    fn hide(&self) {}

    //todo!(linux)
    fn hide_other_apps(&self) {}

    //todo!(linux)
    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        let setup = self.xcb_connection.get_setup();
        setup
            .roots()
            .enumerate()
            .map(|(root_id, _)| {
                Rc::new(LinuxDisplay::new(&self.xcb_connection, root_id as i32))
                    as Rc<dyn PlatformDisplay>
            })
            .collect()
    }

    fn display(&self, id: DisplayId) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(LinuxDisplay::new(
            &self.xcb_connection,
            id.0 as i32,
        )))
    }

    //todo!(linux)
    fn active_window(&self) -> Option<AnyWindowHandle> {
        None
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowOptions,
    ) -> Box<dyn PlatformWindow> {
        let x_window = self.xcb_connection.generate_id();

        let window_ptr = Arc::new(LinuxWindowState::new(
            options,
            &self.xcb_connection,
            self.x_root_index,
            x_window,
            &self.atoms,
        ));

        self.state
            .lock()
            .windows
            .insert(x_window, Arc::clone(&window_ptr));
        Box::new(LinuxWindow(window_ptr))
    }

    fn open_url(&self, url: &str) {
        unimplemented!()
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.lock().open_urls = Some(callback);
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Option<Vec<PathBuf>>> {
        unimplemented!()
    }

    fn prompt_for_new_path(&self, directory: &Path) -> oneshot::Receiver<Option<PathBuf>> {
        unimplemented!()
    }

    fn reveal_path(&self, path: &Path) {
        unimplemented!()
    }

    fn on_become_active(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.lock().become_active = Some(callback);
    }

    fn on_resign_active(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.lock().resign_active = Some(callback);
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.lock().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.lock().reopen = Some(callback);
    }

    fn on_event(&self, callback: Box<dyn FnMut(PlatformInput) -> bool>) {
        self.callbacks.lock().event = Some(callback);
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.callbacks.lock().app_menu_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.lock().will_open_app_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.lock().validate_app_menu_command = Some(callback);
    }

    fn os_name(&self) -> &'static str {
        "Linux"
    }

    fn double_click_interval(&self) -> Duration {
        Duration::default()
    }

    fn os_version(&self) -> Result<SemanticVersion> {
        Ok(SemanticVersion {
            major: 1,
            minor: 0,
            patch: 0,
        })
    }

    fn app_version(&self) -> Result<SemanticVersion> {
        Ok(SemanticVersion {
            major: 1,
            minor: 0,
            patch: 0,
        })
    }

    fn app_path(&self) -> Result<PathBuf> {
        unimplemented!()
    }

    //todo!(linux)
    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {}

    fn local_timezone(&self) -> UtcOffset {
        UtcOffset::UTC
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        unimplemented!()
    }

    //todo!(linux)
    fn set_cursor_style(&self, style: CursorStyle) {}

    //todo!(linux)
    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    //todo!(linux)
    fn write_to_clipboard(&self, item: ClipboardItem) {}

    //todo!(linux)
    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        None
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        unimplemented!()
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        unimplemented!()
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        unimplemented!()
    }

    fn window_appearance(&self) -> crate::WindowAppearance {
        crate::WindowAppearance::Light
    }
}

#[cfg(test)]
mod tests {
    use crate::ClipboardItem;

    use super::*;

    fn build_platform() -> LinuxPlatform {
        let platform = LinuxPlatform::new();
        platform
    }
}
