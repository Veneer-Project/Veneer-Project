//! `LUCID_JNI_TRACE`: live references are labelled, released ones are not
//! reported. Its own test binary, since the switch is read once per process.

#[test]
fn live_references_are_reported_until_released() {
    unsafe { std::env::set_var("LUCID_JNI_TRACE", "1") };
    let (to_thread, from_main) = std::sync::mpsc::channel::<()>();
    let (to_main, from_thread) = std::sync::mpsc::channel::<()>();
    let engine_thread = std::thread::Builder::new()
        .name("engine-like".into())
        .spawn(move || {
            // No frame pushed, like an engine thread.
            lucid_jni::to_local(Some(lucid_jni::new_string("leaked")));
            to_main.send(()).unwrap();
            from_main.recv().unwrap();
        })
        .unwrap();
    from_thread.recv().unwrap();
    let report = lucid_jni::trace_report(10).unwrap();
    assert!(report.contains("engine-like (tid "), "{report}");
    assert!(report.contains("trace.rs:"), "{report}");
    assert!(report.contains("java/lang/String"), "{report}");
    assert!(report.contains("bottom frame"), "{report}");
    // The thread exits: its references are released and no longer reported.
    to_thread.send(()).unwrap();
    engine_thread.join().unwrap();
    assert_eq!(lucid_jni::trace_report(10).unwrap(), "");
}
