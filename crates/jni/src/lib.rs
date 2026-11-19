//! A fake Java VM.
//!
//! There is no Java here: the guest library gets a `JavaVM`/`JNIEnv` whose
//! function table is implemented in Rust, backed by a small object model
//! (classes, strings, arrays, instances). The "Java side" of the app is
//! reimplemented by registering Rust handlers for the methods the native
//! code calls; anything unregistered logs once and returns a zero value, so
//! missing pieces are discovered by running.

mod table;

use std::any::Any;
use std::cell::{RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

pub use table::{JNIEnv, JavaVM, env, java_vm};

/// A raw JNI reference as seen by the guest (`jobject`): either null or a
/// pointer obtained from `Arc::into_raw`.
pub type JObject = *mut c_void;

pub type Ref = Arc<Object>;

#[derive(Clone, Default)]
pub enum JValue {
    #[default]
    Void,
    Bool(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object(Option<Ref>),
}

impl std::fmt::Debug for JValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JValue::Void => write!(f, "void"),
            JValue::Bool(v) => write!(f, "{v}"),
            JValue::Byte(v) => write!(f, "{v}b"),
            JValue::Char(v) => write!(f, "'\\u{v:04x}'"),
            JValue::Short(v) => write!(f, "{v}s"),
            JValue::Int(v) => write!(f, "{v}"),
            JValue::Long(v) => write!(f, "{v}L"),
            JValue::Float(v) => write!(f, "{v}f"),
            JValue::Double(v) => write!(f, "{v}d"),
            JValue::Object(None) => write!(f, "null"),
            JValue::Object(Some(o)) => match &**o {
                Object::Str(s) => write!(f, "{s:?}"),
                other => write!(f, "<{}>", other.class_name()),
            },
        }
    }
}

impl JValue {
    pub fn null() -> JValue {
        JValue::Object(None)
    }

    /// The zero value for a JNI type descriptor character.
    pub fn zero(ty: u8) -> JValue {
        match ty {
            b'V' => JValue::Void,
            b'Z' => JValue::Bool(false),
            b'B' => JValue::Byte(0),
            b'C' => JValue::Char(0),
            b'S' => JValue::Short(0),
            b'I' => JValue::Int(0),
            b'J' => JValue::Long(0),
            b'F' => JValue::Float(0.0),
            b'D' => JValue::Double(0.0),
            _ => JValue::Object(None),
        }
    }

    pub fn as_i64(&self) -> i64 {
        match self {
            JValue::Bool(b) => *b as i64,
            JValue::Byte(v) => *v as i64,
            JValue::Char(v) => *v as i64,
            JValue::Short(v) => *v as i64,
            JValue::Int(v) => *v as i64,
            JValue::Long(v) => *v,
            JValue::Float(v) => *v as i64,
            JValue::Double(v) => *v as i64,
            _ => 0,
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            JValue::Float(v) => *v as f64,
            JValue::Double(v) => *v,
            other => other.as_i64() as f64,
        }
    }

    pub fn as_object(&self) -> Option<&Ref> {
        match self {
            JValue::Object(o) => o.as_ref(),
            _ => None,
        }
    }

    /// The Rust string inside a `java.lang.String` argument.
    pub fn as_str(&self) -> Option<&str> {
        self.as_object().and_then(|o| o.as_str())
    }
}

impl From<bool> for JValue {
    fn from(v: bool) -> Self {
        JValue::Bool(v)
    }
}
impl From<i32> for JValue {
    fn from(v: i32) -> Self {
        JValue::Int(v)
    }
}
impl From<i64> for JValue {
    fn from(v: i64) -> Self {
        JValue::Long(v)
    }
}
impl From<f32> for JValue {
    fn from(v: f32) -> Self {
        JValue::Float(v)
    }
}
impl From<f64> for JValue {
    fn from(v: f64) -> Self {
        JValue::Double(v)
    }
}
impl From<Ref> for JValue {
    fn from(v: Ref) -> Self {
        JValue::Object(Some(v))
    }
}
impl From<Option<Ref>> for JValue {
    fn from(v: Option<Ref>) -> Self {
        JValue::Object(v)
    }
}
impl From<()> for JValue {
    fn from(_: ()) -> Self {
        JValue::Void
    }
}
impl From<&str> for JValue {
    fn from(v: &str) -> Self {
        JValue::Object(Some(new_string(v)))
    }
}
impl From<String> for JValue {
    fn from(v: String) -> Self {
        JValue::Object(Some(new_string(&v)))
    }
}

/// Storage for a primitive array. Elements live in 8-byte aligned memory
/// that never moves, so the guest can hold direct pointers into it.
pub struct PrimArray {
    pub elem: u8,
    pub len: usize,
    data: UnsafeCell<Box<[u64]>>,
}

unsafe impl Sync for PrimArray {}
unsafe impl Send for PrimArray {}

pub fn elem_size(ty: u8) -> usize {
    match ty {
        b'Z' | b'B' => 1,
        b'C' | b'S' => 2,
        b'I' | b'F' => 4,
        _ => 8,
    }
}

impl PrimArray {
    pub fn new(elem: u8, len: usize) -> PrimArray {
        let words = (len * elem_size(elem)).div_ceil(8).max(1);
        PrimArray {
            elem,
            len,
            data: UnsafeCell::new(vec![0u64; words].into_boxed_slice()),
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        unsafe { (*self.data.get()).as_mut_ptr() as *mut u8 }
    }

    pub fn byte_len(&self) -> usize {
        self.len * elem_size(self.elem)
    }

    /// View the contents as bytes (no synchronisation, like Java arrays).
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.byte_len()) }
    }
}

pub struct Instance {
    pub class: &'static Class,
    pub fields: Mutex<HashMap<String, JValue>>,
    /// Host-side state attached to this object by its Rust implementation.
    pub data: Mutex<Option<Box<dyn Any + Send + Sync>>>,
}

pub enum Object {
    Class(&'static Class),
    Str(String),
    PrimArray(PrimArray),
    ObjArray {
        elem_class: String,
        items: Mutex<Vec<Option<Ref>>>,
    },
    Instance(Instance),
    DirectBuffer {
        ptr: usize,
        cap: usize,
    },
}

impl Instance {
    /// Set a field by name (the guest reads it with `Get<Type>Field`, or
    /// through a same-named no-argument getter method).
    pub fn set(&self, name: &str, v: impl Into<JValue>) -> &Self {
        self.fields.lock().unwrap().insert(name.to_string(), v.into());
        self
    }

    pub fn get(&self, name: &str) -> Option<JValue> {
        self.fields.lock().unwrap().get(name).cloned()
    }
}

impl Object {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Object::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_instance(&self) -> Option<&Instance> {
        match self {
            Object::Instance(i) => Some(i),
            _ => None,
        }
    }

    pub fn as_prim_array(&self) -> Option<&PrimArray> {
        match self {
            Object::PrimArray(a) => Some(a),
            _ => None,
        }
    }

    pub fn class_name(&self) -> String {
        match self {
            Object::Class(_) => "java/lang/Class".into(),
            Object::Str(_) => "java/lang/String".into(),
            Object::PrimArray(a) => format!("[{}", a.elem as char),
            Object::ObjArray { elem_class, .. } => format!("[L{elem_class};"),
            Object::Instance(i) => i.class.name.clone(),
            Object::DirectBuffer { .. } => "java/nio/DirectByteBuffer".into(),
        }
    }

    pub fn class(&self) -> &'static Class {
        match self {
            Object::Instance(i) => i.class,
            other => vm().class(&other.class_name()),
        }
    }
}

pub type Handler = Arc<dyn Fn(Option<&Ref>, &[JValue]) -> JValue + Send + Sync>;

pub struct Method {
    pub class: &'static Class,
    pub name: String,
    pub sig: String,
    pub is_static: bool,
    /// One descriptor character per parameter (`L` for any reference).
    pub params: Vec<u8>,
    pub ret: u8,
    handler: OnceLock<Handler>,
    warned: AtomicBool,
}

pub struct Field {
    pub class: &'static Class,
    pub name: String,
    pub sig: String,
    pub is_static: bool,
}

pub struct Class {
    pub name: String,
    methods: Mutex<HashMap<(String, String, bool), &'static Method>>,
    fields: Mutex<HashMap<(String, String, bool), &'static Field>>,
    pub statics: Mutex<HashMap<String, JValue>>,
    /// Native methods registered by the guest: `name + sig` -> address.
    pub natives: Mutex<HashMap<String, usize>>,
    pub supers: Mutex<Vec<String>>,
    object: OnceLock<Ref>,
}

impl Class {
    /// The `java.lang.Class` object for this class.
    pub fn object(&'static self) -> Ref {
        self.object.get_or_init(|| Arc::new(Object::Class(self))).clone()
    }

    pub fn method(&'static self, name: &str, sig: &str, is_static: bool) -> &'static Method {
        let mut methods = self.methods.lock().unwrap();
        methods
            .entry((name.to_string(), sig.to_string(), is_static))
            .or_insert_with(|| {
                let (params, ret) = parse_sig(sig);
                Box::leak(Box::new(Method {
                    class: self,
                    name: name.to_string(),
                    sig: sig.to_string(),
                    is_static,
                    params,
                    ret,
                    handler: OnceLock::new(),
                    warned: AtomicBool::new(false),
                }))
            })
    }

    pub fn field(&'static self, name: &str, sig: &str, is_static: bool) -> &'static Field {
        let mut fields = self.fields.lock().unwrap();
        fields
            .entry((name.to_string(), sig.to_string(), is_static))
            .or_insert_with(|| {
                Box::leak(Box::new(Field {
                    class: self,
                    name: name.to_string(),
                    sig: sig.to_string(),
                    is_static,
                }))
            })
    }

    pub fn is_subclass_of(&self, other: &str) -> bool {
        if self.name == other || other == "java/lang/Object" {
            return true;
        }
        let supers = self.supers.lock().unwrap().clone();
        supers.iter().any(|s| s == other || vm().class(s).is_subclass_of(other))
    }
}

/// Split a method descriptor into parameter kinds and return kind.
pub fn parse_sig(sig: &str) -> (Vec<u8>, u8) {
    let b = sig.as_bytes();
    let mut params = Vec::new();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        let mut c = b[i];
        if c == b'[' {
            while i < b.len() && b[i] == b'[' {
                i += 1;
            }
            if b[i] == b'L' {
                while b[i] != b';' {
                    i += 1;
                }
            }
            c = b'L';
        } else if c == b'L' {
            while b[i] != b';' {
                i += 1;
            }
        }
        params.push(c);
        i += 1;
    }
    let ret = match b.get(i + 1).copied().unwrap_or(b'V') {
        b'[' => b'L',
        c => c,
    };
    (params, ret)
}

pub struct Vm {
    classes: Mutex<HashMap<String, &'static Class>>,
    handlers: RwLock<HashMap<String, Handler>>,
    symbol_resolver: RwLock<Option<Box<dyn Fn(&str) -> Option<usize> + Send + Sync>>>,
}

static VM: OnceLock<Vm> = OnceLock::new();

pub fn vm() -> &'static Vm {
    VM.get_or_init(|| Vm {
        classes: Mutex::new(HashMap::new()),
        handlers: RwLock::new(HashMap::new()),
        symbol_resolver: RwLock::new(None),
    })
}

fn handler_key(class: &str, name: &str, sig: &str) -> String {
    format!("{class}#{name}#{sig}")
}

impl Vm {
    /// Find a class, creating an empty placeholder if it is unknown.
    pub fn class(&self, name: &str) -> &'static Class {
        let mut classes = self.classes.lock().unwrap();
        if let Some(c) = classes.get(name) {
            return c;
        }
        let c: &'static Class = Box::leak(Box::new(Class {
            name: name.to_string(),
            methods: Mutex::new(HashMap::new()),
            fields: Mutex::new(HashMap::new()),
            statics: Mutex::new(HashMap::new()),
            natives: Mutex::new(HashMap::new()),
            supers: Mutex::new(Vec::new()),
            object: OnceLock::new(),
        }));
        classes.insert(name.to_string(), c);
        c
    }

    pub fn has_class(&self, name: &str) -> bool {
        self.classes.lock().unwrap().contains_key(name)
    }

    /// Implement a Java method in Rust. Use class `*` to match any class.
    pub fn on<F, R>(&self, class: &str, name: &str, sig: &str, f: F)
    where
        F: Fn(Option<&Ref>, &[JValue]) -> R + Send + Sync + 'static,
        R: Into<JValue>,
    {
        let h: Handler = Arc::new(move |this, args| f(this, args).into());
        self.handlers
            .write()
            .unwrap()
            .insert(handler_key(class, name, sig), h);
    }

    pub fn set_super(&self, class: &str, parent: &str) {
        self.class(class).supers.lock().unwrap().push(parent.to_string());
    }

    fn find_handler(&self, m: &Method) -> Option<Handler> {
        let handlers = self.handlers.read().unwrap();
        if let Some(h) = handlers.get(&handler_key(&m.class.name, &m.name, &m.sig)) {
            return Some(h.clone());
        }
        for sup in m.class.supers.lock().unwrap().iter() {
            if let Some(h) = handlers.get(&handler_key(sup, &m.name, &m.sig)) {
                return Some(h.clone());
            }
        }
        handlers.get(&handler_key("*", &m.name, &m.sig)).cloned()
    }

    /// Call a (fake) Java method.
    pub fn invoke(&self, this: Option<&Ref>, m: &Method, args: &[JValue]) -> JValue {
        let handler = match m.handler.get() {
            Some(h) => Some(h.clone()),
            None => self.find_handler(m).inspect(|h| {
                let _ = m.handler.set(h.clone());
            }),
        };
        match handler {
            Some(h) => {
                log::trace!("java call {}.{}{}", m.class.name, m.name, m.sig);
                h(this, args)
            }
            None => {
                // Value classes: a no-argument method named like a field
                // is treated as its getter.
                if m.params.is_empty() {
                    if let Some(Object::Instance(i)) = this.map(|t| &**t) {
                        if let Some(v) = i.get(&m.name) {
                            return v;
                        }
                    }
                }
                if !m.warned.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "UNIMPLEMENTED java method {}.{}{}{}",
                        m.class.name,
                        m.name,
                        m.sig,
                        if m.is_static { " [static]" } else { "" }
                    );
                }
                JValue::zero(m.ret)
            }
        }
    }

    pub fn set_symbol_resolver<F>(&self, f: F)
    where
        F: Fn(&str) -> Option<usize> + Send + Sync + 'static,
    {
        *self.symbol_resolver.write().unwrap() = Some(Box::new(f));
    }

    /// Address of a guest `native` method implementation, either registered
    /// through `RegisterNatives` or exported as `Java_<class>_<method>`.
    pub fn native(&self, class: &str, name: &str, sig: &str) -> Option<usize> {
        let c = self.class(class);
        let natives = c.natives.lock().unwrap();
        if let Some(a) = natives.get(&format!("{name}{sig}")) {
            return Some(*a);
        }
        if let Some((_, a)) = natives.iter().find(|(k, _)| {
            k.strip_prefix(name).is_some_and(|rest| rest.starts_with('('))
        }) {
            return Some(*a);
        }
        drop(natives);
        let sym = format!("Java_{}_{}", mangle(class), mangle(name));
        let resolver = self.symbol_resolver.read().unwrap();
        let resolver = resolver.as_ref()?;
        // Overloaded natives use the long form with the argument signature.
        let params = sig.strip_prefix('(').and_then(|s| s.split(')').next()).unwrap_or("");
        resolver(&sym).or_else(|| resolver(&format!("{sym}__{}", mangle(params))))
    }
}

/// JNI short-name mangling.
pub fn mangle(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '/' | '.' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => out.push_str(&format!("_0{:04x}", c as u32)),
        }
    }
    out
}

// --- object construction helpers --------------------------------------------

pub fn new_string(s: &str) -> Ref {
    Arc::new(Object::Str(s.to_string()))
}

pub fn new_instance(class: &str) -> Ref {
    Arc::new(Object::Instance(Instance {
        class: vm().class(class),
        fields: Mutex::new(HashMap::new()),
        data: Mutex::new(None),
    }))
}

pub fn new_instance_with<T: Any + Send + Sync>(class: &str, data: T) -> Ref {
    Arc::new(Object::Instance(Instance {
        class: vm().class(class),
        fields: Mutex::new(HashMap::new()),
        data: Mutex::new(Some(Box::new(data))),
    }))
}

pub fn new_byte_array(bytes: &[u8]) -> Ref {
    let a = PrimArray::new(b'B', bytes.len());
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), a.as_ptr(), bytes.len()) };
    Arc::new(Object::PrimArray(a))
}

pub fn new_object_array(elem_class: &str, items: Vec<Option<Ref>>) -> Ref {
    Arc::new(Object::ObjArray {
        elem_class: elem_class.to_string(),
        items: Mutex::new(items),
    })
}

pub fn new_string_array(items: &[&str]) -> Ref {
    new_object_array(
        "java/lang/String",
        items.iter().map(|s| Some(new_string(s))).collect(),
    )
}

/// A `java.util.List` holding the given objects.
pub fn new_list(items: Vec<Option<Ref>>) -> Ref {
    new_instance_with("java/util/ArrayList", items)
}

fn list_items(this: Option<&Ref>) -> Vec<Option<Ref>> {
    this.and_then(|t| t.as_instance())
        .and_then(|i| {
            i.data
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|d| d.downcast_ref::<Vec<Option<Ref>>>().cloned())
        })
        .unwrap_or_default()
}

/// Register the handful of `java.*` methods native code commonly uses.
pub fn install_stdlib() {
    let vm = vm();
    for class in ["java/util/List", "java/util/ArrayList", "java/util/Collection"] {
        vm.on(class, "size", "()I", |this, _| list_items(this).len() as i32);
        vm.on(class, "isEmpty", "()Z", |this, _| list_items(this).is_empty());
        vm.on(class, "get", "(I)Ljava/lang/Object;", |this, args| {
            let i = args.first().map(|a| a.as_i64()).unwrap_or(0) as usize;
            list_items(this).get(i).cloned().flatten()
        });
    }
    // Native code converts Java strings with `getBytes("UTF-8")`.
    for sig in ["()[B", "(Ljava/lang/String;)[B", "(Ljava/nio/charset/Charset;)[B"] {
        vm.on("java/lang/String", "getBytes", sig, |this, _| {
            new_byte_array(this.and_then(|t| t.as_str()).unwrap_or("").as_bytes())
        });
    }
    vm.on("java/lang/String", "length", "()I", |this, _| {
        this.and_then(|t| t.as_str()).unwrap_or("").encode_utf16().count() as i32
    });
    vm.on("*", "toString", "()Ljava/lang/String;", |this, _| {
        match this.map(|t| &**t) {
            Some(Object::Str(s)) => new_string(s),
            Some(o) => new_string(&o.class_name()),
            None => new_string("null"),
        }
    });
    vm.on("*", "hashCode", "()I", |this, _| {
        this.map(|t| Arc::as_ptr(t) as usize as i32).unwrap_or(0)
    });
    vm.on("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;", |_, _| {
        new_instance("java/lang/ClassLoader")
    });
    vm.on("java/lang/Class", "getName", "()Ljava/lang/String;", |this, _| {
        match this.map(|t| &**t) {
            Some(Object::Class(c)) => new_string(&c.name.replace('/', ".")),
            _ => new_string("java.lang.Object"),
        }
    });
    vm.on(
        "java/lang/ClassLoader",
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |_, args| {
            let name = args.first().and_then(|a| a.as_str()).unwrap_or("").replace('.', "/");
            vm_class_object(&name)
        },
    );
}

fn vm_class_object(name: &str) -> Ref {
    vm().class(name).object()
}

// --- reference management -----------------------------------------------------

thread_local! {
    static FRAMES: RefCell<Vec<Vec<JObject>>> = const { RefCell::new(Vec::new()) };
    static PENDING: RefCell<Option<Ref>> = const { RefCell::new(None) };
}

/// Turn an owned reference into a guest-visible local reference, owned by
/// the current thread's innermost local frame.
pub fn to_local(r: Option<Ref>) -> JObject {
    match r {
        None => std::ptr::null_mut(),
        Some(r) => {
            let p = Arc::into_raw(r) as JObject;
            FRAMES.with(|f| {
                let mut f = f.borrow_mut();
                if f.is_empty() {
                    f.push(Vec::new());
                }
                f.last_mut().unwrap().push(p);
            });
            p
        }
    }
}

/// Get an owned reference from a guest `jobject`.
///
/// # Safety
/// `o` must be null or a live reference handed out by this VM.
pub unsafe fn from_raw(o: JObject) -> Option<Ref> {
    if o.is_null() {
        return None;
    }
    unsafe {
        Arc::increment_strong_count(o as *const Object);
        Some(Arc::from_raw(o as *const Object))
    }
}

pub(crate) fn delete_local(o: JObject) {
    if o.is_null() {
        return;
    }
    let found = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        for frame in f.iter_mut().rev() {
            if let Some(i) = frame.iter().rposition(|p| *p == o) {
                frame.swap_remove(i);
                return true;
            }
        }
        false
    });
    if found {
        unsafe { Arc::decrement_strong_count(o as *const Object) };
    }
}

pub fn push_frame() {
    FRAMES.with(|f| f.borrow_mut().push(Vec::new()));
}

pub fn pop_frame() {
    let frame = FRAMES.with(|f| f.borrow_mut().pop());
    for p in frame.unwrap_or_default() {
        unsafe { Arc::decrement_strong_count(p as *const Object) };
    }
}

/// Run `f` (typically a call into a guest native method) inside its own
/// local reference frame, releasing every local created during the call.
pub fn with_frame<R>(f: impl FnOnce() -> R) -> R {
    push_frame();
    let r = f();
    pop_frame();
    r
}

pub fn pending_exception() -> Option<Ref> {
    PENDING.with(|p| p.borrow().clone())
}

pub fn set_pending_exception(e: Option<Ref>) {
    PENDING.with(|p| *p.borrow_mut() = e);
}
