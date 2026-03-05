use crate::connection::ConnectionOps;
use crate::macos::menu::RepresentedItem;
use crate::macos::{nsstring, nsstring_to_str};
use crate::menu::{Menu, MenuItem};
use crate::{ApplicationEvent, Connection, KeyCode, Modifiers};
use cocoa::appkit::{
    NSApp, NSApplicationActivateIgnoringOtherApps, NSApplicationTerminateReply,
    NSFilenamesPboardType, NSRunningApplication, NSStringPboardType,
};
use cocoa::base::{id, nil};
use cocoa::foundation::NSInteger;
use config::keyassignment::KeyAssignment;
use config::WindowCloseConfirmation;
use objc::declare::ClassDecl;
use objc::rc::StrongPtr;
use objc::runtime::{Class, Object, Sel, BOOL, NO, YES};
use objc::*;
use std::cell::RefCell;
use std::convert::TryFrom;
use std::ffi::c_void;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

use super::keycodes::phys_to_vkey;

const CLS_NAME: &str = "KakuAppDelegate";

type OSStatus = i32;
type EventHotKeyRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

const NO_ERR: OSStatus = 0;
const K_EVENT_CLASS_KEYBOARD: u32 = 0x6b657962;
const K_EVENT_HOT_KEY_PRESSED: u32 = 6;

const HOTKEY_SIGNATURE: u32 = u32::from_be_bytes(*b"KAKU");
const HOTKEY_ID: u32 = 1;

const SHIFT_KEY: u32 = 1 << 9;
const OPTION_KEY: u32 = 1 << 11;
const CONTROL_KEY: u32 = 1 << 12;
const CMD_KEY: u32 = 1 << 8;

unsafe extern "C" {
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        hot_key_code: u32,
        hot_key_modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn GetApplicationEventTarget() -> EventTargetRef;
}

thread_local! {
    static LAST_OPEN_UNTITLED_SPAWN: RefCell<Option<Instant>> = RefCell::new(None);
}

lazy_static::lazy_static! {
    static ref PENDING_SERVICE_OPENS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
    static ref PENDING_TTY_ACTIVATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ref LAST_SERVICE_OPEN_REQUEST: Mutex<Option<Instant>> = Mutex::new(None);
    static ref GLOBAL_HOTKEY_STATE: Mutex<GlobalHotKeyState> =
        Mutex::new(GlobalHotKeyState::default());
}
// macOS can emit applicationOpenUntitledFile twice while no window has
// materialized yet; keep a wider debounce to avoid duplicate SpawnWindow work.
const OPEN_UNTITLED_SPAWN_DEBOUNCE: Duration = Duration::from_millis(1200);
const OPEN_UNTITLED_DEFER_SPAWN_DELAY: Duration = Duration::from_millis(350);
// Guard against startup races: when launched with an explicit file/folder to open,
// macOS may still emit applicationOpenUntitledFile and cause an extra “~/” tab.
// Use 2 seconds to balance startup race protection vs. user intent for new windows.
const OPEN_UNTITLED_AFTER_SERVICE_OPEN_GUARD: Duration = Duration::from_secs(2);
const DISPLAY_CHANGE_RETRY_DELAY: Duration = Duration::from_millis(100);
const DISPLAY_CHANGE_MAX_RETRIES: usize = 5;
static OPEN_UNTITLED_SPAWN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn note_service_open_request() {
    *LAST_SERVICE_OPEN_REQUEST.lock().unwrap() = Some(Instant::now());
    // Cancel any pending deferred untitled spawn from applicationOpenUntitledFile.
    OPEN_UNTITLED_SPAWN_SEQUENCE.fetch_add(1, Ordering::SeqCst);
}

fn should_suppress_open_untitled_spawn() -> bool {
    if !PENDING_SERVICE_OPENS.lock().unwrap().is_empty() {
        return true;
    }

    let now = Instant::now();
    LAST_SERVICE_OPEN_REQUEST
        .lock()
        .unwrap()
        .map(|ts| now.duration_since(ts) < OPEN_UNTITLED_AFTER_SERVICE_OPEN_GUARD)
        .unwrap_or(false)
}

fn schedule_open_untitled_spawn() {
    let sequence = OPEN_UNTITLED_SPAWN_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    promise::spawn::spawn(async move {
        async_io::Timer::after(OPEN_UNTITLED_DEFER_SPAWN_DELAY).await;
        promise::spawn::spawn_into_main_thread(async move {
            if OPEN_UNTITLED_SPAWN_SEQUENCE.load(Ordering::SeqCst) != sequence {
                return;
            }

            if should_suppress_open_untitled_spawn() {
                log::debug!(
                    "Skipping deferred applicationOpenUntitledFile spawn because a service \
                     open request was received"
                );
                return;
            }

            let Some(conn) = Connection::get() else {
                return;
            };

            let has_window = {
                let windows = conn.windows.borrow();
                windows.values().next().is_some()
            };
            if has_window {
                return;
            }

            conn.dispatch_app_event(ApplicationEvent::PerformKeyAssignment(
                KeyAssignment::SpawnWindow,
            ));
        })
        .detach();
    })
    .detach();
}

#[derive(Default)]
struct GlobalHotKeyState {
    handler_installed: bool,
    hotkey_ref: Option<usize>,
}

fn keycode_to_macos_vkey(key: &KeyCode) -> Option<u32> {
    match key {
        KeyCode::RawCode(raw) => u16::try_from(*raw).ok().map(u32::from),
        _ => key.to_phys().and_then(phys_to_vkey).map(u32::from),
    }
}

fn mods_to_carbon_flags(mods: Modifiers) -> u32 {
    let mods = mods.remove_positional_mods();
    let mut flags = 0;
    if mods.contains(Modifiers::SHIFT) {
        flags |= SHIFT_KEY;
    }
    if mods.contains(Modifiers::ALT) {
        flags |= OPTION_KEY;
    }
    if mods.contains(Modifiers::CTRL) {
        flags |= CONTROL_KEY;
    }
    if mods.contains(Modifiers::SUPER) {
        flags |= CMD_KEY;
    }
    flags
}

fn configured_global_hotkey() -> Option<(u32, u32)> {
    let config = config::configuration();
    let hotkey = config.macos_global_hotkey.clone()?;
    let key = hotkey.key.resolve(config.key_map_preference);
    let Some(vkey) = keycode_to_macos_vkey(&key) else {
        log::warn!("macos_global_hotkey key {key:?} cannot be mapped to a macOS virtual key");
        return None;
    };

    let supported = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CTRL | Modifiers::SUPER;
    let cleaned_mods = hotkey.mods.remove_positional_mods();
    let unsupported = cleaned_mods & !supported;
    if unsupported != Modifiers::NONE {
        log::warn!("macos_global_hotkey has unsupported modifiers: {unsupported:?}");
    }

    let flags = mods_to_carbon_flags(cleaned_mods);
    if flags == 0 {
        log::warn!("macos_global_hotkey requires at least one modifier key");
        return None;
    }

    Some((vkey, flags))
}

fn uninstall_registered_hotkey(state: &mut GlobalHotKeyState) {
    if let Some(hotkey_ref) = state.hotkey_ref.take() {
        let status = unsafe { UnregisterEventHotKey(hotkey_ref as EventHotKeyRef) };
        if status != NO_ERR {
            log::warn!("UnregisterEventHotKey failed with status={status}");
        }
    }
}

fn ensure_hotkey_handler_installed(state: &mut GlobalHotKeyState) -> bool {
    if state.handler_installed {
        return true;
    }

    let target = unsafe { GetApplicationEventTarget() };
    if target.is_null() {
        log::warn!("GetApplicationEventTarget returned null");
        return false;
    }

    let spec = EventTypeSpec {
        event_class: K_EVENT_CLASS_KEYBOARD,
        event_kind: K_EVENT_HOT_KEY_PRESSED,
    };
    let mut handler_ref: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        InstallEventHandler(
            target,
            global_hotkey_event_handler,
            1,
            &spec,
            std::ptr::null_mut(),
            &mut handler_ref,
        )
    };
    if status != NO_ERR {
        log::warn!("InstallEventHandler failed with status={status}");
        return false;
    }
    state.handler_installed = true;
    true
}

fn toggle_hotkey_window() {
    let Some(conn) = Connection::get() else {
        log::warn!("global hotkey pressed before GUI connection is ready");
        return;
    };

    let is_active: BOOL = unsafe { msg_send![NSApp(), isActive] };
    if is_active == YES {
        // [NSApp hide:] is a no-op when a window is in native fullscreen.
        // Instead, directly order out each window so it disappears without
        // leaving fullscreen. The window keeps its fullscreen state and will
        // restore on the next hotkey press via makeKeyAndOrderFront:.
        let has_fullscreen = {
            let windows = conn.windows.borrow();
            let mut any_fs = false;
            for window in windows.values() {
                let mut inner = window.borrow_mut();
                if inner.is_fullscreen() {
                    any_fs = true;
                    inner.order_out();
                }
            }
            any_fs
        };
        if has_fullscreen {
            // Deactivate the app so macOS switches to the next app.
            unsafe {
                let () = msg_send![NSApp(), hide: NSApp()];
            }
        } else {
            conn.hide_application();
        }
        return;
    }

    let existing_window = {
        let windows = conn.windows.borrow();
        windows.values().next().cloned()
    };

    if let Some(window) = existing_window {
        unsafe {
            let current_app = NSRunningApplication::currentApplication(nil);
            current_app.activateWithOptions_(NSApplicationActivateIgnoringOtherApps);
        }
        window.borrow_mut().focus();
    } else {
        conn.dispatch_app_event(ApplicationEvent::PerformKeyAssignment(
            KeyAssignment::SpawnWindow,
        ));
    }
}

extern "C" fn global_hotkey_event_handler(
    _next_handler: EventHandlerCallRef,
    _event: EventRef,
    _user_data: *mut c_void,
) -> OSStatus {
    toggle_hotkey_window();
    NO_ERR
}

pub(crate) fn sync_global_hotkey_registration() {
    let mut state = GLOBAL_HOTKEY_STATE.lock().unwrap();

    uninstall_registered_hotkey(&mut state);

    let Some((vkey, flags)) = configured_global_hotkey() else {
        return;
    };

    if !ensure_hotkey_handler_installed(&mut state) {
        return;
    }

    let mut hotkey_ref: EventHotKeyRef = std::ptr::null_mut();
    let hotkey_id = EventHotKeyID {
        signature: HOTKEY_SIGNATURE,
        id: HOTKEY_ID,
    };
    let target = unsafe { GetApplicationEventTarget() };
    let status = unsafe { RegisterEventHotKey(vkey, flags, hotkey_id, target, 0, &mut hotkey_ref) };
    if status != NO_ERR || hotkey_ref.is_null() {
        log::warn!("RegisterEventHotKey failed with status={status}");
        return;
    }

    state.hotkey_ref = Some(hotkey_ref as usize);
}

fn reap_kaku_autofill_helpers() {
    // Best-effort cleanup for macOS helper leaks where AutoFill (Kaku)
    // can accumulate across app restarts.
    const SCRIPT: &str = r#"
for pid in $(pgrep -f 'SafariPlatformSupport.Helper|CredentialProviderExtensionHelper' 2>/dev/null); do
  name=$(lsappinfo info -only name -pid "$pid" 2>/dev/null | sed -n 's/.*"LSDisplayName"="\([^"]*\)".*/\1/p')
  case "$name" in
    *"(Kaku)"*) kill "$pid" 2>/dev/null || true ;;
  esac
done
"#;

    match Command::new("/bin/sh").arg("-c").arg(SCRIPT).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            log::debug!("reap_kaku_autofill_helpers exited with status {}", status);
        }
        Err(err) => {
            log::warn!("reap_kaku_autofill_helpers failed: {err:#}");
        }
    }
}

extern "C" fn application_should_terminate(
    _self: &mut Object,
    _sel: Sel,
    _app: *mut Object,
) -> u64 {
    unsafe {
        match config::configuration().window_close_confirmation {
            WindowCloseConfirmation::NeverPrompt => terminate_now(),
            WindowCloseConfirmation::AlwaysPrompt => {
                let alert: id = msg_send![class!(NSAlert), alloc];
                let alert: id = msg_send![alert, init];
                let message_text = nsstring("Terminate Kaku?");
                let info_text = nsstring("Detach and close all panes and terminate Kaku?");
                let cancel = nsstring("Cancel");
                let ok = nsstring("Ok");

                let () = msg_send![alert, setMessageText: message_text];
                let () = msg_send![alert, setInformativeText: info_text];
                let () = msg_send![alert, addButtonWithTitle: cancel];
                let () = msg_send![alert, addButtonWithTitle: ok];
                #[allow(non_upper_case_globals)]
                const NSModalResponseCancel: NSInteger = 1000;
                #[allow(non_upper_case_globals, dead_code)]
                const NSModalResponseOK: NSInteger = 1001;
                let result: NSInteger = msg_send![alert, runModal];

                if result == NSModalResponseCancel {
                    NSApplicationTerminateReply::NSTerminateCancel as u64
                } else {
                    terminate_now()
                }
            }
        }
    }
}

extern "C" fn application_should_terminate_after_last_window_closed(
    _self: &mut Object,
    _sel: Sel,
    _app: *mut Object,
) -> BOOL {
    // Keep app process alive on macOS after the last window closes,
    // so Dock reopen can create a new window without cold-start.
    NO
}

fn terminate_now() -> u64 {
    // Persist the key (frontmost) window's geometry before the event loop
    // stops. This is the reliable save path for Cmd+Q; window_will_close
    // may not fire for every window before the process exits.
    super::window::on_app_terminating();
    reap_kaku_autofill_helpers();
    uninstall_registered_hotkey(&mut GLOBAL_HOTKEY_STATE.lock().unwrap());
    if let Some(conn) = Connection::get() {
        conn.terminate_message_loop();
    }
    NSApplicationTerminateReply::NSTerminateNow as u64
}

extern "C" fn application_will_finish_launching(
    _self: &mut Object,
    _sel: Sel,
    _notif: *mut Object,
) {
    log::debug!("application_will_finish_launching");
    reap_kaku_autofill_helpers();
}

extern "C" fn application_did_finish_launching(this: &mut Object, _sel: Sel, _notif: *mut Object) {
    log::debug!("application_did_finish_launching");
    unsafe {
        let () = msg_send![NSApp(), setServicesProvider: this as *mut Object];
        (*this).set_ivar("launched", YES);

        // Register for screen wake notifications to update OpenGL contexts.
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let notification_center: id = msg_send![workspace, notificationCenter];
        let notification_name = nsstring("NSWorkspaceScreensDidWakeNotification");
        let () = msg_send![notification_center,
            addObserver: this as *mut Object
            selector: sel!(screensDidWake:)
            name: *notification_name
            object: nil
        ];
        log::debug!("registered for NSWorkspaceScreensDidWakeNotification");

        // Register for display topology changes (monitor connect/disconnect,
        // resolution updates) and refresh all window backends the same way.
        let app_notification_center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        let app_notification_name = nsstring("NSApplicationDidChangeScreenParametersNotification");
        let () = msg_send![app_notification_center,
            addObserver: this as *mut Object
            selector: sel!(screenParametersDidChange:)
            name: *app_notification_name
            object: nil
        ];
        log::debug!("registered for NSApplicationDidChangeScreenParametersNotification");
    }
    sync_global_hotkey_registration();
}

fn refresh_all_window_contexts_after_display_change(
    reason: &'static str,
    remaining_retries: usize,
) {
    let Some(conn) = Connection::get() else {
        return;
    };

    let windows: Vec<_> = conn.windows.borrow().values().cloned().collect();
    let mut busy_windows = false;
    for window in windows {
        if let Ok(mut inner) = window.try_borrow_mut() {
            if !inner.refresh_after_display_change() {
                busy_windows = true;
            }
        } else {
            busy_windows = true;
        }
    }

    if busy_windows && remaining_retries > 0 {
        log::debug!(
            "retrying display refresh after {} because some windows were busy (remaining_retries={})",
            reason,
            remaining_retries
        );
        promise::spawn::spawn_into_main_thread(async move {
            async_io::Timer::after(DISPLAY_CHANGE_RETRY_DELAY).await;
            refresh_all_window_contexts_after_display_change(reason, remaining_retries - 1);
        })
        .detach();
    }
}

/// Called when the system wakes from sleep. Updates all OpenGL contexts to
/// prevent crashes from stale surfaces when AppKit tries to flush the backing layer.
extern "C" fn screens_did_wake(_self: &mut Object, _sel: Sel, _notification: *mut Object) {
    log::debug!("NSWorkspaceScreensDidWakeNotification received, updating OpenGL contexts");
    refresh_all_window_contexts_after_display_change("system wake", DISPLAY_CHANGE_MAX_RETRIES);
}

/// Called when macOS reports a display topology change (screen attach/detach,
/// resolution or arrangement changes). Refreshes all window backends so stale
/// OpenGL drawables and cached screen-dependent values are rebuilt promptly.
extern "C" fn screen_parameters_did_change(
    _self: &mut Object,
    _sel: Sel,
    _notification: *mut Object,
) {
    log::debug!("NSApplicationDidChangeScreenParametersNotification received, refreshing displays");
    refresh_all_window_contexts_after_display_change(
        "screen parameter change",
        DISPLAY_CHANGE_MAX_RETRIES,
    );
}

extern "C" fn application_open_untitled_file(
    this: &mut Object,
    _sel: Sel,
    _app: *mut Object,
) -> BOOL {
    let launched: BOOL = unsafe { *this.get_ivar("launched") };
    if let Some(conn) = Connection::get() {
        if launched == YES {
            let existing_window = {
                let windows = conn.windows.borrow();
                windows.values().next().cloned()
            };
            if let Some(window) = existing_window {
                LAST_OPEN_UNTITLED_SPAWN.with(|last| {
                    last.borrow_mut().take();
                });
                window.borrow_mut().focus();
            } else {
                if should_suppress_open_untitled_spawn() {
                    log::debug!(
                        "Skipping applicationOpenUntitledFile because a service open request \
                         was received recently"
                    );
                    return YES;
                }

                let should_spawn = LAST_OPEN_UNTITLED_SPAWN.with(|last| {
                    let now = Instant::now();
                    let mut last = last.borrow_mut();
                    let elapsed = last.map(|prior| now.duration_since(prior));
                    let too_soon = elapsed
                        .map(|duration| duration < OPEN_UNTITLED_SPAWN_DEBOUNCE)
                        .unwrap_or(false);
                    if too_soon {
                        false
                    } else {
                        *last = Some(now);
                        true
                    }
                });

                if should_spawn {
                    schedule_open_untitled_spawn();
                }
            }
        }
        return YES;
    }
    NO
}

extern "C" fn kaku_perform_key_assignment(_self: &mut Object, _sel: Sel, menu_item: *mut Object) {
    let menu_item = crate::os::macos::menu::MenuItem::with_menu_item(menu_item);
    // Safe because kakuPerformKeyAssignment: is only used with KeyAssignment
    let action = menu_item.get_represented_item();
    log::debug!("kaku_perform_key_assignment {action:?}",);
    match action {
        Some(RepresentedItem::KeyAssignment(action)) => {
            if let Some(conn) = Connection::get() {
                conn.dispatch_app_event(ApplicationEvent::PerformKeyAssignment(action));
            }
        }
        None => {}
    }
}

extern "C" fn show_settings_window(_self: &mut Object, _sel: Sel, _sender: *mut Object) {
    if let Some(conn) = Connection::get() {
        conn.dispatch_app_event(ApplicationEvent::PerformKeyAssignment(
            KeyAssignment::EmitEvent("open-kaku-config".to_string()),
        ));
    }
}

extern "C" fn application_open_file(
    _this: &mut Object,
    _sel: Sel,
    _app: *mut Object,
    file_name: *mut Object,
) -> BOOL {
    let file_name = unsafe { nsstring_to_str(file_name) }.to_string();
    if file_name.is_empty() {
        return NO;
    }

    log::debug!("application_open_file {file_name}");
    dispatch_or_queue_service_open(file_name, true);
    YES
}

extern "C" fn application_open_files(
    _this: &mut Object,
    _sel: Sel,
    app: *mut Object,
    file_names: *mut Object,
) {
    #[allow(non_upper_case_globals)]
    const NSApplicationDelegateReplySuccess: NSInteger = 0;
    #[allow(non_upper_case_globals)]
    const NSApplicationDelegateReplyFailure: NSInteger = 2;

    let mut reply = NSApplicationDelegateReplyFailure;
    let mut dispatched = false;
    unsafe {
        let count: NSInteger = msg_send![file_names, count];
        for i in 0..count {
            let file_name: *mut Object = msg_send![file_names, objectAtIndex: i];
            let file_str = nsstring_to_str(file_name).to_string();
            if file_str.is_empty() {
                continue;
            }
            log::debug!("application_open_files {file_str}");
            dispatch_or_queue_service_open(file_str, true);
            dispatched = true;
        }
    }
    if dispatched {
        reply = NSApplicationDelegateReplySuccess;
    }

    unsafe {
        let target = if app.is_null() { NSApp() } else { app };
        let _: () = msg_send![target, replyToOpenOrPrint: reply];
    }
}

fn first_service_path(pasteboard: *mut Object) -> Option<String> {
    if pasteboard.is_null() {
        return None;
    }

    unsafe {
        let files: id = msg_send![pasteboard, propertyListForType: NSFilenamesPboardType];
        if !files.is_null() {
            let count: NSInteger = msg_send![files, count];
            if count > 0 {
                let file_name: *mut Object = msg_send![files, objectAtIndex: 0];
                if !file_name.is_null() {
                    return Some(nsstring_to_str(file_name).to_string());
                }
            }
        }

        let text: *mut Object = msg_send![pasteboard, stringForType: NSStringPboardType];
        if text.is_null() {
            return None;
        }

        let raw = nsstring_to_str(text).trim().to_string();
        if raw.is_empty() {
            return None;
        }

        if let Ok(url) = Url::parse(&raw) {
            if url.scheme() == "file" {
                if let Ok(path) = url.to_file_path() {
                    return Some(path.to_string_lossy().into_owned());
                }
            }
        }

        Some(raw)
    }
}

fn dispatch_or_queue_service_open(path: String, prefer_existing_window: bool) {
    note_service_open_request();

    if let Some(conn) = Connection::get() {
        if crate::connection::app_event_handler_ready() {
            let event = if prefer_existing_window {
                ApplicationEvent::OpenCommandScriptInTab(path)
            } else {
                ApplicationEvent::OpenCommandScript(path)
            };
            conn.dispatch_app_event(event);
            return;
        }
    }

    if Connection::get().is_some() {
        log::debug!("service request queued until app event handler is ready");
    } else {
        log::debug!("service request queued until GUI connection is ready");
    }
    PENDING_SERVICE_OPENS
        .lock()
        .unwrap()
        .push((path, prefer_existing_window));
}

fn dispatch_or_queue_tty_activation(tty: String) {
    if let Some(conn) = Connection::get() {
        if crate::connection::app_event_handler_ready() {
            conn.dispatch_app_event(ApplicationEvent::ActivatePaneForTty(tty));
            return;
        }
    }

    if Connection::get().is_some() {
        log::debug!("tty activation queued until app event handler is ready");
    } else {
        log::debug!("tty activation queued until GUI connection is ready");
    }
    PENDING_TTY_ACTIVATIONS.lock().unwrap().push(tty);
}

pub(crate) fn flush_pending_service_opens() {
    let pending = {
        let mut queued = PENDING_SERVICE_OPENS.lock().unwrap();
        std::mem::take(&mut *queued)
    };
    let pending_ttys = {
        let mut queued = PENDING_TTY_ACTIVATIONS.lock().unwrap();
        std::mem::take(&mut *queued)
    };

    if pending.is_empty() && pending_ttys.is_empty() {
        return;
    }

    if let Some(conn) = Connection::get() {
        for (path, prefer_existing_window) in pending {
            let event = if prefer_existing_window {
                ApplicationEvent::OpenCommandScriptInTab(path)
            } else {
                ApplicationEvent::OpenCommandScript(path)
            };
            conn.dispatch_app_event(event);
        }
        for tty in pending_ttys {
            conn.dispatch_app_event(ApplicationEvent::ActivatePaneForTty(tty));
        }
    } else {
        let mut queued = PENDING_SERVICE_OPENS.lock().unwrap();
        queued.extend(pending);
        let mut queued_ttys = PENDING_TTY_ACTIVATIONS.lock().unwrap();
        queued_ttys.extend(pending_ttys);
    }
}

fn parse_url_action(url: &Url) -> Option<(&str, String)> {
    if url.scheme() != "kaku" {
        return None;
    }

    let action = url.host_str().filter(|host| !host.is_empty()).or_else(|| {
        url.path_segments()
            .and_then(|mut segments| segments.next())
            .filter(|segment| !segment.is_empty())
    })?;

    if action != "open-tab" {
        return None;
    }

    let tty = url
        .query_pairs()
        .find_map(|(key, value)| (key == "tty").then(|| value.into_owned()))?
        .trim()
        .to_string();
    if tty.is_empty() {
        return None;
    }

    Some((action, tty))
}

extern "C" fn application_open_urls(
    _this: &mut Object,
    _sel: Sel,
    _app: *mut Object,
    urls: *mut Object,
) {
    unsafe {
        let count: NSInteger = msg_send![urls, count];
        for i in 0..count {
            let ns_url: *mut Object = msg_send![urls, objectAtIndex: i];
            let abs_string: *mut Object = msg_send![ns_url, absoluteString];
            if abs_string.is_null() {
                continue;
            }

            let raw_url = nsstring_to_str(abs_string).to_string();
            match Url::parse(&raw_url) {
                Ok(parsed) => {
                    if parsed.scheme() == "file" {
                        match parsed.to_file_path() {
                            Ok(path) => {
                                let path = path.to_string_lossy().into_owned();
                                log::debug!("application_open_urls file {path}");
                                dispatch_or_queue_service_open(path, true);
                            }
                            Err(_) => {
                                log::warn!("application_open_urls invalid file url: {raw_url}");
                            }
                        }
                        continue;
                    }

                    match parse_url_action(&parsed) {
                        Some(("open-tab", tty)) => {
                            log::debug!("application_open_urls open-tab tty={tty}");
                            dispatch_or_queue_tty_activation(tty);
                        }
                        Some((_action, _)) => {}
                        None => {
                            log::warn!("application_open_urls unsupported url: {raw_url}");
                        }
                    }
                }
                Err(err) => {
                    log::warn!("application_open_urls invalid url {raw_url}: {err:#}");
                }
            }
        }
    }
}

extern "C" fn open_in_kaku_service(
    _self: &mut Object,
    _sel: Sel,
    pasteboard: *mut Object,
    _user_data: *mut Object,
    _error: *mut Object,
) {
    let Some(path) = first_service_path(pasteboard) else {
        log::warn!("openInKakuService: Finder provided no usable paths");
        return;
    };

    log::debug!("openInKakuService {path}");
    dispatch_or_queue_service_open(path, true);
}

extern "C" fn open_in_kaku_window_service(
    _self: &mut Object,
    _sel: Sel,
    pasteboard: *mut Object,
    _user_data: *mut Object,
    _error: *mut Object,
) {
    let Some(path) = first_service_path(pasteboard) else {
        log::warn!("openInKakuWindowService: Finder provided no usable paths");
        return;
    };

    log::debug!("openInKakuWindowService {path}");
    dispatch_or_queue_service_open(path, false);
}

extern "C" fn application_dock_menu(
    _self: &mut Object,
    _sel: Sel,
    _app: *mut Object,
) -> *mut Object {
    let dock_menu = Menu::new_with_title("");
    let new_window_item =
        MenuItem::new_with("New Window", Some(sel!(kakuPerformKeyAssignment:)), "");
    new_window_item
        .set_represented_item(RepresentedItem::KeyAssignment(KeyAssignment::SpawnWindow));
    dock_menu.add_item(&new_window_item);
    dock_menu.autorelease()
}

fn get_class() -> &'static Class {
    Class::get(CLS_NAME).unwrap_or_else(|| {
        let mut cls = ClassDecl::new(CLS_NAME, class!(NSObject))
            .expect("Unable to register application delegate class");

        cls.add_ivar::<BOOL>("launched");

        unsafe {
            cls.add_method(
                sel!(applicationShouldTerminate:),
                application_should_terminate as extern "C" fn(&mut Object, Sel, *mut Object) -> u64,
            );
            cls.add_method(
                sel!(applicationShouldTerminateAfterLastWindowClosed:),
                application_should_terminate_after_last_window_closed
                    as extern "C" fn(&mut Object, Sel, *mut Object) -> BOOL,
            );
            cls.add_method(
                sel!(applicationWillFinishLaunching:),
                application_will_finish_launching as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(applicationDidFinishLaunching:),
                application_did_finish_launching as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(application:openFile:),
                application_open_file
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object) -> BOOL,
            );
            cls.add_method(
                sel!(application:openFiles:),
                application_open_files as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            cls.add_method(
                sel!(application:openURLs:),
                application_open_urls as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object),
            );
            cls.add_method(
                sel!(applicationDockMenu:),
                application_dock_menu
                    as extern "C" fn(&mut Object, Sel, *mut Object) -> *mut Object,
            );
            cls.add_method(
                sel!(kakuPerformKeyAssignment:),
                kaku_perform_key_assignment as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            // macOS may route "Settings..." through one of these standard selectors
            // instead of our custom menu-item selector.
            cls.add_method(
                sel!(showSettingsWindow:),
                show_settings_window as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(showPreferencesWindow:),
                show_settings_window as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(applicationOpenUntitledFile:),
                application_open_untitled_file
                    as extern "C" fn(&mut Object, Sel, *mut Object) -> BOOL,
            );
            cls.add_method(
                sel!(openInKakuService:userData:error:),
                open_in_kaku_service
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object, *mut Object),
            );
            cls.add_method(
                sel!(openInKakuWindowService:userData:error:),
                open_in_kaku_window_service
                    as extern "C" fn(&mut Object, Sel, *mut Object, *mut Object, *mut Object),
            );
            cls.add_method(
                sel!(screensDidWake:),
                screens_did_wake as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(screenParametersDidChange:),
                screen_parameters_did_change as extern "C" fn(&mut Object, Sel, *mut Object),
            );
        }

        cls.register()
    })
}

pub fn create_app_delegate() -> StrongPtr {
    let cls = get_class();
    unsafe {
        let delegate: *mut Object = msg_send![cls, alloc];
        let delegate: *mut Object = msg_send![delegate, init];
        (*delegate).set_ivar("launched", NO);
        StrongPtr::new(delegate)
    }
}
