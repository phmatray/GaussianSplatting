//! Entry point of the `splat-worker` Web Worker: glue between
//! [`gs_worker::Driver`] and the worker's `postMessage` and `setTimeout`.
//! See the library documentation for the protocol.

#[cfg(target_arch = "wasm32")]
mod glue {
    use std::cell::{Cell, RefCell};

    use gs_worker::{Batch, Driver, WorkerEvent};
    use js_sys::{Array, Uint8Array};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

    struct Worker {
        scope: DedicatedWorkerGlobalScope,
        driver: RefCell<Driver>,
        /// Handle of the pending `setTimeout`, if any.
        timer: Cell<Option<i32>>,
        on_timer: Closure<dyn FnMut()>,
    }

    thread_local! {
        static WORKER: RefCell<Option<&'static Worker>> = const { RefCell::new(None) };
    }

    fn worker() -> &'static Worker {
        WORKER.with(|w| w.borrow().expect("the worker is initialised in main"))
    }

    impl Worker {
        fn now(&self) -> f64 {
            self.scope.performance().map_or_else(js_sys::Date::now, |p| p.now())
        }

        fn post_event(&self, event: &WorkerEvent) {
            let _ = self.scope.post_message(&JsValue::from_str(&event.to_json()));
        }

        /// Posts `[id, buffer, ...]`, transferring the buffers.
        fn post_batch(&self, batch: Batch) {
            let message = Array::new();
            let transfer = Array::new();
            message.push(&JsValue::from(batch.id));
            for bytes in &batch.messages {
                // A copy into a buffer owned by JavaScript: a view of the
                // WebAssembly memory cannot be transferred.
                let buffer = Uint8Array::from(bytes.as_slice()).buffer();
                message.push(&buffer);
                transfer.push(&buffer);
            }
            if self.scope.post_message_with_transfer(&message, &transfer).is_err() {
                self.post_event(&WorkerEvent::Error { message: "postMessage failed".into() });
            }
        }

        fn on_message(&self, event: &MessageEvent) {
            let Some(text) = event.data().as_string() else { return };
            let result = self.driver.borrow_mut().handle_text(&text, self.now());
            match result {
                Ok(Some(batch)) => self.post_batch(batch),
                Ok(None) => {}
                Err(message) => self.post_event(&WorkerEvent::Error { message }),
            }
            // An acknowledgement may have unblocked a tick that is due.
            self.pump();
        }

        /// Runs a tick if one is due, then sleeps until the next one.
        fn pump(&self) {
            let poll = self.driver.borrow_mut().poll(self.now());
            let ticked = poll.batch.is_some();
            if let Some(batch) = poll.batch {
                self.post_batch(batch);
            }
            // After a tick, measure again: the tick itself took time.
            let wait = if ticked { self.driver.borrow().wait_ms(self.now()) } else { poll.wait_ms };
            self.schedule(wait);
        }

        fn schedule(&self, wait_ms: Option<f64>) {
            if let Some(handle) = self.timer.take() {
                self.scope.clear_timeout_with_handle(handle);
            }
            let Some(wait_ms) = wait_ms else { return };
            // Always through the event loop, even when overdue: pending
            // acknowledgements and control messages must get their turn.
            let handle = self
                .scope
                .set_timeout_with_callback_and_timeout_and_arguments_0(self.on_timer.as_ref().unchecked_ref(), wait_ms.ceil() as i32);
            self.timer.set(handle.ok());
        }
    }

    pub fn run() {
        console_error_panic_hook::set_once();
        let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        // The worker lives as long as its global scope: leak it.
        let instance: &'static Worker = Box::leak(Box::new(Worker {
            scope: scope.clone(),
            driver: RefCell::new(Driver::new()),
            timer: Cell::new(None),
            on_timer: Closure::new(|| {
                worker().timer.set(None);
                worker().pump();
            }),
        }));
        WORKER.with(|w| *w.borrow_mut() = Some(instance));

        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(|event: MessageEvent| worker().on_message(&event));
        scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        instance.post_event(&WorkerEvent::Ready);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    glue::run();
}

/// The worker only exists in the browser.
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("splat-worker is a Web Worker: build it for wasm32-unknown-unknown (trunk build in crates/gs-app).");
}
