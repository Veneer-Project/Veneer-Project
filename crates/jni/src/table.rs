//! The `JNIEnv` / `JavaVM` function tables handed to the guest.

use std::collections::HashMap;
use std::ffi::{CStr, CString, VaList, c_char, c_void};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    Class, Field, Instance, JObject, JValue, Method, Object, PrimArray, Ref, delete_local,
    elem_size, from_raw, new_string, pending_exception, pop_frame, push_frame,
    set_pending_exception, to_local, vm,
};

#[repr(C)]
pub struct JNIEnv {
    functions: *const usize,
}

#[repr(C)]
pub struct JavaVM {
    functions: *const usize,
}

const JNI_VERSION_1_6: i32 = 0x0001_0006;
const ENV_SLOTS: usize = 233;

type Env = *mut JNIEnv;
type JSize = i32;

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

unsafe fn obj(o: JObject) -> Option<&'static Object> {
    if o.is_null() {
        None
    } else {
        Some(unsafe { &*(o as *const Object) })
    }
}

unsafe fn class_of(o: JObject) -> &'static Class {
    match unsafe { obj(o) } {
        Some(Object::Class(c)) => c,
        Some(other) => other.class(),
        None => vm().class("java/lang/Object"),
    }
}

/// Decode JNI "modified UTF-8" (CESU-8 with an overlong NUL).
fn decode_mutf8(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            units.push(b as u16);
            i += 1;
        } else if b & 0xe0 == 0xc0 && i + 1 < bytes.len() {
            units.push(((b as u16 & 0x1f) << 6) | (bytes[i + 1] as u16 & 0x3f));
            i += 2;
        } else if b & 0xf0 == 0xe0 && i + 2 < bytes.len() {
            units.push(
                ((b as u16 & 0x0f) << 12)
                    | ((bytes[i + 1] as u16 & 0x3f) << 6)
                    | (bytes[i + 2] as u16 & 0x3f),
            );
            i += 3;
        } else if b & 0xf8 == 0xf0 && i + 3 < bytes.len() {
            let cp = ((b as u32 & 0x07) << 18)
                | ((bytes[i + 1] as u32 & 0x3f) << 12)
                | ((bytes[i + 2] as u32 & 0x3f) << 6)
                | (bytes[i + 3] as u32 & 0x3f);
            let c = char::from_u32(cp).unwrap_or('\u{fffd}');
            let mut buf = [0u16; 2];
            units.extend_from_slice(c.encode_utf16(&mut buf));
            i += 4;
        } else {
            units.push(0xfffd);
            i += 1;
        }
    }
    String::from_utf16_lossy(&units)
}

// --- return / element type plumbing ---------------------------------------------

/// A type that can be returned from a `Call*Method` family function.
pub(crate) trait Ret {
    fn from_jvalue(v: JValue) -> Self;
}

impl Ret for () {
    fn from_jvalue(_: JValue) {}
}
impl Ret for JObject {
    fn from_jvalue(v: JValue) -> JObject {
        match v {
            JValue::Object(o) => to_local(o),
            _ => std::ptr::null_mut(),
        }
    }
}

/// A JNI primitive type.
pub(crate) trait Prim: Copy + 'static {
    const SIG: u8;
    fn from_j(v: &JValue) -> Self;
    fn to_j(self) -> JValue;
}

macro_rules! prim {
    ($t:ty, $sig:expr, $from:expr, $to:expr) => {
        impl Prim for $t {
            const SIG: u8 = $sig;
            fn from_j(v: &JValue) -> Self {
                ($from)(v)
            }
            fn to_j(self) -> JValue {
                ($to)(self)
            }
        }
        impl Ret for $t {
            fn from_jvalue(v: JValue) -> Self {
                <$t as Prim>::from_j(&v)
            }
        }
    };
}

prim!(u8, b'Z', |v: &JValue| (v.as_i64() != 0) as u8, |v: u8| JValue::Bool(v != 0));
prim!(i8, b'B', |v: &JValue| v.as_i64() as i8, JValue::Byte);
prim!(u16, b'C', |v: &JValue| v.as_i64() as u16, JValue::Char);
prim!(i16, b'S', |v: &JValue| v.as_i64() as i16, JValue::Short);
prim!(i32, b'I', |v: &JValue| v.as_i64() as i32, JValue::Int);
prim!(i64, b'J', |v: &JValue| v.as_i64(), JValue::Long);
prim!(f32, b'F', |v: &JValue| v.as_f64() as f32, JValue::Float);
prim!(f64, b'D', |v: &JValue| v.as_f64(), JValue::Double);

// --- argument marshalling ---------------------------------------------------------

unsafe fn args_va(m: &Method, ap: &mut VaList) -> Vec<JValue> {
    m.params
        .iter()
        .map(|p| unsafe {
            match p {
                b'Z' => JValue::Bool(ap.next_arg::<i32>() != 0),
                b'B' => JValue::Byte(ap.next_arg::<i32>() as i8),
                b'C' => JValue::Char(ap.next_arg::<i32>() as u16),
                b'S' => JValue::Short(ap.next_arg::<i32>() as i16),
                b'I' => JValue::Int(ap.next_arg::<i32>()),
                b'J' => JValue::Long(ap.next_arg::<i64>()),
                b'F' => JValue::Float(ap.next_arg::<f64>() as f32),
                b'D' => JValue::Double(ap.next_arg::<f64>()),
                _ => JValue::Object(from_raw(ap.next_arg::<*mut c_void>())),
            }
        })
        .collect()
}

unsafe fn args_array(m: &Method, args: *const u64) -> Vec<JValue> {
    m.params
        .iter()
        .enumerate()
        .map(|(i, p)| unsafe {
            let raw = args.add(i);
            match p {
                b'Z' => JValue::Bool(*(raw as *const u8) != 0),
                b'B' => JValue::Byte(*(raw as *const i8)),
                b'C' => JValue::Char(*(raw as *const u16)),
                b'S' => JValue::Short(*(raw as *const i16)),
                b'I' => JValue::Int(*(raw as *const i32)),
                b'J' => JValue::Long(*(raw as *const i64)),
                b'F' => JValue::Float(*(raw as *const f32)),
                b'D' => JValue::Double(*(raw as *const f64)),
                _ => JValue::Object(from_raw(*(raw as *const JObject))),
            }
        })
        .collect()
}

unsafe fn dispatch(this: JObject, m: *const Method, args: Vec<JValue>) -> JValue {
    let m = unsafe { &*m };
    let this = if m.is_static { None } else { unsafe { from_raw(this) } };
    vm().invoke(this.as_ref(), m, &args)
}

// Call<Type>Method / CallStatic<Type>Method (the second argument is the
// receiver or the class; statics ignore it).
unsafe extern "C" fn call<R: Ret>(_e: Env, o: JObject, m: *const Method, mut ap: ...) -> R {
    let args = unsafe { args_va(&*m, &mut ap) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}
unsafe extern "C" fn call_v<R: Ret>(_e: Env, o: JObject, m: *const Method, mut ap: VaList) -> R {
    let args = unsafe { args_va(&*m, &mut ap) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}
unsafe extern "C" fn call_a<R: Ret>(_e: Env, o: JObject, m: *const Method, a: *const u64) -> R {
    let args = unsafe { args_array(&*m, a) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}

// CallNonvirtual<Type>Method
unsafe extern "C" fn call_nv<R: Ret>(
    _e: Env,
    o: JObject,
    _c: JObject,
    m: *const Method,
    mut ap: ...
) -> R {
    let args = unsafe { args_va(&*m, &mut ap) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}
unsafe extern "C" fn call_nv_v<R: Ret>(
    _e: Env,
    o: JObject,
    _c: JObject,
    m: *const Method,
    mut ap: VaList,
) -> R {
    let args = unsafe { args_va(&*m, &mut ap) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}
unsafe extern "C" fn call_nv_a<R: Ret>(
    _e: Env,
    o: JObject,
    _c: JObject,
    m: *const Method,
    a: *const u64,
) -> R {
    let args = unsafe { args_array(&*m, a) };
    R::from_jvalue(unsafe { dispatch(o, m, args) })
}

// --- object creation ----------------------------------------------------------------

fn instantiate(class: &'static Class) -> Ref {
    Arc::new(Object::Instance(Instance {
        class,
        fields: Mutex::new(HashMap::new()),
        data: Mutex::new(None),
    }))
}

unsafe fn construct(clazz: JObject, m: *const Method, args: Vec<JValue>) -> JObject {
    let class = unsafe { class_of(clazz) };
    let m = unsafe { &*m };
    // A registered constructor may return a replacement object (e.g. a
    // host-backed string); otherwise a plain instance is created.
    let this = instantiate(class);
    match vm().invoke(Some(&this), m, &args) {
        JValue::Object(Some(replacement)) => to_local(Some(replacement)),
        _ => to_local(Some(this)),
    }
}

unsafe extern "C" fn alloc_object(_e: Env, clazz: JObject) -> JObject {
    to_local(Some(instantiate(unsafe { class_of(clazz) })))
}
unsafe extern "C" fn new_object(_e: Env, c: JObject, m: *const Method, mut ap: ...) -> JObject {
    let args = unsafe { args_va(&*m, &mut ap) };
    unsafe { construct(c, m, args) }
}
unsafe extern "C" fn new_object_v(_e: Env, c: JObject, m: *const Method, mut ap: VaList) -> JObject {
    let args = unsafe { args_va(&*m, &mut ap) };
    unsafe { construct(c, m, args) }
}
unsafe extern "C" fn new_object_a(_e: Env, c: JObject, m: *const Method, a: *const u64) -> JObject {
    let args = unsafe { args_array(&*m, a) };
    unsafe { construct(c, m, args) }
}

// --- classes, methods, fields ---------------------------------------------------------

unsafe extern "C" fn get_version(_e: Env) -> i32 {
    JNI_VERSION_1_6
}

unsafe extern "C" fn find_class(_e: Env, name: *const c_char) -> JObject {
    let name = cstr(name);
    if !vm().has_class(&name) {
        log::debug!("FindClass({name}) [new placeholder]");
    }
    to_local(Some(vm().class(&name).object()))
}

unsafe extern "C" fn get_superclass(_e: Env, clazz: JObject) -> JObject {
    let c = unsafe { class_of(clazz) };
    if c.name == "java/lang/Object" {
        return std::ptr::null_mut();
    }
    let sup = c.supers.lock().unwrap().first().cloned();
    to_local(Some(vm().class(sup.as_deref().unwrap_or("java/lang/Object")).object()))
}

unsafe extern "C" fn is_assignable_from(_e: Env, a: JObject, b: JObject) -> u8 {
    unsafe { class_of(a).is_subclass_of(&class_of(b).name) as u8 }
}

unsafe extern "C" fn get_object_class(_e: Env, o: JObject) -> JObject {
    match unsafe { obj(o) } {
        Some(o) => to_local(Some(o.class().object())),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn is_instance_of(_e: Env, o: JObject, clazz: JObject) -> u8 {
    match unsafe { obj(o) } {
        // null is an instance of everything, per the JNI spec.
        None => 1,
        Some(o) => o.class().is_subclass_of(&unsafe { class_of(clazz) }.name) as u8,
    }
}

unsafe extern "C" fn get_method_id(
    _e: Env,
    clazz: JObject,
    name: *const c_char,
    sig: *const c_char,
) -> *const Method {
    let c = unsafe { class_of(clazz) };
    let (name, sig) = (cstr(name), cstr(sig));
    log::trace!("GetMethodID({}.{name}{sig})", c.name);
    c.method(&name, &sig, false)
}

unsafe extern "C" fn get_static_method_id(
    _e: Env,
    clazz: JObject,
    name: *const c_char,
    sig: *const c_char,
) -> *const Method {
    let c = unsafe { class_of(clazz) };
    let (name, sig) = (cstr(name), cstr(sig));
    log::trace!("GetStaticMethodID({}.{name}{sig})", c.name);
    c.method(&name, &sig, true)
}

unsafe extern "C" fn get_field_id(
    _e: Env,
    clazz: JObject,
    name: *const c_char,
    sig: *const c_char,
) -> *const Field {
    let c = unsafe { class_of(clazz) };
    let (name, sig) = (cstr(name), cstr(sig));
    log::trace!("GetFieldID({}.{name} {sig})", c.name);
    c.field(&name, &sig, false)
}

unsafe extern "C" fn get_static_field_id(
    _e: Env,
    clazz: JObject,
    name: *const c_char,
    sig: *const c_char,
) -> *const Field {
    let c = unsafe { class_of(clazz) };
    let (name, sig) = (cstr(name), cstr(sig));
    log::trace!("GetStaticFieldID({}.{name} {sig})", c.name);
    c.field(&name, &sig, true)
}

unsafe fn field_get(o: JObject, f: *const Field) -> JValue {
    let f = unsafe { &*f };
    let v = if f.is_static {
        f.class.statics.lock().unwrap().get(&f.name).cloned()
    } else {
        match unsafe { obj(o) } {
            Some(Object::Instance(i)) => {
                i.fields.lock().unwrap().get(&f.name).cloned()
            }
            _ => None,
        }
    };
    v.unwrap_or_else(|| {
        log::debug!("read of unset field {}.{} {}", f.class.name, f.name, f.sig);
        JValue::zero(f.sig.as_bytes()[0])
    })
}

unsafe fn field_set(o: JObject, f: *const Field, v: JValue) {
    let f = unsafe { &*f };
    let key = f.name.clone();
    if f.is_static {
        f.class.statics.lock().unwrap().insert(key, v);
    } else if let Some(Object::Instance(i)) = unsafe { obj(o) } {
        i.fields.lock().unwrap().insert(key, v);
    }
}

unsafe extern "C" fn get_object_field(_e: Env, o: JObject, f: *const Field) -> JObject {
    JObject::from_jvalue(unsafe { field_get(o, f) })
}
unsafe extern "C" fn set_object_field(_e: Env, o: JObject, f: *const Field, v: JObject) {
    unsafe { field_set(o, f, JValue::Object(from_raw(v))) }
}
unsafe extern "C" fn get_field<T: Prim>(_e: Env, o: JObject, f: *const Field) -> T {
    T::from_j(&unsafe { field_get(o, f) })
}
unsafe extern "C" fn set_field<T: Prim>(_e: Env, o: JObject, f: *const Field, v: T) {
    unsafe { field_set(o, f, v.to_j()) }
}

// --- references and exceptions ------------------------------------------------------------

unsafe extern "C" fn new_global_ref(_e: Env, o: JObject) -> JObject {
    if !o.is_null() {
        unsafe { Arc::increment_strong_count(o as *const Object) };
    }
    o
}
unsafe extern "C" fn delete_global_ref(_e: Env, o: JObject) {
    if !o.is_null() {
        unsafe { Arc::decrement_strong_count(o as *const Object) };
    }
}
unsafe extern "C" fn delete_local_ref(_e: Env, o: JObject) {
    delete_local(o);
}
unsafe extern "C" fn new_local_ref(_e: Env, o: JObject) -> JObject {
    to_local(unsafe { from_raw(o) })
}
unsafe extern "C" fn is_same_object(_e: Env, a: JObject, b: JObject) -> u8 {
    (a == b) as u8
}
unsafe extern "C" fn ensure_local_capacity(_e: Env, _n: i32) -> i32 {
    0
}
unsafe extern "C" fn push_local_frame(_e: Env, _n: i32) -> i32 {
    push_frame();
    0
}
unsafe extern "C" fn pop_local_frame(_e: Env, result: JObject) -> JObject {
    let keep = unsafe { from_raw(result) };
    pop_frame();
    to_local(keep)
}
unsafe extern "C" fn get_object_ref_type(_e: Env, o: JObject) -> i32 {
    if o.is_null() { 0 } else { 1 }
}

unsafe extern "C" fn throw(_e: Env, o: JObject) -> i32 {
    let r = unsafe { from_raw(o) };
    log::error!(
        "guest threw java exception {}",
        r.as_ref().map(|r| r.class_name()).unwrap_or_default()
    );
    set_pending_exception(r);
    0
}
unsafe extern "C" fn throw_new(_e: Env, clazz: JObject, msg: *const c_char) -> i32 {
    let c = unsafe { class_of(clazz) };
    log::error!("guest threw java exception {}: {}", c.name, cstr(msg));
    set_pending_exception(Some(instantiate(c)));
    0
}
unsafe extern "C" fn exception_occurred(_e: Env) -> JObject {
    to_local(pending_exception())
}
unsafe extern "C" fn exception_describe(_e: Env) {}
unsafe extern "C" fn exception_clear(_e: Env) {
    set_pending_exception(None);
}
unsafe extern "C" fn exception_check(_e: Env) -> u8 {
    pending_exception().is_some() as u8
}
unsafe extern "C" fn fatal_error(_e: Env, msg: *const c_char) -> ! {
    eprintln!("veneer: JNI FatalError: {}", cstr(msg));
    std::process::abort();
}

// --- strings ---------------------------------------------------------------------------

static UTF16_BUFFERS: Mutex<Option<HashMap<usize, Box<[u16]>>>> = Mutex::new(None);

unsafe fn string(o: JObject) -> &'static str {
    match unsafe { obj(o) } {
        Some(Object::Str(s)) => s.as_str(),
        _ => "",
    }
}

unsafe extern "C" fn new_string_utf16(_e: Env, chars: *const u16, len: JSize) -> JObject {
    let units = unsafe { std::slice::from_raw_parts(chars, len.max(0) as usize) };
    to_local(Some(new_string(&String::from_utf16_lossy(units))))
}
unsafe extern "C" fn get_string_length(_e: Env, s: JObject) -> JSize {
    unsafe { string(s) }.encode_utf16().count() as JSize
}
unsafe extern "C" fn get_string_chars(_e: Env, s: JObject, is_copy: *mut u8) -> *const u16 {
    let buf: Box<[u16]> = unsafe { string(s) }.encode_utf16().chain([0]).collect();
    let p = buf.as_ptr();
    UTF16_BUFFERS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(p as usize, buf);
    if !is_copy.is_null() {
        unsafe { *is_copy = 1 };
    }
    p
}
unsafe extern "C" fn release_string_chars(_e: Env, _s: JObject, chars: *const u16) {
    if let Some(m) = UTF16_BUFFERS.lock().unwrap().as_mut() {
        m.remove(&(chars as usize));
    }
}
unsafe extern "C" fn new_string_utf(_e: Env, bytes: *const c_char) -> JObject {
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    let b = unsafe { CStr::from_ptr(bytes) }.to_bytes();
    to_local(Some(new_string(&decode_mutf8(b))))
}
unsafe extern "C" fn get_string_utf_length(_e: Env, s: JObject) -> JSize {
    unsafe { string(s) }.len() as JSize
}
unsafe extern "C" fn get_string_utf_chars(_e: Env, s: JObject, is_copy: *mut u8) -> *const c_char {
    if s.is_null() {
        return std::ptr::null();
    }
    let bytes: Vec<u8> = unsafe { string(s) }.bytes().filter(|b| *b != 0).collect();
    if !is_copy.is_null() {
        unsafe { *is_copy = 1 };
    }
    CString::new(bytes).unwrap().into_raw()
}
unsafe extern "C" fn release_string_utf_chars(_e: Env, _s: JObject, chars: *mut c_char) {
    if !chars.is_null() {
        drop(unsafe { CString::from_raw(chars) });
    }
}
unsafe extern "C" fn get_string_region(_e: Env, s: JObject, start: JSize, len: JSize, buf: *mut u16) {
    let units: Vec<u16> = unsafe { string(s) }
        .encode_utf16()
        .skip(start as usize)
        .take(len as usize)
        .collect();
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), buf, units.len()) };
}
unsafe extern "C" fn get_string_utf_region(
    _e: Env,
    s: JObject,
    start: JSize,
    len: JSize,
    buf: *mut u8,
) {
    let units: Vec<u16> = unsafe { string(s) }
        .encode_utf16()
        .skip(start as usize)
        .take(len as usize)
        .collect();
    let out = String::from_utf16_lossy(&units);
    unsafe {
        std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
        *buf.add(out.len()) = 0;
    }
}

// --- arrays ----------------------------------------------------------------------------

unsafe extern "C" fn get_array_length(_e: Env, a: JObject) -> JSize {
    match unsafe { obj(a) } {
        Some(Object::PrimArray(p)) => p.len as JSize,
        Some(Object::ObjArray { items, .. }) => items.lock().unwrap().len() as JSize,
        _ => 0,
    }
}
unsafe extern "C" fn new_object_array(_e: Env, len: JSize, clazz: JObject, init: JObject) -> JObject {
    let init = unsafe { from_raw(init) };
    to_local(Some(Arc::new(Object::ObjArray {
        elem_class: unsafe { class_of(clazz) }.name.clone(),
        items: Mutex::new(vec![init; len.max(0) as usize]),
    })))
}
unsafe extern "C" fn get_object_array_element(_e: Env, a: JObject, i: JSize) -> JObject {
    match unsafe { obj(a) } {
        Some(Object::ObjArray { items, .. }) => {
            to_local(items.lock().unwrap().get(i as usize).cloned().flatten())
        }
        _ => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn set_object_array_element(_e: Env, a: JObject, i: JSize, v: JObject) {
    if let Some(Object::ObjArray { items, .. }) = unsafe { obj(a) } {
        let v = unsafe { from_raw(v) };
        if let Some(slot) = items.lock().unwrap().get_mut(i as usize) {
            *slot = v;
        }
    }
}

unsafe extern "C" fn new_prim_array<T: Prim>(_e: Env, len: JSize) -> JObject {
    to_local(Some(Arc::new(Object::PrimArray(PrimArray::new(T::SIG, len.max(0) as usize)))))
}
unsafe extern "C" fn get_array_elements(_e: Env, a: JObject, is_copy: *mut u8) -> *mut u8 {
    if !is_copy.is_null() {
        unsafe { *is_copy = 0 };
    }
    match unsafe { obj(a) } {
        Some(Object::PrimArray(p)) => p.as_ptr(),
        _ => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn release_array_elements(_e: Env, _a: JObject, _p: *mut u8, _mode: i32) {}
unsafe extern "C" fn get_array_region(_e: Env, a: JObject, start: JSize, len: JSize, buf: *mut u8) {
    if let Some(Object::PrimArray(p)) = unsafe { obj(a) } {
        let es = elem_size(p.elem);
        let (start, len) = (start.max(0) as usize, len.max(0) as usize);
        if start + len <= p.len {
            unsafe { std::ptr::copy_nonoverlapping(p.as_ptr().add(start * es), buf, len * es) };
        }
    }
}
unsafe extern "C" fn set_array_region(_e: Env, a: JObject, start: JSize, len: JSize, buf: *const u8) {
    if let Some(Object::PrimArray(p)) = unsafe { obj(a) } {
        let es = elem_size(p.elem);
        let (start, len) = (start.max(0) as usize, len.max(0) as usize);
        if start + len <= p.len {
            unsafe { std::ptr::copy_nonoverlapping(buf, p.as_ptr().add(start * es), len * es) };
        }
    }
}

// --- natives, monitors, buffers --------------------------------------------------------------

#[repr(C)]
struct JNINativeMethod {
    name: *const c_char,
    signature: *const c_char,
    fn_ptr: usize,
}

unsafe extern "C" fn register_natives(
    _e: Env,
    clazz: JObject,
    methods: *const JNINativeMethod,
    n: i32,
) -> i32 {
    let c = unsafe { class_of(clazz) };
    let mut natives = c.natives.lock().unwrap();
    for i in 0..n.max(0) as usize {
        let m = unsafe { &*methods.add(i) };
        let (name, sig) = (cstr(m.name), cstr(m.signature));
        log::debug!("RegisterNatives {}.{name}{sig} -> {:#x}", c.name, m.fn_ptr);
        natives.insert(format!("{name}{sig}"), m.fn_ptr);
    }
    0
}
unsafe extern "C" fn unregister_natives(_e: Env, _clazz: JObject) -> i32 {
    0
}
unsafe extern "C" fn monitor_enter(_e: Env, _o: JObject) -> i32 {
    0
}
unsafe extern "C" fn monitor_exit(_e: Env, _o: JObject) -> i32 {
    0
}
unsafe extern "C" fn get_java_vm(_e: Env, out: *mut *mut JavaVM) -> i32 {
    unsafe { *out = java_vm() };
    0
}
unsafe extern "C" fn new_direct_byte_buffer(_e: Env, addr: *mut c_void, cap: i64) -> JObject {
    to_local(Some(Arc::new(Object::DirectBuffer {
        ptr: addr as usize,
        cap: cap as usize,
    })))
}
unsafe extern "C" fn get_direct_buffer_address(_e: Env, o: JObject) -> *mut c_void {
    match unsafe { obj(o) } {
        Some(Object::DirectBuffer { ptr, .. }) => *ptr as *mut c_void,
        _ => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn get_direct_buffer_capacity(_e: Env, o: JObject) -> i64 {
    match unsafe { obj(o) } {
        Some(Object::DirectBuffer { cap, .. }) => *cap as i64,
        _ => -1,
    }
}

unsafe extern "C" fn unimplemented_slot<const N: usize>() -> usize {
    log::error!("guest called unimplemented JNIEnv function #{N}");
    0
}

// --- JavaVM (invocation interface) ---------------------------------------------------------------

unsafe extern "C" fn vm_destroy(_vm: *mut JavaVM) -> i32 {
    0
}
unsafe extern "C" fn vm_attach(_vm: *mut JavaVM, out: *mut Env, _args: *mut c_void) -> i32 {
    unsafe { *out = env() };
    0
}
unsafe extern "C" fn vm_detach(_vm: *mut JavaVM) -> i32 {
    match crate::detach_current_thread() {
        Some(0) => {}
        Some(n) => log::debug!("JNI: DetachCurrentThread released {n} local references"),
        None => log::debug!("JNI: DetachCurrentThread during a call into the guest; nothing released"),
    }
    0
}
unsafe extern "C" fn vm_get_env(_vm: *mut JavaVM, out: *mut Env, _version: i32) -> i32 {
    unsafe { *out = env() };
    0
}

fn build_env_table() -> [usize; ENV_SLOTS] {
    let mut t = [0usize; ENV_SLOTS];

    t[4] = get_version as usize;
    t[5] = unimplemented_slot::<5> as usize; // DefineClass
    t[6] = find_class as usize;
    t[7] = unimplemented_slot::<7> as usize; // FromReflectedMethod
    t[8] = unimplemented_slot::<8> as usize; // FromReflectedField
    t[9] = unimplemented_slot::<9> as usize; // ToReflectedMethod
    t[10] = get_superclass as usize;
    t[11] = is_assignable_from as usize;
    t[12] = unimplemented_slot::<12> as usize; // ToReflectedField
    t[13] = throw as usize;
    t[14] = throw_new as usize;
    t[15] = exception_occurred as usize;
    t[16] = exception_describe as usize;
    t[17] = exception_clear as usize;
    t[18] = fatal_error as usize;
    t[19] = push_local_frame as usize;
    t[20] = pop_local_frame as usize;
    t[21] = new_global_ref as usize;
    t[22] = delete_global_ref as usize;
    t[23] = delete_local_ref as usize;
    t[24] = is_same_object as usize;
    t[25] = new_local_ref as usize;
    t[26] = ensure_local_capacity as usize;
    t[27] = alloc_object as usize;
    t[28] = new_object as usize;
    t[29] = new_object_v as usize;
    t[30] = new_object_a as usize;
    t[31] = get_object_class as usize;
    t[32] = is_instance_of as usize;
    t[33] = get_method_id as usize;

    // Three call families (virtual, nonvirtual, static), ten return types
    // each, three calling styles (varargs, va_list, jvalue array).
    macro_rules! family {
        ($base:expr, $plain:ident, $v:ident, $a:ident) => {{
            macro_rules! ty {
                ($i:expr, $t:ty) => {
                    t[$base + $i * 3] = $plain::<$t> as usize;
                    t[$base + $i * 3 + 1] = $v::<$t> as usize;
                    t[$base + $i * 3 + 2] = $a::<$t> as usize;
                };
            }
            ty!(0, JObject);
            ty!(1, u8);
            ty!(2, i8);
            ty!(3, u16);
            ty!(4, i16);
            ty!(5, i32);
            ty!(6, i64);
            ty!(7, f32);
            ty!(8, f64);
            ty!(9, ());
        }};
    }
    family!(34, call, call_v, call_a);
    family!(64, call_nv, call_nv_v, call_nv_a);
    family!(114, call, call_v, call_a);

    // Field accessors: instance at 94.., static at 144.. (same layout).
    for base in [94usize, 144] {
        t[base] = if base == 94 { get_field_id as usize } else { get_static_field_id as usize };
        t[base + 1] = get_object_field as usize;
        t[base + 2] = get_field::<u8> as usize;
        t[base + 3] = get_field::<i8> as usize;
        t[base + 4] = get_field::<u16> as usize;
        t[base + 5] = get_field::<i16> as usize;
        t[base + 6] = get_field::<i32> as usize;
        t[base + 7] = get_field::<i64> as usize;
        t[base + 8] = get_field::<f32> as usize;
        t[base + 9] = get_field::<f64> as usize;
        t[base + 10] = set_object_field as usize;
        t[base + 11] = set_field::<u8> as usize;
        t[base + 12] = set_field::<i8> as usize;
        t[base + 13] = set_field::<u16> as usize;
        t[base + 14] = set_field::<i16> as usize;
        t[base + 15] = set_field::<i32> as usize;
        t[base + 16] = set_field::<i64> as usize;
        t[base + 17] = set_field::<f32> as usize;
        t[base + 18] = set_field::<f64> as usize;
    }
    t[113] = get_static_method_id as usize;

    t[163] = new_string_utf16 as usize;
    t[164] = get_string_length as usize;
    t[165] = get_string_chars as usize;
    t[166] = release_string_chars as usize;
    t[167] = new_string_utf as usize;
    t[168] = get_string_utf_length as usize;
    t[169] = get_string_utf_chars as usize;
    t[170] = release_string_utf_chars as usize;
    t[171] = get_array_length as usize;
    t[172] = new_object_array as usize;
    t[173] = get_object_array_element as usize;
    t[174] = set_object_array_element as usize;
    t[175] = new_prim_array::<u8> as usize;
    t[176] = new_prim_array::<i8> as usize;
    t[177] = new_prim_array::<u16> as usize;
    t[178] = new_prim_array::<i16> as usize;
    t[179] = new_prim_array::<i32> as usize;
    t[180] = new_prim_array::<i64> as usize;
    t[181] = new_prim_array::<f32> as usize;
    t[182] = new_prim_array::<f64> as usize;
    for i in 0..8 {
        t[183 + i] = get_array_elements as usize;
        t[191 + i] = release_array_elements as usize;
        t[199 + i] = get_array_region as usize;
        t[207 + i] = set_array_region as usize;
    }
    t[215] = register_natives as usize;
    t[216] = unregister_natives as usize;
    t[217] = monitor_enter as usize;
    t[218] = monitor_exit as usize;
    t[219] = get_java_vm as usize;
    t[220] = get_string_region as usize;
    t[221] = get_string_utf_region as usize;
    t[222] = get_array_elements as usize; // GetPrimitiveArrayCritical
    t[223] = release_array_elements as usize;
    t[224] = get_string_chars as usize; // GetStringCritical
    t[225] = release_string_chars as usize;
    t[226] = new_global_ref as usize; // NewWeakGlobalRef
    t[227] = delete_global_ref as usize;
    t[228] = exception_check as usize;
    t[229] = new_direct_byte_buffer as usize;
    t[230] = get_direct_buffer_address as usize;
    t[231] = get_direct_buffer_capacity as usize;
    t[232] = get_object_ref_type as usize;
    t
}

struct Tables {
    env_fns: usize,
    vm: usize,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let env_fns: &'static [usize; ENV_SLOTS] = Box::leak(Box::new(build_env_table()));
        let vm_fns: &'static [usize; 8] = Box::leak(Box::new([
            0,
            0,
            0,
            vm_destroy as usize,
            vm_attach as usize,
            vm_detach as usize,
            vm_get_env as usize,
            vm_attach as usize, // AttachCurrentThreadAsDaemon
        ]));
        let vm = Box::leak(Box::new(JavaVM {
            functions: vm_fns.as_ptr(),
        }));
        Tables {
            env_fns: env_fns.as_ptr() as usize,
            vm: vm as *mut JavaVM as usize,
        }
    })
}

thread_local! {
    static ENV: std::cell::Cell<*mut JNIEnv> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// The calling thread's `JNIEnv*`.
///
/// Each thread gets its own structure, as on a real VM: the engine swaps
/// the function table pointer of the env it is given for an instrumented
/// copy, and assumes that has not already happened on another thread.
pub fn env() -> *mut JNIEnv {
    ENV.with(|e| {
        if e.get().is_null() {
            e.set(Box::into_raw(Box::new(JNIEnv {
                functions: tables().env_fns as *const usize,
            })));
        }
        e.get()
    })
}

pub fn java_vm() -> *mut JavaVM {
    tables().vm as *mut JavaVM
}
