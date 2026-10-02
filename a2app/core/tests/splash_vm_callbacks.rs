use makepad_widgets::*;

#[test]
fn callback_control_flow_retains_a_value_for_each_statement() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    cx.with_vm(makepad_widgets::script_mod);
    cx.with_vm(|vm| {
        // These stock callback shapes must continue after a successful loop.
        // A loop alone supplies no if-branch result in the pinned script VM.
        for code in [
            script! {
                let pending = []
                let messages = []
                fn fill(r) {
                    if r.is_ok {
                        for m in r.data.messages { pending.push(m.event_id) }
                        nil
                    } else { messages = [] }
                    for id in pending { messages.push(id) }
                    return messages.len()
                }
                fill({is_ok:true data:{messages:[{event_id:"test"}]}})
            },
            script! {
                let pins = []
                fn fill(r) {
                    if r.is_ok {
                        for p in r.data.pinned { pins.push(p.event_id) }
                        nil
                    } else { pins = [] }
                    return pins.len()
                }
                fill({is_ok:true data:{pinned:[{event_id:"test"}]}})
            },
        ] {
            vm.bx.captured_errors = Some(Vec::new());
            let value = vm.eval(code);
            let errors = vm.take_errors();
            assert!(errors.is_empty(), "callback failed: {errors:?}; value: {value:?}");
            assert_eq!(value.as_f64(), Some(1.0), "callback must finish after collecting its event IDs");
        }
    });
}
