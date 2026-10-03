//! Timing probe for the x11 output driver: types the text given on the command
//! line into a window this process owns, resolving the key events the way a
//! client does (cached keyboard mapping, refreshed on MappingNotify), then
//! prints what arrived and how long the driver took.
//!
//! Needs a display with XTEST. Headless:
//!
//! ```text
//! Xvfb :99 -screen 0 1024x768x24 &
//! DISPLAY=:99 cargo run --example x11_type_probe -- "今天天气很好"
//! ```
//!
//! On this machine a 17-distinct-character Chinese sentence reports one
//! mapping change and lands in about 60 ms; the per-character rebinding it
//! replaced reported 17 changes and about 625 ms.

use std::time::{Duration, Instant};

use voxtype::output::x11::X11Output;
use voxtype::output::TextOutput;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt, CreateWindowAux, EventMask, InputFocus, KeyButMask, WindowClass,
};
use x11rb::protocol::Event;

fn char_for_keysym(keysym: u32) -> Option<char> {
    match keysym {
        0 => None,
        0xff0d => Some('\n'),
        0xff09 => Some('\t'),
        0x20..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
        _ => None,
    }
}

/// A client's own copy of the keyboard mapping.
struct CachedMap {
    keysyms: Vec<u32>,
    per: usize,
    min: u8,
}

impl CachedMap {
    fn fetch(conn: &impl Connection) -> Self {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn
            .get_keyboard_mapping(min, max - min + 1)
            .unwrap()
            .reply()
            .unwrap();
        Self {
            keysyms: reply.keysyms,
            per: reply.keysyms_per_keycode as usize,
            min,
        }
    }

    fn char_for(&self, keycode: u8, shifted: bool) -> Option<char> {
        let level = usize::from(shifted);
        let index = (keycode.wrapping_sub(self.min) as usize) * self.per + level;
        self.keysyms.get(index).copied().and_then(char_for_keysym)
    }
}

fn main() {
    let mut into_focus = false;
    let mut text = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            // Type into whatever window already has the input focus, instead of
            // a window of our own: how a real client's reception gets checked.
            "--into-focus" => into_focus = true,
            other => text = Some(other.to_string()),
        }
    }
    let text = text.expect("usage: x11_type_probe [--into-focus] <text>");
    let settle_ms: u32 = std::env::var("X11_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);

    let (conn, screen_num) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[screen_num];

    let win = conn.generate_id().unwrap();
    if !into_focus {
        conn.create_window(
            0,
            win,
            screen.root,
            0,
            0,
            400,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS),
        )
        .unwrap();
        conn.map_window(win).unwrap();
        conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .unwrap();
        conn.flush().unwrap();
        let focus = conn.get_input_focus().unwrap().reply().unwrap().focus;
        assert_eq!(focus, win, "another client holds the input focus");
    }

    let expected = text.chars().count();
    let payload = text.clone();
    let typing = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let driver = X11Output::new(0, 0, settle_ms, false, None);
        let started = Instant::now();
        runtime
            .block_on(async { driver.output(&payload).await })
            .expect("typing failed");
        started.elapsed()
    });

    let mut map = CachedMap::fetch(&conn);
    let mut received = String::new();
    let mut changes = 0usize;
    let mut presses = 0usize;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && (into_focus || received.chars().count() < expected) {
        while let Some(event) = conn.poll_for_event().unwrap() {
            match event {
                Event::MappingNotify(_) => {
                    // A client refreshes its copy only when told to.
                    map = CachedMap::fetch(&conn);
                    changes += 1;
                }
                Event::KeyPress(event) => {
                    presses += 1;
                    if let Some(ch) =
                        map.char_for(event.detail, event.state.contains(KeyButMask::SHIFT))
                    {
                        received.push(ch);
                    }
                }
                _ => {}
            }
        }
        if into_focus && typing.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }

    let typing_time = typing.join().unwrap();
    println!(
        "typed {expected} chars in {typing_time:?} (settle {settle_ms} ms): {changes} mapping \
         change(s), {presses} press(es), {} char(s) resolved",
        received.chars().count()
    );
    if !into_focus {
        println!("received: {received}");
        assert_eq!(received, text, "client did not resolve every character");
    }
}
