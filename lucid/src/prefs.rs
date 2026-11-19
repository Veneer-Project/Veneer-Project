//! Android `SharedPreferences`, which the engine uses (through JNI) to
//! persist small settings. Each preferences file is a JSON object under
//! `<data>/prefs/<name>.json`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use lucid_jni::{JValue, Ref, new_instance, new_string, vm};
use serde_json::{Map, Value};

use crate::paths;

const PREFS: &str = "android/content/SharedPreferences";
const EDITOR: &str = "android/content/SharedPreferences$Editor";

static STORE: Mutex<Option<HashMap<String, Map<String, Value>>>> = Mutex::new(None);

fn file(name: &str) -> PathBuf {
    // Preference names are plain identifiers; never let one escape the dir.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    paths::data_dir().join("prefs").join(format!("{safe}.json"))
}

fn with_file<R>(name: &str, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
    let mut store = STORE.lock().unwrap();
    let map = store
        .get_or_insert_with(HashMap::new)
        .entry(name.to_string())
        .or_insert_with(|| {
            fs::read_to_string(file(name))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        });
    f(map)
}

fn save(name: &str) {
    let text = with_file(name, |m| serde_json::to_string_pretty(m).unwrap_or_default());
    let path = file(name);
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = fs::write(&path, text) {
        log::warn!("could not save preferences {name}: {e}");
    }
}

/// The preferences file name an object (prefs or editor) belongs to.
fn name_of(this: Option<&Ref>) -> String {
    this.and_then(|t| t.as_instance())
        .and_then(|i| i.get("name"))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "prefs".into())
}

fn key(a: &[JValue]) -> String {
    a.first().and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn named(class: &str, name: &str) -> Ref {
    let o = new_instance(class);
    o.as_instance().unwrap().set("name", name);
    o
}

pub fn install() {
    let vm = vm();
    // How native code reaches the application context.
    vm.on(
        "android/app/ActivityThread",
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
        |_, _| new_instance("android/app/ActivityThread"),
    );
    vm.on(
        "android/app/ActivityThread",
        "getApplication",
        "()Landroid/app/Application;",
        |_, _| new_instance("android/app/Application"),
    );
    for ctx in ["android/content/Context", "android/app/Application"] {
        vm.on(
            ctx,
            "getSharedPreferences",
            "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
            |_, a| named(PREFS, &key(a)),
        );
    }

    vm.on(PREFS, "edit", "()Landroid/content/SharedPreferences$Editor;", |this, _| {
        named(EDITOR, &name_of(this))
    });
    vm.on(PREFS, "contains", "(Ljava/lang/String;)Z", |this, a| {
        with_file(&name_of(this), |m| m.contains_key(&key(a)))
    });
    vm.on(
        PREFS,
        "getString",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |this, a| match with_file(&name_of(this), |m| m.get(&key(a)).cloned()) {
            Some(Value::String(s)) => JValue::Object(Some(new_string(&s))),
            _ => a.get(1).cloned().unwrap_or(JValue::null()),
        },
    );
    vm.on(PREFS, "getBoolean", "(Ljava/lang/String;Z)Z", |this, a| {
        with_file(&name_of(this), |m| m.get(&key(a)).and_then(Value::as_bool))
            .unwrap_or(a[1].as_i64() != 0)
    });
    vm.on(PREFS, "getInt", "(Ljava/lang/String;I)I", |this, a| {
        with_file(&name_of(this), |m| m.get(&key(a)).and_then(Value::as_i64))
            .unwrap_or(a[1].as_i64()) as i32
    });
    vm.on(PREFS, "getLong", "(Ljava/lang/String;J)J", |this, a| {
        with_file(&name_of(this), |m| m.get(&key(a)).and_then(Value::as_i64))
            .unwrap_or(a[1].as_i64())
    });
    vm.on(PREFS, "getFloat", "(Ljava/lang/String;F)F", |this, a| {
        with_file(&name_of(this), |m| m.get(&key(a)).and_then(Value::as_f64))
            .unwrap_or(a[1].as_f64()) as f32
    });

    // Editor: writes go straight to the in-memory map; apply/commit persist.
    let ret = "Landroid/content/SharedPreferences$Editor;";
    vm.on(
        EDITOR,
        "putString",
        &format!("(Ljava/lang/String;Ljava/lang/String;){ret}"),
        |this, a| {
            let v = match a[1].as_str() {
                Some(s) => Value::String(s.to_string()),
                None => Value::Null,
            };
            with_file(&name_of(this), |m| m.insert(key(a), v));
            this.cloned()
        },
    );
    vm.on(EDITOR, "putBoolean", &format!("(Ljava/lang/String;Z){ret}"), |this, a| {
        with_file(&name_of(this), |m| m.insert(key(a), Value::Bool(a[1].as_i64() != 0)));
        this.cloned()
    });
    for sig in ["I", "J"] {
        let name = if sig == "I" { "putInt" } else { "putLong" };
        vm.on(EDITOR, name, &format!("(Ljava/lang/String;{sig}){ret}"), |this, a| {
            with_file(&name_of(this), |m| m.insert(key(a), Value::from(a[1].as_i64())));
            this.cloned()
        });
    }
    vm.on(EDITOR, "putFloat", &format!("(Ljava/lang/String;F){ret}"), |this, a| {
        with_file(&name_of(this), |m| m.insert(key(a), Value::from(a[1].as_f64())));
        this.cloned()
    });
    vm.on(EDITOR, "remove", &format!("(Ljava/lang/String;){ret}"), |this, a| {
        with_file(&name_of(this), |m| m.remove(&key(a)));
        this.cloned()
    });
    vm.on(EDITOR, "clear", &format!("(){ret}"), |this, _| {
        with_file(&name_of(this), |m| m.clear());
        this.cloned()
    });
    vm.on(EDITOR, "apply", "()V", |this, _| save(&name_of(this)));
    vm.on(EDITOR, "commit", "()Z", |this, _| {
        save(&name_of(this));
        true
    });
}
