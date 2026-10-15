//! Installed-module descriptions via libkmod, ported from `src/devices/modulenames.cpp`.
//!
//! libkmod is opened at run time (`dlopen`) like the C++ build's optional backends, so no
//! development headers are needed and its absence only disables module-based naming.
//! The context reads the running kernel's indexes with no modprobe.d overrides and no
//! subprocess. At most [`CACHE_LIMIT`] results, misses included, are kept per instance.
use crate::model::{Attribute, Device, ReadState};
use std::collections::HashMap;
#[cfg(feature = "kmod")]
use std::ffi::c_int;
#[cfg(feature = "kmod")]
use std::ffi::{c_char, c_void, CStr, CString};
#[cfg(feature = "kmod")]
use std::ptr;

pub const CACHE_LIMIT: usize = 512;
#[cfg(feature = "kmod")]
const DESCRIPTION_LIMIT: usize = 4096;

#[derive(Clone, Debug, Default)]
struct Lookup {
    description: Attribute,
    module: String,
}

/// Fresh-context lookup of one installed module by name (no cache), as the Driver tab does.
pub fn module_details(name: &str) -> ModuleLookup {
    match Kmod::open() {
        Some(kmod) => kmod.details(name),
        None => ModuleLookup::Unsupported,
    }
}

/// Worker-owned libkmod context plus a per-refresh lookup cache.
#[derive(Default)]
pub struct ModuleNames {
    kmod: Option<Kmod>,
    cache: HashMap<String, Lookup>,
}

#[cfg(feature = "kmod")]
fn error_attribute(status: c_int) -> Attribute {
    let error = -status;
    let state = match error {
        libc::EACCES | libc::EPERM => ReadState::PermissionDenied,
        libc::ENOENT => ReadState::Unavailable,
        _ => ReadState::Error,
    };
    Attribute { state, value: String::new(), error }
}

/// Everything libkmod knows about one installed module, for the Driver tab.
pub struct ModuleDetails {
    pub filename: Attribute,
    /// `modinfo` key/value pairs as bounded reads (a field over 4096 bytes is an error value).
    pub info: Result<Vec<(Attribute, Attribute)>, i32>,
    /// More than 4096 entries were present; the list is cut off.
    pub too_many: bool,
}

pub enum ModuleLookup {
    /// libkmod is not built in or could not be opened.
    Unsupported,
    /// Lookup failed with this errno.
    Failed(i32),
    /// The lookup succeeded but returned no module.
    Missing,
    Found(ModuleDetails),
}

/// A text field no longer than `DESCRIPTION_LIMIT`, else an overflow error value.
#[cfg(feature = "kmod")]
fn bounded_field(bytes: &[u8]) -> Attribute {
    if bytes.len() > DESCRIPTION_LIMIT {
        Attribute { state: ReadState::Error, value: String::new(), error: libc::EOVERFLOW }
    } else {
        Attribute { state: ReadState::Available, value: String::from_utf8_lossy(bytes).into_owned(), error: 0 }
    }
}

#[cfg(feature = "kmod")]
mod backend {
    use super::*;
    type NewFn = unsafe extern "C" fn(*const c_char, *const *const c_char) -> *mut c_void;
    type UnrefFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type LookupNameFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut *mut c_void) -> c_int;
    type LookupFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut *mut c_void) -> c_int;
    type ListNextFn = unsafe extern "C" fn(*const c_void, *const c_void) -> *mut c_void;
    type GetModuleFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
    type UnrefListFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type GetNameFn = unsafe extern "C" fn(*const c_void) -> *const c_char;
    type GetPathFn = unsafe extern "C" fn(*const c_void) -> *const c_char;
    type GetInfoFn = unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> c_int;
    type InfoStrFn = unsafe extern "C" fn(*const c_void) -> *const c_char;
    type InfoFreeFn = unsafe extern "C" fn(*mut c_void);

    pub struct Kmod {
        // Keeps the shared object mapped for as long as the function pointers below live.
        _library: libloading::Library,
        context: *mut c_void,
        unref: UnrefFn,
        module_unref: UnrefFn,
        lookup_name: LookupNameFn,
        lookup: LookupFn,
        list_next: ListNextFn,
        get_module: GetModuleFn,
        unref_list: UnrefListFn,
        get_name: GetNameFn,
        get_path: GetPathFn,
        get_info: GetInfoFn,
        info_key: InfoStrFn,
        info_value: InfoStrFn,
        info_free: InfoFreeFn,
    }

    impl Kmod {
        pub fn open() -> Option<Self> {
            // SAFETY: loading libkmod runs only its own initializers; every symbol is resolved
            // with the exact C signature declared in libkmod.h before use.
            unsafe {
                let library = libloading::Library::new("libkmod.so.2").ok()?;
                macro_rules! sym {
                    ($name:literal, $ty:ty) => {
                        *library.get::<$ty>(concat!($name, "\0").as_bytes()).ok()?
                    };
                }
                let new: NewFn = sym!("kmod_new", NewFn);
                let no_configuration: [*const c_char; 1] = [ptr::null()];
                let context = new(ptr::null(), no_configuration.as_ptr());
                if context.is_null() {
                    return None;
                }
                Some(Self {
                    unref: sym!("kmod_unref", UnrefFn),
                    module_unref: sym!("kmod_module_unref", UnrefFn),
                    lookup_name: sym!("kmod_module_new_from_name_lookup", LookupNameFn),
                    lookup: sym!("kmod_module_new_from_lookup", LookupFn),
                    list_next: sym!("kmod_list_next", ListNextFn),
                    get_module: sym!("kmod_module_get_module", GetModuleFn),
                    unref_list: sym!("kmod_module_unref_list", UnrefListFn),
                    get_name: sym!("kmod_module_get_name", GetNameFn),
                    get_path: sym!("kmod_module_get_path", GetPathFn),
                    get_info: sym!("kmod_module_get_info", GetInfoFn),
                    info_key: sym!("kmod_module_info_get_key", InfoStrFn),
                    info_value: sym!("kmod_module_info_get_value", InfoStrFn),
                    info_free: sym!("kmod_module_info_free_list", InfoFreeFn),
                    context,
                    _library: library,
                })
            }
        }
    }

    impl Drop for Kmod {
        fn drop(&mut self) {
            // SAFETY: `context` came from kmod_new and is released exactly once, before the
            // library field is dropped.
            unsafe {
                (self.unref)(self.context);
            }
        }
    }

    impl Kmod {
        pub fn details(&self, name: &str) -> ModuleLookup {
            let Ok(encoded) = CString::new(name) else { return ModuleLookup::Failed(libc::EINVAL) };
            // SAFETY: as in `describe`: valid pointers for each call, every libkmod object
            // released on every path.
            unsafe {
                let mut module: *mut c_void = ptr::null_mut();
                let status = (self.lookup_name)(self.context, encoded.as_ptr(), &mut module);
                if status < 0 {
                    return ModuleLookup::Failed(-status);
                }
                if module.is_null() {
                    return ModuleLookup::Missing;
                }
                let path = (self.get_path)(module);
                let filename = if path.is_null() { Attribute::default() } else { bounded_field(CStr::from_ptr(path).to_bytes()) };
                let mut list: *mut c_void = ptr::null_mut();
                let count = (self.get_info)(module, &mut list);
                let mut too_many = false;
                let info = if count < 0 {
                    Err(-count)
                } else {
                    let mut entries = Vec::new();
                    let mut entry = list;
                    while !entry.is_null() {
                        if entries.len() >= 4096 {
                            too_many = true;
                            break;
                        }
                        let (key, value) = ((self.info_key)(entry), (self.info_value)(entry));
                        let field = |p: *const c_char| {
                            if p.is_null() {
                                Attribute::default()
                            } else {
                                bounded_field(CStr::from_ptr(p).to_bytes())
                            }
                        };
                        entries.push((field(key), field(value)));
                        entry = (self.list_next)(list, entry);
                    }
                    Ok(entries)
                };
                if !list.is_null() {
                    (self.info_free)(list);
                }
                (self.module_unref)(module);
                ModuleLookup::Found(ModuleDetails { filename, info, too_many })
            }
        }

        pub fn describe(&self, key: &str, exact_name: bool) -> Lookup {
            let kmod = self;
            let mut result = Lookup::default();
            let Ok(encoded) = CString::new(key) else { return result };
            // SAFETY: all pointers passed are valid for the call; every object libkmod hands
            // back is released with its matching unref/free function on every path below.
            unsafe {
                let mut module: *mut c_void = ptr::null_mut();
                if exact_name {
                    let status = (kmod.lookup_name)(kmod.context, encoded.as_ptr(), &mut module);
                    if status < 0 {
                        result.description = error_attribute(status);
                    }
                } else {
                    let mut matches: *mut c_void = ptr::null_mut();
                    let status = (kmod.lookup)(kmod.context, encoded.as_ptr(), &mut matches);
                    // Never select an arbitrary first module from ambiguous aliases.
                    if status == 0 && !matches.is_null() && (kmod.list_next)(matches, matches).is_null() {
                        module = (kmod.get_module)(matches);
                    } else if status < 0 {
                        result.description = error_attribute(status);
                    }
                    if !matches.is_null() {
                        (kmod.unref_list)(matches);
                    }
                }
                if !module.is_null() {
                    let name = (kmod.get_name)(module);
                    if !name.is_null() {
                        result.module = CStr::from_ptr(name).to_string_lossy().into_owned();
                    }
                    let mut info: *mut c_void = ptr::null_mut();
                    let status = (kmod.get_info)(module, &mut info);
                    if status < 0 {
                        result.description = error_attribute(status);
                    } else {
                        let mut entry = info;
                        while !entry.is_null() {
                            let (field, value) = ((kmod.info_key)(entry), (kmod.info_value)(entry));
                            if !field.is_null() && !value.is_null() && CStr::from_ptr(field).to_bytes() == b"description" {
                                let bytes = CStr::from_ptr(value).to_bytes();
                                if bytes.len() > DESCRIPTION_LIMIT {
                                    result.description = Attribute { state: ReadState::Error, value: String::new(), error: libc::EOVERFLOW };
                                    break;
                                }
                                let text = String::from_utf8_lossy(bytes).trim().to_string();
                                if !text.is_empty() {
                                    result.description = Attribute { state: ReadState::Available, value: text, error: 0 };
                                    break;
                                }
                            }
                            entry = (kmod.list_next)(info, entry);
                        }
                    }
                    if !info.is_null() {
                        (kmod.info_free)(info);
                    }
                    (kmod.module_unref)(module);
                }
            }
            result
        }
    }
}
#[cfg(not(feature = "kmod"))]
mod backend {
    use super::*;

    pub struct Kmod;

    impl Kmod {
        pub fn open() -> Option<Self> {
            None
        }

        pub fn describe(&self, _key: &str, _exact_name: bool) -> Lookup {
            Lookup::default()
        }

        pub fn details(&self, _name: &str) -> ModuleLookup {
            ModuleLookup::Unsupported
        }
    }
}
use backend::Kmod;

impl ModuleNames {
    pub fn new() -> Self {
        Self { kmod: Kmod::open(), cache: HashMap::new() }
    }

    pub fn available(&self) -> bool {
        self.kmod.is_some()
    }

    fn lookup(&mut self, key: &str, exact_name: bool) -> Lookup {
        let cache_key = format!("{}{key}", if exact_name { "module:" } else { "alias:" });
        if let Some(hit) = self.cache.get(&cache_key) {
            return hit.clone();
        }
        let Some(kmod) = &self.kmod else { return Lookup::default() };
        if self.cache.len() >= CACHE_LIMIT {
            return Lookup::default();
        }
        let result = kmod.describe(key, exact_name);
        if self.cache.len() < CACHE_LIMIT {
            self.cache.insert(cache_key, result.clone()); // Cache misses too.
        }
        result
    }

    /// Supplements a device that still has only a kernel-derived name. A direct
    /// `driver/module` link is evidence; a unique modalias match is only a candidate.
    pub fn supplement(&mut self, d: &mut Device) {
        if self.kmod.is_none() {
            return;
        }
        if d.subsystem == "cpu" || (d.name_source != "kernel name" && d.name_source != "direct kernel driver and kernel name") {
            return;
        }
        let link = std::fs::symlink_metadata(format!("{}/driver/module", d.path));
        let module = if link.is_ok() { d.driver_module.clone() } else { String::new() };
        let candidate = module.is_empty();
        let key = if candidate { d.properties.get("MODALIAS").cloned().unwrap_or_default() } else { module };
        if key.is_empty() {
            return; // No module link does not prove a built-in driver.
        }
        let result = self.lookup(&key, !candidate);
        d.attributes.insert("module_description".into(), result.description.clone());
        if result.description.state != ReadState::Available {
            return;
        }
        d.name = result.description.value;
        d.name_candidate = candidate;
        d.name_source = format!(
            "installed module metadata: {}; {}: {key}",
            result.module,
            if candidate { "unique modalias candidate" } else { "direct driver/module link" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lookup_resolves_a_loaded_module_when_libkmod_is_present() {
        let mut names = ModuleNames::new();
        if !names.available() {
            return; // libkmod.so.2 absent: naming degrades silently, as designed.
        }
        let modules = std::fs::read_to_string("/proc/modules").unwrap_or_default();
        let Some(module) = modules.lines().next().and_then(|l| l.split(' ').next()) else { return };
        let found = names.lookup(module, true);
        assert_eq!(found.module, module);
        // Second call is served from the cache and is identical.
        assert_eq!(names.lookup(module, true).module, module);
        assert_eq!(names.cache.len(), 1);
    }

    #[test]
    fn missing_module_is_cached_and_not_an_available_description() {
        let mut names = ModuleNames::new();
        if !names.available() {
            return;
        }
        let miss = names.lookup("definitely_not_a_module_xyz", true);
        assert_ne!(miss.description.state, ReadState::Available);
        assert_eq!(names.cache.len(), 1);
    }

    #[test]
    fn supplement_ignores_devices_that_already_have_a_real_name() {
        let mut names = ModuleNames::new();
        let mut d = Device { name: "Real".into(), name_source: "udev: ID_MODEL".into(), ..Default::default() };
        names.supplement(&mut d);
        assert_eq!(d.name, "Real");
        assert!(!d.attributes.contains_key("module_description"));
    }
}
