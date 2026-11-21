//! Secrets in the desktop's keyring (GNOME Keyring, KWallet, ...) through
//! the freedesktop Secret Service, via libsecret.
//!
//! libsecret is loaded with `dlopen`, so it is not a build dependency; when
//! it, or a keyring service, is missing, every call returns an error and the
//! caller falls back. Items are found by a schema name plus an `account`
//! attribute.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

const SCHEMA_NAME: &CStr = c"io.gitlab.veneer_project.Veneer";

#[repr(C)]
#[derive(Clone, Copy)]
struct SchemaAttribute {
    name: *const c_char,
    /// `SECRET_SCHEMA_ATTRIBUTE_STRING` = 0.
    kind: c_int,
}

/// `SecretSchema` from libsecret's `secret-schema.h`.
#[repr(C)]
struct Schema {
    name: *const c_char,
    /// `SECRET_SCHEMA_NONE` = 0.
    flags: c_int,
    attributes: [SchemaAttribute; 32],
    reserved: c_int,
    reserved_pointers: [*const c_void; 7],
}

#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}

type Store = unsafe extern "C" fn(
    *const Schema,
    *const c_char, // collection (null: the default one)
    *const c_char, // label
    *const c_char, // password
    *mut c_void,   // cancellable
    *mut *mut GError,
    ...
) -> c_int;
type Lookup = unsafe extern "C" fn(*const Schema, *mut c_void, *mut *mut GError, ...) -> *mut c_char;
type Clear = unsafe extern "C" fn(*const Schema, *mut c_void, *mut *mut GError, ...) -> c_int;

struct Api {
    schema: Box<Schema>,
    store: Store,
    lookup: Lookup,
    clear: Clear,
    free: unsafe extern "C" fn(*mut c_char),
    error_free: unsafe extern "C" fn(*mut GError),
}

// The schema only points at static strings, and libsecret's sync calls may
// be made from any thread.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libsecret-1.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            return None;
        }
        // `dlsym` on the handle also searches its dependencies (GLib).
        let sym = |name: &CStr| {
            let p = libc::dlsym(lib, name.as_ptr());
            (!p.is_null()).then_some(p)
        };
        let mut attributes = [SchemaAttribute { name: std::ptr::null(), kind: 0 }; 32];
        attributes[0] = SchemaAttribute { name: c"account".as_ptr(), kind: 0 };
        Some(Api {
            schema: Box::new(Schema {
                name: SCHEMA_NAME.as_ptr(),
                flags: 0,
                attributes,
                reserved: 0,
                reserved_pointers: [std::ptr::null(); 7],
            }),
            store: std::mem::transmute(sym(c"secret_password_store_sync")?),
            lookup: std::mem::transmute(sym(c"secret_password_lookup_sync")?),
            clear: std::mem::transmute(sym(c"secret_password_clear_sync")?),
            free: std::mem::transmute(sym(c"secret_password_free")?),
            error_free: std::mem::transmute(sym(c"g_error_free")?),
        })
    })
    .as_ref()
    .ok_or_else(|| "libsecret is not installed".to_string())
}

/// Turn a GError into a message and free it; `Ok` if there was none.
unsafe fn check(api: &Api, error: *mut GError) -> Result<(), String> {
    if error.is_null() {
        return Ok(());
    }
    let message = unsafe {
        let m = (*error).message;
        let text = if m.is_null() { "unknown error".into() } else { CStr::from_ptr(m).to_string_lossy().into_owned() };
        (api.error_free)(error);
        text
    };
    Err(message)
}

fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "text with a NUL byte".to_string())
}

/// Save `secret` under `account`, replacing what was there.
pub fn store(account: &str, label: &str, secret: &str) -> Result<(), String> {
    let api = api()?;
    let (account, label, secret) = (c(account)?, c(label)?, c(secret)?);
    let mut error = std::ptr::null_mut();
    let ok = unsafe {
        (api.store)(
            &*api.schema,
            std::ptr::null(),
            label.as_ptr(),
            secret.as_ptr(),
            std::ptr::null_mut(),
            &mut error,
            c"account".as_ptr(),
            account.as_ptr(),
            std::ptr::null::<c_char>(),
        )
    };
    unsafe { check(api, error)? };
    if ok == 0 {
        return Err("the keyring refused to store the item".into());
    }
    Ok(())
}

/// The secret saved under `account`, if any.
pub fn lookup(account: &str) -> Result<Option<String>, String> {
    let api = api()?;
    let account = c(account)?;
    let mut error = std::ptr::null_mut();
    let found = unsafe {
        (api.lookup)(
            &*api.schema,
            std::ptr::null_mut(),
            &mut error,
            c"account".as_ptr(),
            account.as_ptr(),
            std::ptr::null::<c_char>(),
        )
    };
    unsafe { check(api, error)? };
    if found.is_null() {
        return Ok(None);
    }
    let secret = unsafe { CStr::from_ptr(found) }.to_string_lossy().into_owned();
    // Wipes the copy libsecret made.
    unsafe { (api.free)(found) };
    Ok(Some(secret))
}

/// Delete what is saved under `account` (fine if nothing is).
pub fn clear(account: &str) -> Result<(), String> {
    let api = api()?;
    let account = c(account)?;
    let mut error = std::ptr::null_mut();
    unsafe {
        (api.clear)(
            &*api.schema,
            std::ptr::null_mut(),
            &mut error,
            c"account".as_ptr(),
            account.as_ptr(),
            std::ptr::null::<c_char>(),
        );
        check(api, error)
    }
}

#[cfg(test)]
mod tests {
    //! Needs a running keyring (GNOME Keyring, KWallet, ...); uses its own
    //! test item, never the real login. Run with `-- --include-ignored`.
    use super::*;

    #[test]
    #[ignore]
    fn store_lookup_replace_clear() {
        let account = format!("veneer-test-{}", std::process::id());
        assert_eq!(lookup(&account).unwrap(), None);
        store(&account, "Veneer test item", "first").unwrap();
        assert_eq!(lookup(&account).unwrap().as_deref(), Some("first"));
        store(&account, "Veneer test item", "second").unwrap();
        assert_eq!(lookup(&account).unwrap().as_deref(), Some("second"));
        clear(&account).unwrap();
        assert_eq!(lookup(&account).unwrap(), None);
        clear(&account).unwrap();
    }
}
