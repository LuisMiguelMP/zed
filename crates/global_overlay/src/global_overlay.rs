use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use gpui::{App, AnyWindowHandle, Global, KeyBinding, actions};
use log::error;
use parking_lot::Mutex;
use std::sync::Arc;
use util::ResultExt;

actions!(global_overlay, [ToggleGlobalOverlay]);

const TOGGLE_BINDING: &str = "shift-tab";

// ─── macOS ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos_hotkey {
    use super::*;
    use cocoa::base::id;
    use std::ffi::c_void;

    #[allow(non_upper_case_globals)]
    const kEventClassKeyboard: u32 = 0x6b657962; // 'keyb'
    #[allow(non_upper_case_globals)]
    const kEventHotKeyPressed: u32 = 5;
    const VKEY_TAB: u32 = 48;
    const CARBON_SHIFT_KEY: u32 = 0x0200;

    // NSEventType::NSKeyDown = 10
    const NS_KEY_DOWN: u64 = 10;
    // NSEventModifierFlags: shift key mask
    const NS_SHIFT_KEY_MASK: u64 = 1 << 17;

    #[allow(non_camel_case_types)]
    type EventHotKeyRef = *mut c_void;
    #[allow(non_camel_case_types)]
    type EventHandlerRef = *mut c_void;
    #[allow(non_camel_case_types)]
    type EventRef = *mut c_void;
    #[allow(non_camel_case_types)]
    type EventHandlerCallRef = *mut c_void;
    #[allow(non_camel_case_types)]
    type OSStatus = i32;
    #[allow(non_camel_case_types)]
    type EventTargetRef = *mut c_void;

    #[repr(C)]
    #[allow(non_snake_case)]
    #[derive(Copy, Clone)]
    struct EventHotKeyIDRaw {
        signature: u32,
        id: u32,
    }

    #[repr(C)]
    struct EventTypeSpec {
        event_class: u32,
        event_kind: u32,
    }

    struct HotkeyCallbackData {
        sender: mpsc::UnboundedSender<()>,
    }

    extern "C" fn hotkey_callback(
        _: EventHandlerCallRef,
        _: EventRef,
        user_data: *mut c_void,
    ) -> OSStatus {
        if user_data.is_null() {
            return 0;
        }
        // Safety: user_data points to a Box<HotkeyCallbackData> that lives as long as
        // GlobalHotkey. We only read from it here; the Box is not dropped from this callback.
        let data = unsafe { &*(user_data as *const HotkeyCallbackData) };
        data.sender.unbounded_send(()).ok();
        0
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn RegisterEventHotKey(
            inHotKeyCode: u32,
            inHotKeyModifiers: u32,
            inHotKeyID: EventHotKeyIDRaw,
            inTarget: EventTargetRef,
            inOptions: u32,
            outRef: *mut EventHotKeyRef,
        ) -> OSStatus;
        fn UnregisterEventHotKey(inHotKey: EventHotKeyRef) -> OSStatus;
        fn InstallEventHandler(
            inTarget: EventTargetRef,
            inHandler: extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus,
            inNumTypes: usize,
            inList: *const EventTypeSpec,
            inUserData: *mut c_void,
            outRef: *mut EventHandlerRef,
        ) -> OSStatus;
        fn RemoveEventHandler(inHandlerRef: EventHandlerRef) -> OSStatus;
        fn GetApplicationEventTarget() -> EventTargetRef;
    }

    pub struct GlobalHotkey {
        hotkey_ref: EventHotKeyRef,
        handler_ref: EventHandlerRef,
        _callback_data: Box<HotkeyCallbackData>,
        local_monitor: cocoa::base::id,
        sender_ptr: *mut mpsc::UnboundedSender<()>,
    }

    // Safety: These opaque pointers are only accessed from the main thread.
    unsafe impl Send for GlobalHotkey {}

    impl GlobalHotkey {
        pub fn new(sender: mpsc::UnboundedSender<()>) -> Result<Self> {
            let callback_data = Box::new(HotkeyCallbackData { sender: sender.clone() });
            let user_data = &*callback_data as *const HotkeyCallbackData as *mut c_void;

            let event_spec = EventTypeSpec {
                event_class: kEventClassKeyboard,
                event_kind: kEventHotKeyPressed,
            };

            let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
            let status = unsafe {
                InstallEventHandler(
                    GetApplicationEventTarget(),
                    hotkey_callback,
                    1,
                    &event_spec,
                    user_data,
                    &mut handler_ref,
                )
            };
            if status != 0 {
                return Err(anyhow!("InstallEventHandler failed with status {}", status));
            }

            let hotkey_id = EventHotKeyIDRaw {
                signature: u32::from_be_bytes(*b"GZED"),
                id: 1,
            };

            let mut hotkey_ref: EventHotKeyRef = std::ptr::null_mut();
            let status = unsafe {
                RegisterEventHotKey(
                    VKEY_TAB,
                    CARBON_SHIFT_KEY,
                    hotkey_id,
                    GetApplicationEventTarget(),
                    0,
                    &mut hotkey_ref,
                )
            };
            if status != 0 {
                unsafe { RemoveEventHandler(handler_ref) };
                return Err(anyhow!(
                    "RegisterEventHotKey failed with status {}",
                    status
                ));
            }

            // The Carbon hotkey fires for other apps, but GPUI's keyDown:/keyEquivalent:
            // dispatch runs before Carbon processes events within the same Zed process.
            // An NSEvent local monitor intercepts Shift+Tab before any window/view sees it.
            let (local_monitor, sender_raw) = unsafe { install_local_monitor(sender.clone()) };

            Ok(Self {
                hotkey_ref,
                handler_ref,
                _callback_data: callback_data,
                local_monitor,
                sender_ptr: sender_raw,
            })
        }
    }

    impl Drop for GlobalHotkey {
        fn drop(&mut self) {
            unsafe {
                if !self.hotkey_ref.is_null() {
                    UnregisterEventHotKey(self.hotkey_ref);
                }
                if !self.handler_ref.is_null() {
                    RemoveEventHandler(self.handler_ref);
                }
                if !self.local_monitor.is_null() {
                    use objc::{class, msg_send, sel, sel_impl};
                    let _: () = msg_send![class!(NSEvent), removeMonitor: self.local_monitor];
                }
                if !self.sender_ptr.is_null() {
                    let _ = Box::from_raw(self.sender_ptr);
                }
            }
        }
    }

    /// Registers an `NSEvent` local monitor for `NSKeyDown` events. It intercepts
    /// Shift+Tab before GPUI's `keyDown:`/`keyEquivalent:` handlers run and returns
    /// `nil` to consume the event, triggering the overlay toggle.
    unsafe fn install_local_monitor(
        sender: mpsc::UnboundedSender<()>,
    ) -> (cocoa::base::id, *mut mpsc::UnboundedSender<()>) {
        use block::ConcreteBlock;
        use cocoa::base::{id, nil};
        use objc::{class, msg_send, sel, sel_impl};

        // Box the sender and get a raw pointer to it so the block closure can use it.
        // It is freed when GlobalHotkey is dropped.
        let sender_ptr = Box::into_raw(Box::new(sender));

        let block = ConcreteBlock::new(move |event: id| -> id {
            let key_code: u16 = msg_send![event, keyCode];
            let modifier_flags: u64 = msg_send![event, modifierFlags];
            let shift_down = (modifier_flags & NS_SHIFT_KEY_MASK) != 0;

            // Virtual key code 48 = Tab. Shift+Tab is the overlay toggle binding.
            if key_code == 48 && shift_down {
                // Safety: sender_ptr lives until the monitor is removed.
                unsafe {
                    let sender = &*sender_ptr;
                    sender.unbounded_send(()).ok();
                }
                // Return nil to consume the event so it doesn't reach GPUI's handlers.
                return nil;
            }
            event
        });
        let block = block.copy();

        let mask: u64 = 1 << NS_KEY_DOWN; // NSEventMaskKeyDown
        let monitor: id = msg_send![
            class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: mask
            handler: &*block
        ];
        (monitor, sender_ptr)
    }

    pub struct StatusItem {
        item: cocoa::base::id,
        _target: cocoa::base::id,
        _sender_ptr: *mut mpsc::UnboundedSender<()>,
    }

    impl StatusItem {
        pub unsafe fn new(sender: mpsc::UnboundedSender<()>) -> Self {
            use cocoa::base::{id, nil};
            use cocoa::foundation::NSString;
            use objc::{class, msg_send, sel, sel_impl};
            use objc::declare::ClassDecl;
            use objc::runtime::{Object, Sel};

            let sender_ptr = Box::into_raw(Box::new(sender));

            // Define a simple target class for the status item click.
            let class_name = "GlobalOverlayStatusItemHandler";
            let class_ptr = unsafe {
                let mut class_opt = objc::runtime::Class::get(class_name);
                if class_opt.is_none() {
                    let mut decl = ClassDecl::new(class_name, class!(NSObject)).unwrap();
                    decl.add_ivar::<*mut std::ffi::c_void>("sender_ptr");

                    extern "C" fn handle_click(this: &Object, _: Sel, _: id) {
                        unsafe {
                            let sender_ptr = *this.get_ivar::<*mut std::ffi::c_void>("sender_ptr")
                                as *mut mpsc::UnboundedSender<()>;
                            let sender = &*sender_ptr;
                            sender.unbounded_send(()).ok();
                        }
                    }
                    decl.add_method(sel!(clicked:), handle_click as extern "C" fn(&Object, Sel, id));
                    class_opt = Some(decl.register());
                }
                class_opt.map_or(std::ptr::null(), |c| c as *const objc::runtime::Class)
            };
            let target: id = unsafe { msg_send![class_ptr, alloc] };
            let target: id = unsafe { msg_send![target, init] };
            unsafe { (*target).set_ivar("sender_ptr", sender_ptr as *mut std::ffi::c_void) };

            let status_bar: id = unsafe { msg_send![class!(NSStatusBar), systemStatusBar] };
            // NSVariableStatusItemLength = -1.0
            let item: id = unsafe { msg_send![status_bar, statusItemWithLength: -1.0f64] };
            let _: id = unsafe { msg_send![item, retain] };
            let button: id = unsafe { msg_send![item, button] };

            let title = unsafe { NSString::alloc(nil).init_str("Z") };
            let _: () = unsafe { msg_send![button, setTitle: title] };

            let _: id = unsafe { msg_send![target, retain] };
            let _: () = unsafe { msg_send![button, setTarget: target] };
            let _: () = unsafe { msg_send![button, setAction: sel!(clicked:)] };

            Self {
                item,
                _target: target,
                _sender_ptr: sender_ptr,
            }
        }
    }

    impl Drop for StatusItem {
        fn drop(&mut self) {
            unsafe {
                use objc::{class, msg_send, sel, sel_impl};
                let status_bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
                let _: () = msg_send![status_bar, removeStatusItem: self.item];
                let _ = Box::from_raw(self._sender_ptr);
                // The target object will be leaked but it's small and only happens once.
            }
        }
    }
}

// ─── Windows ─────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_hotkey {
    use super::*;
    use std::thread;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_SHIFT, RegisterHotKey, UnregisterHotKey, VK_TAB,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, WM_HOTKEY,
    };

    const OVERLAY_HOTKEY_ID: i32 = 1;

    pub struct GlobalHotkey {
        // The thread that pumps the Win32 message loop for hotkey events.
        _pump_thread: thread::JoinHandle<()>,
    }

    impl GlobalHotkey {
        pub fn new(sender: mpsc::UnboundedSender<()>) -> Result<Self> {
            // RegisterHotKey must be called on the same thread that pumps messages
            // for the hotkey, so we spawn a dedicated thread.
            let pump_thread = thread::spawn(move || {
                let result = unsafe {
                    RegisterHotKey(
                        HWND::default(),
                        OVERLAY_HOTKEY_ID,
                        MOD_SHIFT,
                        VK_TAB.0 as u32,
                    )
                };
                if result.is_err() {
                    log::error!(
                        "RegisterHotKey failed: {:?}",
                        windows::core::Error::from_win32()
                    );
                    return;
                }

                // Pump messages until the channel closes.
                let mut msg = MSG::default();
                loop {
                    let ret = unsafe { GetMessageW(&mut msg, HWND::default(), 0, 0) };
                    if ret.0 <= 0 {
                        break;
                    }
                    if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == OVERLAY_HOTKEY_ID {
                        if sender.unbounded_send(()).is_err() {
                            break;
                        }
                    }
                    unsafe { DispatchMessageW(&msg) };
                }

                unsafe { UnregisterHotKey(HWND::default(), OVERLAY_HOTKEY_ID).ok() };
            });

            Ok(Self {
                _pump_thread: pump_thread,
            })
        }
    }
}

// ─── Linux ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_hotkey {
    use super::*;
    use std::thread;

    pub struct GlobalHotkey {
        _grab_thread: thread::JoinHandle<()>,
    }

    impl GlobalHotkey {
        pub fn new(sender: mpsc::UnboundedSender<()>) -> Result<Self> {
            // XGrabKey works only under X11. Under Wayland there is no stable
            // global-hotkey protocol, so we log a warning and return an error.
            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                && std::env::var_os("DISPLAY").is_none()
            {
                return Err(anyhow!(
                    "Global overlay hotkey is not supported under pure Wayland. \
                     Run Zed under XWayland (set DISPLAY) to enable it."
                ));
            }

            let sender = sender.clone();
            let grab_thread = thread::spawn(move || {
                if let Err(error) = run_x11_grab_loop(sender) {
                    log::error!("Global overlay hotkey X11 grab loop failed: {error:#}");
                }
            });

            Ok(Self {
                _grab_thread: grab_thread,
            })
        }
    }

    fn run_x11_grab_loop(sender: mpsc::UnboundedSender<()>) -> Result<()> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            ConnectionExt, EventMask, GrabMode, KEY_PRESS_EVENT, KeyPressEvent, ModMask,
        };
        use x11rb::protocol::Event;

        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| anyhow!("Failed to connect to X11 display: {e}"))?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;

        // Keycode for Tab varies by layout; query it from the keysym XK_Tab (0xff09).
        let xk_tab: u32 = 0xff09;
        let keycodes = conn
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()
            .map_err(|e| anyhow!("GetKeyboardMapping failed: {e}"))?;

        let keysyms_per_keycode = keycodes.keysyms_per_keycode as usize;
        let tab_keycode = keycodes
            .keysyms
            .chunks(keysyms_per_keycode)
            .enumerate()
            .find_map(|(i, syms)| {
                if syms.contains(&xk_tab) {
                    Some(setup.min_keycode + i as u8)
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("Could not find keycode for XK_Tab"))?;

        // Grab Shift+Tab on the root window.
        conn.grab_key(
            false,
            root,
            ModMask::SHIFT,
            tab_keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )?
        .check()
        .map_err(|e| anyhow!("XGrabKey failed: {e}"))?;

        // Also grab with Num Lock (mod2) and Caps Lock (mod_lock) active so the
        // hotkey works regardless of lock key state.
        for extra_mod in [ModMask::M2, ModMask::LOCK] {
            conn.grab_key(
                false,
                root,
                ModMask::SHIFT | extra_mod,
                tab_keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check()
            .map_err(|e| anyhow!("XGrabKey (extra mod) failed: {e}"))?;
        }

        // Register for KeyPress events on the root window.
        conn.change_window_attributes(
            root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::KEY_PRESS),
        )?
        .check()
        .map_err(|e| anyhow!("ChangeWindowAttributes failed: {e}"))?;

        conn.flush()
            .map_err(|e| anyhow!("X11 flush failed: {e}"))?;

        loop {
            let event = conn
                .wait_for_event()
                .map_err(|e| anyhow!("X11 wait_for_event failed: {e}"))?;

            if let Event::KeyPress(KeyPressEvent { detail, .. }) = event {
                if detail == tab_keycode {
                    if sender.unbounded_send(()).is_err() {
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

// ─── Shared: OverlayManager + init() ─────────────────────────────────────────

#[cfg(target_os = "macos")]
use macos_hotkey::GlobalHotkey;
#[cfg(target_os = "windows")]
use windows_hotkey::GlobalHotkey;
#[cfg(target_os = "linux")]
use linux_hotkey::GlobalHotkey;

/// Manages the global overlay window lifecycle: registration of the hotkey,
/// show/hide toggling, and focus restoration.
pub struct OverlayManager {
    overlay_window: AnyWindowHandle,
    is_visible: bool,
    #[cfg(target_os = "macos")]
    previous_app: Option<cocoa::base::id>,
    #[cfg(target_os = "macos")]
    _status_item: Option<macos_hotkey::StatusItem>,
    _hotkey: GlobalHotkey,
}

// Safety: All members are only accessed from the GPUI foreground thread.
unsafe impl Send for OverlayManager {}

struct OverlayManagerGlobal(Arc<Mutex<OverlayManager>>);

impl Global for OverlayManagerGlobal {}

impl OverlayManager {
    pub fn new(
        overlay_window: AnyWindowHandle,
        toggle_sender: mpsc::UnboundedSender<()>,
    ) -> Result<Self> {
        #[cfg(target_os = "macos")]
        let status_item = Some(unsafe { macos_hotkey::StatusItem::new(toggle_sender.clone()) });
        let hotkey = GlobalHotkey::new(toggle_sender)?;
        Ok(Self {
            overlay_window,
            is_visible: false,
            #[cfg(target_os = "macos")]
            previous_app: None,
            #[cfg(target_os = "macos")]
            _status_item: status_item,
            _hotkey: hotkey,
        })
    }

    pub fn toggle(&mut self, cx: &mut App) {
        if self.is_visible {
            self.hide(cx);
        } else {
            self.show(cx);
        }
    }

    fn show(&mut self, cx: &mut App) {
        #[cfg(target_os = "macos")]
        unsafe {
            use objc::{class, msg_send, sel, sel_impl};
            let workspace: cocoa::base::id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let front_app: cocoa::base::id = msg_send![workspace, frontmostApplication];

            if !front_app.is_null() {
                let current_app: cocoa::base::id =
                    msg_send![class!(NSRunningApplication), currentApplication];
                let front_pid: i32 = msg_send![front_app, processIdentifier];
                let current_pid: i32 = msg_send![current_app, processIdentifier];

                // Only record the previous application if it's NOT Zed itself.
                // This prevents "getting stuck" in Zed when toggling while a Zed window is active.
                if front_pid != current_pid {
                    let _: cocoa::base::id = msg_send![front_app, retain];
                    self.previous_app = Some(front_app);
                }
            }
            cx.activate(true);
        }

        #[cfg(not(target_os = "macos"))]
        cx.activate(true);

        self.overlay_window
            .update(cx, |_, window, _| {
                window.activate_window();
            })
            .log_err();

        self.is_visible = true;
    }

    fn hide(&mut self, cx: &mut App) {
        // Hide the application so the overlay disappears and the previously active
        // app is visible. On macOS we then explicitly reactivate the previous app.
        cx.hide();

        #[cfg(target_os = "macos")]
        if let Some(prev_app) = self.previous_app.take() {
            unsafe {
                use objc::{msg_send, sel, sel_impl};
                // NSApplicationActivateIgnoringOtherApps = 1
                let _: () = msg_send![prev_app, activateWithOptions: 1usize];
                // Balance the retain from show().
                let _: () = msg_send![prev_app, release];
            }
        }

        self.is_visible = false;
    }
}

/// Initialises the global overlay hotkey and spawns a task to respond to it.
///
/// Call this once on startup after the overlay window has been created.
pub fn init(overlay_window: AnyWindowHandle, cx: &mut App) {

    let (toggle_tx, mut toggle_rx) = mpsc::unbounded::<()>();

    let manager = match OverlayManager::new(overlay_window, toggle_tx) {
        Ok(manager) => manager,
        Err(error) => {
            error!("Failed to register global overlay hotkey: {error:#}");
            return;
        }
    };

    let manager = Arc::new(Mutex::new(manager));

    // Store the manager as a GPUI global so the action handler below can reach it.
    cx.set_global(OverlayManagerGlobal(manager.clone()));

    // Register shift-tab → ToggleGlobalOverlay for all contexts so it fires when
    // Zed itself is focused (the Carbon hotkey only fires when another app is focused).
    cx.bind_keys([KeyBinding::new(TOGGLE_BINDING, ToggleGlobalOverlay, None)]);

    cx.on_action(|_: &ToggleGlobalOverlay, cx| {
        if let Some(global) = cx.try_global::<OverlayManagerGlobal>() {
            let manager = global.0.clone();
            manager.lock().toggle(cx);
        }
    });

    cx.spawn({
        let manager = manager.clone();
        async move |cx| {
            while let Some(()) = futures::StreamExt::next(&mut toggle_rx).await {
                cx.update(|cx| {
                    manager.lock().toggle(cx);
                });
            }
        }
    })
    .detach();
}
