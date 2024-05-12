use rustix::{
    fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd},
    net::SocketAddrAny,
};

use log::{debug, error, info};
use utils::ipc::PixelFormat;

use super::{ObjectId, ObjectManager, WlDynObj};
use std::{
    mem::MaybeUninit,
    num::NonZeroU32,
    path::PathBuf,
    sync::{atomic::AtomicBool, Mutex},
};

pub const WL_DISPLAY: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(1) });
pub const WL_REGISTRY: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(2) });
pub const WL_COMPOSITOR: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(3) });
pub const WL_SHM: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(4) });
pub const WP_VIEWPORTER: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(5) });
pub const ZWLR_LAYER_SHELL_V1: ObjectId = ObjectId(unsafe { NonZeroU32::new_unchecked(6) });

const REQUIRED_GLOBALS: [&str; 4] = [
    "wl_compositor",
    "wl_shm",
    "wp_viewporter",
    "zwlr_layer_shell_v1",
];
const VERSIONS: [u32; 4] = [4, 1, 1, 3];

static mut WAYLAND_FD: OwnedFd = unsafe { std::mem::zeroed() };
static mut FRACTIONAL_SCALE_SUPPORT: bool = false;
static mut OBJECT_MANAGER: MaybeUninit<Mutex<ObjectManager>> = MaybeUninit::uninit();
static mut PIXEL_FORMAT: PixelFormat = PixelFormat::Xrgb;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn wayland_fd() -> BorrowedFd<'static> {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { WAYLAND_FD.as_fd() }
}

#[must_use]
pub fn fractional_scale_support() -> bool {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { FRACTIONAL_SCALE_SUPPORT }
}

#[must_use]
pub fn object_type_get(object_id: ObjectId) -> WlDynObj {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { OBJECT_MANAGER.assume_init_ref() }
        .lock()
        .unwrap()
        .get(object_id)
}

#[must_use]
pub fn object_create(object_type: WlDynObj) -> ObjectId {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { OBJECT_MANAGER.assume_init_ref() }
        .lock()
        .unwrap()
        .create(object_type)
}

pub fn object_remove(object_id: ObjectId) {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { OBJECT_MANAGER.assume_init_ref() }
        .lock()
        .unwrap()
        .remove(object_id)
}

#[must_use]
pub fn pixel_format() -> PixelFormat {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    unsafe { PIXEL_FORMAT }
}

#[must_use]
pub fn wl_shm_format() -> u32 {
    debug_assert!(INITIALIZED.load(std::sync::atomic::Ordering::Relaxed));
    match unsafe { PIXEL_FORMAT } {
        PixelFormat::Xrgb => super::interfaces::wl_shm::format::XRGB8888,
        PixelFormat::Xbgr => super::interfaces::wl_shm::format::XBGR8888,
        PixelFormat::Rgb => super::interfaces::wl_shm::format::RGB888,
        PixelFormat::Bgr => super::interfaces::wl_shm::format::BGR888,
    }
}

pub fn init(pixel_format: Option<PixelFormat>) -> Initializer {
    let mut initializer = Initializer::new(pixel_format);
    if INITIALIZED.load(std::sync::atomic::Ordering::SeqCst) {
        return initializer;
    }

    unsafe {
        WAYLAND_FD = connect();
        OBJECT_MANAGER = MaybeUninit::new(Mutex::new(ObjectManager::new()));
        if let Some(format) = pixel_format {
            info!("Forced usage of wl_shm format: {:?}", format);
            PIXEL_FORMAT = format;
        }
    }
    INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);

    super::interfaces::wl_display::req::get_registry().unwrap();
    super::interfaces::wl_display::req::sync(ObjectId::new(NonZeroU32::new(3).unwrap())).unwrap();

    const IDS: [ObjectId; 4] = [WL_COMPOSITOR, WL_SHM, WP_VIEWPORTER, ZWLR_LAYER_SHELL_V1];

    while !initializer.should_exit {
        let (msg, payload) = super::wire::WireMsg::recv().unwrap();
        if msg.sender_id().get() == 3 {
            super::interfaces::wl_callback::event(&mut initializer, msg, payload);
        } else if msg.sender_id() == WL_DISPLAY {
            super::interfaces::wl_display::event(&mut initializer, msg, payload);
        } else if msg.sender_id() == WL_REGISTRY {
            super::interfaces::wl_registry::event(&mut initializer, msg, payload);
        } else {
            panic!("Did not receive expected global events from registry")
        }
    }

    if let Some((_, missing)) = initializer
        .global_names
        .iter()
        .zip(REQUIRED_GLOBALS)
        .find(|(name, _)| **name == 0)
    {
        panic!("Compositor does not implement required interface: {missing}");
    }

    for (i, name) in initializer.global_names.into_iter().enumerate() {
        let id = IDS[i];
        let interface = REQUIRED_GLOBALS[i];
        let version = VERSIONS[i];
        super::interfaces::wl_registry::req::bind(name, id, interface, version).unwrap();
    }

    if let Some((id, name)) = initializer.fractional_scale.as_ref() {
        unsafe { FRACTIONAL_SCALE_SUPPORT = true };
        super::interfaces::wl_registry::req::bind(
            name.get(),
            *id,
            "wp_fractional_scale_manager_v1",
            1,
        )
        .unwrap();
    }

    let callback_id = initializer.callback_id();
    super::interfaces::wl_display::req::sync(callback_id).unwrap();
    initializer.should_exit = false;
    while !initializer.should_exit {
        let (msg, payload) = super::wire::WireMsg::recv().unwrap();
        match msg.sender_id() {
            // in case there are errors
            WL_DISPLAY => super::interfaces::wl_display::event(&mut initializer, msg, payload),
            WL_REGISTRY => super::interfaces::wl_registry::event(&mut initializer, msg, payload),
            WL_SHM => super::interfaces::wl_shm::event(&mut initializer, msg, payload),
            other => {
                if other == callback_id {
                    super::interfaces::wl_callback::event(&mut initializer, msg, payload);
                } else {
                    error!("received unexpected event from compositor during initialization")
                }
            }
        }
    }

    initializer
}

/// copy-pasted from wayland-client.rs
fn connect() -> OwnedFd {
    if let Ok(txt) = std::env::var("WAYLAND_SOCKET") {
        // We should connect to the provided WAYLAND_SOCKET
        let fd = txt
            .parse::<i32>()
            .expect("invalid fd in WAYLAND_SOCKET env var");
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // remove the variable so any child processes don't see it
        std::env::remove_var("WAYLAND_SOCKET");

        // set the CLOEXEC flag on this FD
        let flags = rustix::io::fcntl_getfd(&fd);
        flags
            .map(|f| f | rustix::io::FdFlags::CLOEXEC)
            .and_then(|f| rustix::io::fcntl_setfd(&fd, f))
            .expect("failed to set flags on socket");

        let socket_addr =
            rustix::net::getsockname(&fd).expect("failed to get wayland socket address");
        if let SocketAddrAny::Unix(addr) = socket_addr {
            rustix::net::connect_unix(&fd, &addr).expect("failed to conenct to unix socket");
            fd
        } else {
            panic!("socket address is not a unix socket");
        }
    } else {
        let socket_name = std::env::var_os("WAYLAND_DISPLAY")
            .map(Into::<PathBuf>::into)
            .expect("failed to detect wayland compositor: WAYLAND_DISPLAY not set");

        let socket_path = if socket_name.is_absolute() {
            socket_name
        } else {
            let mut socket_path = std::env::var_os("XDG_RUNTIME_DIR")
                .map(Into::<PathBuf>::into)
                .expect("failed to detect wayland compositor: XDG_RUNTIME_DIR not set");
            if !socket_path.is_absolute() {
                panic!("failed to detect wayland compositor: socket_path is not absolute");
            }
            socket_path.push(socket_name);
            socket_path
        };

        std::os::unix::net::UnixStream::connect(socket_path)
            .expect("failed to connect to socket")
            .into()
    }
}

/// Helper struct to do all the initialization in this file
pub struct Initializer {
    global_names: [u32; 4],
    output_names: Vec<u32>,
    fractional_scale: Option<(ObjectId, NonZeroU32)>,
    forced_shm_format: bool,
    should_exit: bool,
}

impl Initializer {
    fn new(cli_format: Option<PixelFormat>) -> Self {
        Self {
            global_names: [0; 4],
            output_names: Vec::new(),
            fractional_scale: None,
            forced_shm_format: cli_format.is_some(),
            should_exit: false,
        }
    }

    fn callback_id(&self) -> ObjectId {
        if self.fractional_scale.is_some() {
            ObjectId(unsafe { NonZeroU32::new_unchecked(8) })
        } else {
            ObjectId(unsafe { NonZeroU32::new_unchecked(7) })
        }
    }

    pub fn output_names(&self) -> &[u32] {
        &self.output_names
    }

    pub fn fractional_scale(&self) -> Option<&(ObjectId, NonZeroU32)> {
        self.fractional_scale.as_ref()
    }
}

impl super::interfaces::wl_display::EvHandler for Initializer {
    fn delete_id(&mut self, id: u32) {
        if id == 3 // initial callback for the roundtrip
            || self.fractional_scale.is_none() && id == 7
            || self.fractional_scale.is_some() && id == 8
        {
            self.should_exit = true;
        } else {
            panic!("ObjectId removed during initialization! This should be very rare, which is why we don't deal with it");
        }
    }
}

impl super::interfaces::wl_callback::EvHandler for Initializer {
    fn done(&mut self, sender_id: ObjectId, _callback_data: u32) {
        debug!(
            "Initialization: {} callback done",
            if sender_id.get() == 3 {
                "first"
            } else {
                "second"
            }
        );
    }
}

impl super::interfaces::wl_registry::EvHandler for Initializer {
    fn global(&mut self, name: u32, interface: &str, version: u32) {
        match interface {
            "wp_fractional_scale_manager_v1" => {
                self.fractional_scale = Some((
                    ObjectId(unsafe { NonZeroU32::new_unchecked(7) }),
                    name.try_into().unwrap(),
                ));
            }
            "wl_output" => {
                if version < 4 {
                    error!("wl_output implementation must have at least version 4 for swww-daemon")
                } else {
                    self.output_names.push(name);
                }
            }
            _ => {
                for (i, global) in REQUIRED_GLOBALS.iter().enumerate() {
                    if *global == interface {
                        if version < VERSIONS[i] {
                            panic!(
                                "{interface} version must be at least {} for swww",
                                VERSIONS[i]
                            );
                        }
                        self.global_names[i] = name;
                        break;
                    }
                }
            }
        }
    }

    fn global_remove(&mut self, _name: u32) {
        panic!("Global removed during initialization! This should be very rare, which is why we don't deal with it");
    }
}

impl super::interfaces::wl_shm::EvHandler for Initializer {
    fn format(&mut self, format: u32) {
        match format {
            super::interfaces::wl_shm::format::XRGB8888 => {
                debug!("available shm format: Xrbg");
            }
            super::interfaces::wl_shm::format::XBGR8888 => {
                debug!("available shm format: Xbgr");
                if !self.forced_shm_format && pixel_format() == PixelFormat::Xrgb {
                    unsafe { PIXEL_FORMAT = PixelFormat::Xbgr }
                }
            }
            super::interfaces::wl_shm::format::RGB888 => {
                debug!("available shm format: Rbg");
                if !self.forced_shm_format && pixel_format() != PixelFormat::Bgr {
                    unsafe { PIXEL_FORMAT = PixelFormat::Rgb }
                }
            }
            super::interfaces::wl_shm::format::BGR888 => {
                debug!("available shm format: Bgr");
                if !self.forced_shm_format {
                    unsafe { PIXEL_FORMAT = PixelFormat::Bgr }
                }
            }
            _ => (),
        }
    }
}

impl Drop for Initializer {
    fn drop(&mut self) {
        debug!("Initialization Over");
    }
}
